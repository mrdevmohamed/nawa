//! Index creation.

use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::model::*;
use super::update::load_locations;
use super::pack::{pack_groups, pack_recipes};
use crate::archive;

/// Build a legacy v1 index from a directory containing package archives.
#[cfg(test)]
pub(crate) fn create_index(dir: &Path, out: &Path) -> Result<Index> {
    create_index_with(dir, out, None)
}

/// Backward-compatible v1 index generator used by repository unit tests.
#[cfg(test)]
pub(crate) fn create_index_with(
    dir: &Path,
    out: &Path,
    recipes: Option<(&Path, &str)>,
) -> Result<Index> {
    create_index_with_version(dir, out, recipes, 1)
}

/// Build a repository index. Version 1 remains the compatibility default;
/// v2 uses signed-index hashes with base-URL archive lookup, while v3 adds
/// authenticated per-archive HTTPS locations for distributed storage.
#[allow(dead_code)] // compatibility entry point; production CLI uses the groups-aware variant
pub(crate) fn create_index_with_version(
    dir: &Path,
    out: &Path,
    recipes: Option<(&Path, &str)>,
    index_version: u32,
) -> Result<Index> {
    create_index_with_locations(dir, out, recipes, index_version, None)
}

/// Build an index using a publisher-generated archive location manifest.
#[allow(dead_code)] // compatibility entry point; production CLI uses the groups-aware variant
pub(crate) fn create_index_with_locations(
    dir: &Path,
    out: &Path,
    recipes: Option<(&Path, &str)>,
    index_version: u32,
    locations_path: Option<&Path>,
) -> Result<Index> {
    create_index_with_locations_and_groups(dir, out, recipes, index_version, locations_path, None)
}

/// Build an index and optionally publish a validated group catalog beside it.
/// The catalog digest and size are included in the signed index.
pub(crate) fn create_index_with_locations_and_groups(
    dir: &Path,
    out: &Path,
    recipes: Option<(&Path, &str)>,
    index_version: u32,
    locations_path: Option<&Path>,
    groups_path: Option<&Path>,
) -> Result<Index> {
    if !matches!(index_version, 1 | INDEX_SCHEMA_V2 | INDEX_SCHEMA_V3) {
        bail!("unsupported repository index version {index_version} (supported: 1, 2, 3)");
    }
    if index_version == INDEX_SCHEMA_V3 && locations_path.is_none() {
        bail!("schema-v3 requires --locations with package_urls and recipe_urls");
    }
    if groups_path.is_some() && index_version < INDEX_SCHEMA_V2 {
        bail!("--groups requires a signed schema-v2/v3 repository index");
    }
    if index_version != INDEX_SCHEMA_V3 && locations_path.is_some() {
        bail!("--locations is only valid with --index-version 3");
    }
    let locations = match locations_path {
        Some(path) => Some(load_locations(path)?),
        None => None,
    };
    let groups_entry = groups_path.map(|path| pack_groups(path, dir)).transpose()?;
    let mut recipes_entry = match recipes {
        Some((tree, rev)) => Some(pack_recipes(tree, dir, rev)?),
        None => None,
    };
    if let (Some(entry), Some(loc)) = (&mut recipes_entry, &locations) {
        entry.download_url = loc.recipe_urls.get(&entry.file).cloned();
    }
    let mut packages = Vec::new();
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .with_context(|| format!("read {}", dir.display()))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().map(|x| x == "zst").unwrap_or(false))
        .filter(|p| {
            !p.file_name()
                .map(|n| n.to_string_lossy().ends_with(RECIPES_SUFFIX))
                .unwrap_or(false)
        })
        .collect();
    entries.sort();

    for path in entries {
        let raw = archive::read_pkginfo(&path)?;
        let info = archive::parse_pkginfo(&raw)?;
        let name = info.get("pkgname").cloned().unwrap_or_default();
        let version = info.get("pkgver").cloned().unwrap_or_default();
        let rel: u32 = if index_version >= INDEX_SCHEMA_V2 {
            let raw = info.get("pkgrel").with_context(|| {
                format!("{} is missing pkgrel in .PKGINFO", path.display())
            })?;
            let parsed = raw.parse::<u32>().with_context(|| {
                format!("{} has an invalid pkgrel {:?} in .PKGINFO", path.display(), raw)
            })?;
            if parsed == 0 {
                bail!("{} has invalid pkgrel 0 in .PKGINFO", path.display());
            }
            parsed
        } else {
            info.get("pkgrel").and_then(|v| v.parse().ok()).unwrap_or(1)
        };
        let arch = info.get("arch").cloned().unwrap_or_default();
        let sha256 = crate::fetch::sha256_of_file(&path)?;
        let size = std::fs::metadata(&path)
            .with_context(|| format!("stat package archive {}", path.display()))?
            .len();
        let installed_size: u64 = info.get("size").and_then(|v| v.parse().ok()).unwrap_or(0);
        crate::nawa_success!("index -> {}-{}-{}", name, version, rel);
        let file = path.file_name().unwrap().to_string_lossy().to_string();
        let download_url = locations.as_ref().and_then(|loc| loc.package_urls.get(&file).cloned());
        packages.push(RepoEntry {
            name,
            version,
            rel,
            arch,
            file,
            sha256,
            download_url,
            size,
            installed_size,
        });
    }

    // A repository exposes one entry per package — the newest archive.
    let mut newest: BTreeMap<String, RepoEntry> = BTreeMap::new();
    for p in packages {
        let full = format!("{}-{}", p.version, p.rel);
        match newest.get(&p.name) {
            Some(cur) => {
                let cur_full = format!("{}-{}", cur.version, cur.rel);
                if crate::version::vercmp(&full, &cur_full) == std::cmp::Ordering::Greater {
                    newest.insert(p.name.clone(), p);
                }
            }
            None => {
                newest.insert(p.name.clone(), p);
            }
        }
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let index = Index {
        generated: Some(now),
        expires: Some(now + INDEX_VALIDITY_SECS),
        repository: match index_version {
            1 => None,
            INDEX_SCHEMA_V2 => Some(RepositoryMeta::v2()),
            INDEX_SCHEMA_V3 => Some(RepositoryMeta::v3_per_package_url()),
            _ => unreachable!("index version validated above"),
        },
        recipes: recipes_entry,
        groups: groups_entry,
        packages: newest.into_values().collect(),
    };
    index.validate()?;
    write_index_atomic(&index, out)?;
    crate::nawa_success!("wrote index {} ({} packages, schema v{})", out.display(), index.packages.len(), index_version);
    Ok(index)
}

/// Serialise an index and activate it with a durable atomic write
/// (fsync file + parent dir), so a reader never sees a half-written index
/// and a crash cannot lose the directory entry.
pub(crate) fn write_index_atomic(index: &Index, out: &Path) -> Result<()> {
    let raw = toml::to_string_pretty(index)?;
    crate::transaction::atomic::write_atomic(out, raw.as_bytes(), 0o644)
        .with_context(|| format!("activate index {}", out.display()))
}

