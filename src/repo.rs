//! Repository index creation and validation.
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use crate::archive;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoEntry {
    pub name: String,
    pub version: String,
    pub rel: u32,
    pub arch: String,
    pub file: String,
    pub sha256: String,
    /// Absolute HTTPS URL for distributed repositories (schema v3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub download_url: Option<String>,
    /// archive size in bytes (download size); 0 = unknown in legacy indexes
    #[serde(default, skip_serializing_if = "is_zero")]
    pub size: u64,
    /// installed size in bytes, from the archive's size field; 0 = unknown
    #[serde(default, skip_serializing_if = "is_zero")]
    pub installed_size: u64,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

fn default_archive_location() -> String {
    ARCHIVE_LOCATION_BASE_URL.to_string()
}

fn is_base_url(value: &String) -> bool {
    value == ARCHIVE_LOCATION_BASE_URL
}

/// Authentication metadata for a schema-v2 repository.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepositoryMeta {
    pub schema: u32,
    pub archive_auth: String,
    pub hash_algorithm: String,
    pub index_signature: String,
    /// How clients locate package and recipe archives.
    #[serde(default = "default_archive_location", skip_serializing_if = "is_base_url")]
    pub archive_location: String,
}

impl RepositoryMeta {
    pub fn v2() -> Self {
        Self {
            schema: INDEX_SCHEMA_V2,
            archive_auth: ARCHIVE_AUTH_SIGNED_INDEX.to_string(),
            hash_algorithm: HASH_ALGORITHM_SHA256.to_string(),
            index_signature: INDEX_SIGNATURE_NAWA_SIG_V1.to_string(),
            archive_location: ARCHIVE_LOCATION_BASE_URL.to_string(),
        }
    }

    pub fn v3_per_package_url() -> Self {
        Self {
            schema: INDEX_SCHEMA_V3,
            archive_auth: ARCHIVE_AUTH_SIGNED_INDEX.to_string(),
            hash_algorithm: HASH_ALGORITHM_SHA256.to_string(),
            index_signature: INDEX_SIGNATURE_NAWA_SIG_V1.to_string(),
            archive_location: ARCHIVE_LOCATION_PER_PACKAGE_URL.to_string(),
        }
    }
}

pub const INDEX_SCHEMA_V2: u32 = 2;
pub const INDEX_SCHEMA_V3: u32 = 3;
pub const ARCHIVE_LOCATION_BASE_URL: &str = "base-url";
pub const ARCHIVE_LOCATION_PER_PACKAGE_URL: &str = "per-package-url";
pub const ARCHIVE_AUTH_SIGNED_INDEX: &str = "signed-index";
pub const HASH_ALGORITHM_SHA256: &str = "sha256";
pub const INDEX_SIGNATURE_NAWA_SIG_V1: &str = "nawa-sig-v1";

/// The recipe tree published next to the packages (nawa sync). It lives in
/// the signed index, so the signature covers the revision and the tarball hash.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecipesEntry {
    pub rev: String,
    pub file: String,
    pub sha256: String,
    /// Absolute HTTPS URL for distributed recipe storage (schema v3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub download_url: Option<String>,
}

pub const RECIPES_SUFFIX: &str = ".recipes.tar.zst";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Index {
    /// Unix time the index was generated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generated: Option<u64>,
    /// Unix time after which clients must not trust this index.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires: Option<u64>,
    /// Present only for schema-v2 repositories. Absence means legacy v1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<RepositoryMeta>,
    /// Recipe tree for nawa sync.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recipes: Option<RecipesEntry>,
    #[serde(rename = "package")]
    pub packages: Vec<RepoEntry>,
}

pub const INDEX_VALIDITY_SECS: u64 = 90 * 24 * 3600;

/// Load and validate an index. An index without [repository] remains legacy v1.
#[cfg(test)]
pub fn load_index(path: &Path) -> Result<Index> {
    let raw = std::fs::read(path)
        .with_context(|| format!("read index {}", path.display()))?;
    load_index_bytes(&raw).with_context(|| format!("parse index {}", path.display()))
}

/// Parse and validate exactly the byte sequence whose signature was checked.
pub fn load_index_bytes(raw: &[u8]) -> Result<Index> {
    let text = std::str::from_utf8(raw).context("repository index is not valid UTF-8")?;
    let idx: Index = toml::from_str(text).context("parse repository index TOML")?;
    idx.validate()?;
    Ok(idx)
}

impl Index {
    pub fn find(&self, name: &str) -> Option<&RepoEntry> {
        self.packages.iter().find(|p| p.name == name)
    }

    /// Validate schema and the stronger invariants required by v2.
    pub fn validate(&self) -> Result<()> {
        let meta = match &self.repository {
            None => return Ok(()), // v1: retain the historical parser/policy
            Some(meta) => meta,
        };
        if !matches!(meta.schema, INDEX_SCHEMA_V2 | INDEX_SCHEMA_V3) {
            bail!("unsupported repository index schema {}; this Nawa supports schemas 2 and 3", meta.schema);
        }
        let per_package_url = meta.schema == INDEX_SCHEMA_V3;
        let expected_location = if per_package_url {
            ARCHIVE_LOCATION_PER_PACKAGE_URL
        } else {
            ARCHIVE_LOCATION_BASE_URL
        };
        if meta.archive_location != expected_location {
            bail!(
                "schema-v{} archive_location {:?} is unsupported; expected {:?}",
                meta.schema, meta.archive_location, expected_location
            );
        }
        if meta.archive_auth != ARCHIVE_AUTH_SIGNED_INDEX {
            bail!("unsupported repository archive_auth {:?}; expected {:?}", meta.archive_auth, ARCHIVE_AUTH_SIGNED_INDEX);
        }
        if meta.hash_algorithm != HASH_ALGORITHM_SHA256 {
            bail!("unsupported repository hash_algorithm {:?}; expected sha256", meta.hash_algorithm);
        }
        if meta.index_signature != INDEX_SIGNATURE_NAWA_SIG_V1 {
            bail!("unsupported repository index_signature {:?}; expected nawa-sig-v1", meta.index_signature);
        }

        let generated = self.generated.context("schema-v2 index is missing generated")?;
        let expires = self.expires.context("schema-v2 index is missing expires")?;
        if generated == 0 || expires <= generated {
            bail!("schema-v2 index has invalid generated/expires timestamps");
        }

        let mut names = HashSet::new();
        let mut files = HashSet::new();
        for p in &self.packages {
            if p.name.trim().is_empty() || p.version.trim().is_empty() || p.arch.trim().is_empty() {
                bail!("schema-v2/v3 package entry has an empty name, version, or arch");
            }
            if p.rel == 0 {
                bail!("schema-v2 package {} has invalid rel 0", p.name);
            }
            validate_archive_filename(&p.file)?;
            validate_sha256(&p.sha256, &format!("package {}", p.name))?;
            if p.size == 0 {
                bail!("schema-v{} package {} has no archive size", meta.schema, p.name);
            }
            if per_package_url {
                let url = p.download_url.as_deref().with_context(|| {
                    format!("schema-v3 package {} has no download_url", p.name)
                })?;
                validate_download_url(url, &format!("package {}", p.name))?;
            } else if p.download_url.is_some() {
                bail!("schema-v2 package {} unexpectedly has download_url", p.name);
            }
            if !names.insert(p.name.as_str()) {
                bail!("schema-v2/v3 index contains duplicate package name {:?}", p.name);
            }
            if !files.insert(p.file.as_str()) {
                bail!("schema-v2/v3 index contains duplicate archive filename {:?}", p.file);
            }
        }
        if let Some(r) = &self.recipes {
            validate_plain_filename(&r.file)?;
            if !r.file.ends_with(RECIPES_SUFFIX) {
                bail!("schema-v2/v3 recipe archive has an unexpected filename {:?}", r.file);
            }
            validate_sha256(&r.sha256, "recipes archive")?;
            if per_package_url {
                let url = r.download_url.as_deref().context("schema-v3 recipes entry has no download_url")?;
                validate_download_url(url, "recipes archive")?;
            } else if r.download_url.is_some() {
                bail!("schema-v2/v3 recipes entry unexpectedly has download_url");
            }
            validate_rev(&r.rev)?;
        }
        Ok(())
    }
}

fn validate_download_url(value: &str, what: &str) -> Result<()> {
    let parsed = url::Url::parse(value)
        .with_context(|| format!("{what} has an invalid download_url"))?;
    if parsed.scheme() != "https"
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.fragment().is_some()
    {
        bail!("{what} download_url must be an absolute HTTPS URL without credentials or fragment");
    }
    Ok(())
}

pub fn validate_sha256(value: &str, what: &str) -> Result<()> {
    if value.len() != 64 || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("{what} has an invalid SHA-256 digest (expected 64 hexadecimal characters)");
    }
    Ok(())
}

fn validate_plain_filename(file: &str) -> Result<()> {
    if file.is_empty()
        || file.starts_with('.')
        || file.contains('/')
        || file.contains('\\')
        || file.contains("..")
    {
        bail!("index entry has an unsafe filename: {file:?}");
    }
    Ok(())
}

fn validate_archive_filename(file: &str) -> Result<()> {
    validate_plain_filename(file)?;
    if !file.ends_with(".nawa.tar.zst") {
        bail!("schema-v2 package archive has an unexpected filename: {file:?}");
    }
    Ok(())
}

/// Build a legacy v1 index from a directory containing package archives.
#[cfg(test)]
pub fn create_index(dir: &Path, out: &Path) -> Result<Index> {
    create_index_with(dir, out, None)
}

/// Backward-compatible v1 index generator used by repository unit tests.
#[cfg(test)]
pub fn create_index_with(
    dir: &Path,
    out: &Path,
    recipes: Option<(&Path, &str)>,
) -> Result<Index> {
    create_index_with_version(dir, out, recipes, 1)
}

/// Build a repository index. Version 1 remains the compatibility default;
/// v2 uses signed-index hashes with base-URL archive lookup, while v3 adds
/// authenticated per-archive HTTPS locations for distributed storage.
pub fn create_index_with_version(
    dir: &Path,
    out: &Path,
    recipes: Option<(&Path, &str)>,
    index_version: u32,
) -> Result<Index> {
    create_index_with_locations(dir, out, recipes, index_version, None)
}

/// Build an index using a publisher-generated archive location manifest.
pub fn create_index_with_locations(
    dir: &Path,
    out: &Path,
    recipes: Option<(&Path, &str)>,
    index_version: u32,
    locations_path: Option<&Path>,
) -> Result<Index> {
    if !matches!(index_version, 1 | INDEX_SCHEMA_V2 | INDEX_SCHEMA_V3) {
        bail!("unsupported repository index version {index_version} (supported: 1, 2, 3)");
    }
    if index_version == INDEX_SCHEMA_V3 && locations_path.is_none() {
        bail!("schema-v3 requires --locations with package_urls and recipe_urls");
    }
    if index_version != INDEX_SCHEMA_V3 && locations_path.is_some() {
        bail!("--locations is only valid with --index-version 3");
    }
    let locations = match locations_path {
        Some(path) => Some(load_locations(path)?),
        None => None,
    };
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
        println!("nawa: index -> {}-{}-{}", name, version, rel);
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
        packages: newest.into_values().collect(),
    };
    index.validate()?;
    write_index_atomic(&index, out)?;
    println!("nawa: wrote index {} ({} packages, schema v{})", out.display(), index.packages.len(), index_version);
    Ok(index)
}

/// Serialise an index and activate it with a temporary file plus rename, so a
/// reader never sees a half-written index.
fn write_index_atomic(index: &Index, out: &Path) -> Result<()> {
    let raw = toml::to_string_pretty(index)?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp = out.with_file_name(format!(
        ".{}.tmp-{}-{stamp}",
        out.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id()
    ));
    let write_result = (|| -> Result<()> {
        std::fs::write(&tmp, raw.as_bytes())
            .with_context(|| format!("write temporary index {}", tmp.display()))?;
        std::fs::rename(&tmp, out)
            .with_context(|| format!("activate index {}", out.display()))?;
        Ok(())
    })();
    if write_result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    write_result
}

/// What changed since a base index (see `update_index`).
#[derive(Debug, Default)]
pub struct IndexDelta {
    /// Entries for new archives, one per archive, in any order.
    pub add: Vec<RepoEntry>,
    /// Archive file names that left the repository. An unindexed name (an old
    /// archive that was never the newest) is a no-op.
    pub remove_files: Vec<String>,
    /// Package names whose entry disappears on purpose (every archive of the
    /// package is gone).
    pub drop_packages: Vec<String>,
}

fn archive_file_name(e: &RepoEntry) -> String {
    format!("{}-{}-{}.nawa.tar.zst", e.name, e.version, e.rel)
}

/// Apply a delta to a schema-2/3 base index WITHOUT opening any archive.
///
/// The caller must already have verified the base index signature. For every
/// delta this equals `repo-create` over (archives of the base − removed + added)
/// as long as removals follow the contract below, which is why a removal that
/// would silently lose or expose a package is an error:
///
/// * removing the indexed archive of a package needs an added entry for that
///   package (the replacement, possibly an older archive that becomes the
///   newest) or an explicit `drop_packages` entry;
/// * the newest archive per package name wins (`vercmp` of version-rel), exactly
///   like repo-create; an older added entry is ignored;
/// * an archive file name is immutable: re-adding the same name with another
///   sha256 or size is an error, re-adding it unchanged is a no-op.
pub fn update_index(
    base: &Index,
    delta: IndexDelta,
    recipes: Option<RecipesEntry>,
    now: u64,
) -> Result<Index> {
    let meta = base.repository.clone().context(
        "repo-update needs a schema-2 or schema-3 base index; a v1 index has no signed hash policy (use repo-create)",
    )?;
    let mut map: BTreeMap<String, RepoEntry> = BTreeMap::new();
    for p in &base.packages {
        if map.insert(p.name.clone(), p.clone()).is_some() {
            bail!("base index contains duplicate package name {:?}", p.name);
        }
    }

    let mut removed_names: HashSet<String> = HashSet::new();
    for file in &delta.remove_files {
        validate_plain_filename(file)?;
        if let Some(name) = map.values().find(|p| &p.file == file).map(|p| p.name.clone()) {
            map.remove(&name);
            removed_names.insert(name);
        }
    }
    let mut dropped: HashSet<String> = HashSet::new();
    for name in &delta.drop_packages {
        if map.remove(name).is_none() && !removed_names.contains(name) {
            bail!("cannot drop {name:?}: it is not in the base index");
        }
        dropped.insert(name.clone());
    }

    for e in delta.add {
        let expected = archive_file_name(&e);
        if e.file != expected {
            bail!("entry for {} has file {:?}, expected {:?}", e.name, e.file, expected);
        }
        match map.get(&e.name) {
            Some(cur) if cur.file == e.file => {
                if cur.sha256 != e.sha256 || cur.size != e.size {
                    bail!("{} is already indexed with a different sha256/size; archive files are immutable", e.file);
                }
            }
            Some(cur) => {
                let new_full = format!("{}-{}", e.version, e.rel);
                let cur_full = format!("{}-{}", cur.version, cur.rel);
                if crate::version::vercmp(&new_full, &cur_full) == std::cmp::Ordering::Greater {
                    map.insert(e.name.clone(), e);
                }
            }
            None => {
                map.insert(e.name.clone(), e);
            }
        }
    }

    for name in &removed_names {
        if !dropped.contains(name) && !map.contains_key(name) {
            bail!(
                "removing the indexed archive of {name} would leave it without an entry; \
                 add the archive that replaces it, or drop the package explicitly"
            );
        }
    }

    let index = Index {
        generated: Some(now),
        expires: Some(now + INDEX_VALIDITY_SECS),
        repository: Some(meta),
        recipes: recipes.or_else(|| base.recipes.clone()),
        packages: map.into_values().collect(),
    };
    index.validate()?;
    Ok(index)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EntriesFile {
    #[serde(rename = "package", default)]
    packages: Vec<RepoEntry>,
}

/// Optional inputs for the file-level `repo-update` flow, mirroring the CLI
/// flags: entries to add, archives/packages to remove, an archive-locations
/// manifest, and a new recipe snapshot.
#[derive(Debug, Default)]
pub struct UpdateIndexFilesInputs<'a> {
    pub entries_path: Option<&'a Path>,
    pub remove_files: Vec<String>,
    pub drop_packages: Vec<String>,
    pub locations_path: Option<&'a Path>,
    pub recipes: Option<(&'a Path, &'a str)>,
}

/// File-level `repo-update`: read the delta inputs, apply `update_index`, and
/// write the new index atomically. `dir` receives the recipe snapshot when
/// `inputs.recipes` is given (as in repo-create); without it the base recipes
/// entry is kept unchanged. Signing is the caller's job.
pub fn update_index_files(
    base: &Index,
    dir: &Path,
    out: &Path,
    inputs: UpdateIndexFilesInputs<'_>,
) -> Result<Index> {
    let UpdateIndexFilesInputs {
        entries_path,
        remove_files,
        drop_packages,
        locations_path,
        recipes,
    } = inputs;
    let schema = base.repository.as_ref().map(|m| m.schema).unwrap_or(1);
    if schema != INDEX_SCHEMA_V3 && locations_path.is_some() {
        bail!("--locations is only valid with a schema-3 base index");
    }
    let locations = match locations_path {
        Some(path) => Some(load_locations(path)?),
        None => None,
    };
    let mut add = match entries_path {
        Some(path) => {
            let raw = std::fs::read_to_string(path)
                .with_context(|| format!("read entries {}", path.display()))?;
            let parsed: EntriesFile = toml::from_str(&raw)
                .with_context(|| format!("parse entries {}", path.display()))?;
            parsed.packages
        }
        None => Vec::new(),
    };
    if let Some(loc) = &locations {
        for e in &mut add {
            if e.download_url.is_none() {
                e.download_url = loc.package_urls.get(&e.file).cloned();
            }
        }
    }
    let mut recipes_entry = match recipes {
        Some((tree, rev)) => Some(pack_recipes(tree, dir, rev)?),
        None => None,
    };
    if let (Some(entry), Some(loc)) = (&mut recipes_entry, &locations) {
        entry.download_url = loc.recipe_urls.get(&entry.file).cloned();
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let index = update_index(
        base,
        IndexDelta { add, remove_files, drop_packages },
        recipes_entry,
        now,
    )?;
    write_index_atomic(&index, out)?;
    println!("nawa: wrote index {} ({} packages, schema v{})", out.display(), index.packages.len(), schema);
    Ok(index)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LocationsFile {
    schema: u32,
    #[serde(default)]
    package_urls: BTreeMap<String, String>,
    #[serde(default)]
    recipe_urls: BTreeMap<String, String>,
}

fn load_locations(path: &Path) -> Result<LocationsFile> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("read archive locations {}", path.display()))?;
    let locations: LocationsFile = toml::from_str(&raw)
        .with_context(|| format!("parse archive locations {}", path.display()))?;
    if locations.schema != 1 {
        bail!("unsupported archive locations schema {}; expected 1", locations.schema);
    }
    for (file, url) in locations.package_urls.iter().chain(locations.recipe_urls.iter()) {
        validate_plain_filename(file)?;
        validate_download_url(url, &format!("archive {file}"))?;
    }
    Ok(locations)
}

/// A revision is a commit sha or tag: it ends up in a file name, so keep it
/// to a conservative alphabet.
pub fn validate_rev(rev: &str) -> Result<()> {
    let ok = !rev.is_empty()
        && rev.len() <= 64
        && !rev.starts_with('.')
        && rev
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if !ok {
        anyhow::bail!("invalid recipes revision `{rev}` (use a commit sha or tag: [A-Za-z0-9._-], max 64)");
    }
    Ok(())
}

/// Pack the recipe tree into recipes-<rev>.recipes.tar.zst and describe it.
/// .git is skipped; symlinks and special files are refused — a recipe tree is plain data.
pub fn pack_recipes(tree: &Path, out_dir: &Path, rev: &str) -> Result<RecipesEntry> {
    validate_rev(rev)?;
    if !tree.is_dir() {
        anyhow::bail!("recipes directory not found: {}", tree.display());
    }
    let file = format!("recipes-{rev}{RECIPES_SUFFIX}");
    let dest = out_dir.join(&file);
    let part = out_dir.join(format!(".{file}.part"));
    let f = std::fs::File::create(&part).with_context(|| format!("create {}", part.display()))?;
    let enc = zstd::Encoder::new(f, 19)?;
    let mut b = tar::Builder::new(enc);
    b.mode(tar::HeaderMode::Deterministic);
    b.follow_symlinks(false);
    let mut count = 0usize;
    for entry in walkdir::WalkDir::new(tree)
        .min_depth(1)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|e| e.file_name() != ".git")
    {
        let entry = entry?;
        let rel = entry.path().strip_prefix(tree)?;
        let ft = entry.file_type();
        if ft.is_dir() {
            b.append_dir(rel, entry.path())?;
        } else if ft.is_file() {
            b.append_path_with_name(entry.path(), rel)?;
            if entry.file_name() == "recipe.toml" {
                count += 1;
            }
        } else {
            anyhow::bail!("{}: symlinks and special files are not allowed in a recipe tree", entry.path().display());
        }
    }
    if count == 0 {
        anyhow::bail!("{} contains no recipe.toml — not a recipe tree", tree.display());
    }
    b.into_inner()?.finish()?;
    std::fs::rename(&part, &dest).with_context(|| format!("write {}", dest.display()))?;
    let sha256 = crate::fetch::sha256_of_file(&dest)?;
    println!("nawa: index -> recipes {rev} ({count} recipes)");
    Ok(RecipesEntry { rev: rev.to_string(), file, sha256, download_url: None })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive;

    #[test]
    fn index_keeps_only_the_newest_version_per_package() {
        let base = std::env::temp_dir().join(format!("nawa-repo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let dir = base.join("repo");
        std::fs::create_dir_all(&dir).unwrap();

        for (ver, rel) in [("1.0", 1), ("2.0", 1), ("1.5", 9)] {
            let st = base.join(format!("st-{ver}-{rel}"));
            std::fs::create_dir_all(&st).unwrap();
            std::fs::write(st.join("f"), b"x").unwrap();
            let pkginfo = format!("pkgname = demo\npkgver = {ver}\npkgrel = {rel}\narch = x86_64\nsize = 1\n");
            archive::create(&st, &pkginfo, &dir.join(format!("demo-{ver}-{rel}.nawa.tar.zst"))).unwrap();
        }

        let out = base.join("index.toml");
        let idx = create_index(&dir, &out).unwrap();
        assert_eq!(idx.packages.len(), 1);
        assert_eq!(idx.packages[0].version, "2.0");
        assert_eq!(idx.packages[0].rel, 1);
        assert_eq!(idx.find("demo").unwrap().version, "2.0");
        let e = idx.find("demo").unwrap();
        assert!(e.size > 0);
        assert_eq!(e.installed_size, 1);
        let old = "[[package]]\nname = \"a\"\nversion = \"1\"\nrel = 1\narch = \"x86_64\"\nfile = \"a.nawa.tar.zst\"\nsha256 = \"00\"\n";
        let parsed: Index = toml::from_str(old).unwrap();
        assert_eq!((parsed.packages[0].size, parsed.packages[0].installed_size), (0, 0));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn v2_index_has_explicit_archive_authentication_metadata() {
        let meta = RepositoryMeta::v2();
        let index = Index {
            generated: Some(10),
            expires: Some(20),
            repository: Some(meta),
            recipes: None,
            packages: vec![RepoEntry {
                name: "bash".into(),
                version: "5.2".into(),
                rel: 1,
                arch: "x86_64".into(),
                file: "bash-5.2-1.nawa.tar.zst".into(),
                sha256: "a".repeat(64),
                download_url: None,
                size: 123,
                installed_size: 456,
            }],
        };
        index.validate().unwrap();
        let serialized = toml::to_string(&index).unwrap();
        assert!(serialized.contains("schema = 2"));
        assert!(serialized.contains("archive_auth = \"signed-index\""));
        let parsed: Index = toml::from_str(&serialized).unwrap();
        parsed.validate().unwrap();
        assert_eq!(parsed.repository.unwrap().schema, 2);
    }

    #[test]
    fn v3_requires_authenticated_per_package_https_locations() {
        let mut index = Index {
            generated: Some(10),
            expires: Some(20),
            repository: Some(RepositoryMeta::v3_per_package_url()),
            recipes: None,
            packages: vec![RepoEntry {
                name: "bash".into(),
                version: "5.2".into(),
                rel: 1,
                arch: "x86_64".into(),
                file: "bash-5.2-1.nawa.tar.zst".into(),
                sha256: "a".repeat(64),
                download_url: Some("https://github.com/example/repo/releases/download/packages-0001/sha256-a.nawa.tar.zst".into()),
                size: 123,
                installed_size: 456,
            }],
        };
        index.validate().unwrap();

        index.packages[0].download_url = Some("http://github.com/example/repo/file.nawa.tar.zst".into());
        assert!(index.validate().unwrap_err().to_string().contains("HTTPS"));

        index.packages[0].download_url = Some("https://user:password@github.com/example/repo/file.nawa.tar.zst".into());
        assert!(index.validate().is_err());

        index.packages[0].download_url = None;
        assert!(index.validate().unwrap_err().to_string().contains("download_url"));
    }

    #[test]
    fn repo_create_v3_applies_package_locations_manifest() {
        let base = std::env::temp_dir().join(format!("nawa-repo-v3-locations-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let dir = base.join("repo");
        let staging = base.join("staging");
        let recipes = base.join("recipes");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::create_dir_all(&staging).unwrap();
        std::fs::create_dir_all(recipes.join("base/demo")).unwrap();
        std::fs::write(recipes.join("base/demo/recipe.toml"), "[package]\nname = \"demo\"\nversion = \"1.0\"\n").unwrap();
        std::fs::write(staging.join("bin"), b"bash").unwrap();
        archive::create(
            &staging,
            "pkgname = demo\npkgver = 1.0\npkgrel = 1\narch = x86_64\nsize = 4\n",
            &dir.join("demo-1.0-1.nawa.tar.zst"),
        ).unwrap();

        let locations = base.join("locations.toml");
        std::fs::write(
            &locations,
            r#"schema = 1

[package_urls]
"demo-1.0-1.nawa.tar.zst" = "https://github.com/example/repo/releases/download/packages-0001/sha256-demo.nawa.tar.zst"

[recipe_urls]
"recipes-abc123.recipes.tar.zst" = "https://github.com/example/repo/releases/download/packages-0001/sha256-recipes.tar.zst"
"#,
        ).unwrap();

        let index_path = dir.join("index.toml");
        let index = create_index_with_locations(
            &dir, &index_path, Some((&recipes, "abc123")), INDEX_SCHEMA_V3, Some(&locations)
        ).unwrap();
        assert_eq!(
            index.packages[0].download_url.as_deref(),
            Some("https://github.com/example/repo/releases/download/packages-0001/sha256-demo.nawa.tar.zst")
        );
        assert_eq!(
            index.recipes.as_ref().unwrap().download_url.as_deref(),
            Some("https://github.com/example/repo/releases/download/packages-0001/sha256-recipes.tar.zst")
        );
        let loaded = load_index(&index_path).unwrap();
        assert_eq!(
            loaded.repository.unwrap().archive_location,
            ARCHIVE_LOCATION_PER_PACKAGE_URL
        );
        assert_eq!(
            loaded.recipes.unwrap().download_url.as_deref(),
            Some("https://github.com/example/repo/releases/download/packages-0001/sha256-recipes.tar.zst")
        );
        assert!(create_index_with_version(&dir, &index_path, None, INDEX_SCHEMA_V3).is_err());

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn v2_index_defaults_to_base_url_without_changing_legacy_shape() {
        let meta = RepositoryMeta::v2();
        assert_eq!(meta.archive_location, ARCHIVE_LOCATION_BASE_URL);
        let serialized = toml::to_string(&meta).unwrap();
        assert!(!serialized.contains("archive_location"));
        let parsed: RepositoryMeta = toml::from_str(
            "schema = 2\narchive_auth = \"signed-index\"\nhash_algorithm = \"sha256\"\nindex_signature = \"nawa-sig-v1\"\n"
        ).unwrap();
        assert_eq!(parsed.archive_location, ARCHIVE_LOCATION_BASE_URL);
    }

    #[test]
    fn v2_rejects_unknown_authentication_and_malformed_entries() {
        let mut index = Index {
            generated: Some(10),
            expires: Some(20),
            repository: Some(RepositoryMeta::v2()),
            recipes: None,
            packages: vec![RepoEntry {
                name: "bash".into(),
                version: "5.2".into(),
                rel: 1,
                arch: "x86_64".into(),
                file: "bash-5.2-1.nawa.tar.zst".into(),
                sha256: "a".repeat(64),
                download_url: None,
                size: 123,
                installed_size: 456,
            }],
        };
        index.repository.as_mut().unwrap().archive_auth = "detached".into();
        assert!(index.validate().unwrap_err().to_string().contains("archive_auth"));
        index.repository.as_mut().unwrap().archive_auth = ARCHIVE_AUTH_SIGNED_INDEX.into();
        index.packages[0].sha256 = "xyz".into();
        assert!(index.validate().unwrap_err().to_string().contains("SHA-256"));
    }

    // ---- repo-update ------------------------------------------------------

    fn mk_archive(root: &Path, dir: &Path, name: &str, ver: &str, rel: u32) {
        let st = root.join(format!("st-{name}-{ver}-{rel}"));
        std::fs::create_dir_all(&st).unwrap();
        std::fs::write(st.join("f"), format!("{name}{ver}{rel}")).unwrap();
        let info = format!("pkgname = {name}\npkgver = {ver}\npkgrel = {rel}\narch = x86_64\nsize = 7\n");
        archive::create(&st, &info, &dir.join(format!("{name}-{ver}-{rel}.nawa.tar.zst"))).unwrap();
    }

    fn scratch(tag: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!("nawa-repo-update-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    /// Only the package set matters for equivalence, not generated/expires.
    fn shape(i: &Index) -> String {
        format!("{:?}", i.packages)
    }

    #[test]
    fn repo_update_equals_repo_create_over_the_same_archives() {
        let base = scratch("equiv");
        let a = base.join("a");
        std::fs::create_dir_all(&a).unwrap();
        for (n, v) in [("demo", "1.0"), ("keep", "1.0"), ("old", "1.0"), ("swap", "1.0")] {
            mk_archive(&base, &a, n, v, 1);
        }
        let base_idx = create_index_with_version(&a, &base.join("a.toml"), None, INDEX_SCHEMA_V2).unwrap();

        // the delta: new swap, new fresh, an OLDER demo (must be ignored), `old` gone
        let new = base.join("new");
        std::fs::create_dir_all(&new).unwrap();
        mk_archive(&base, &new, "swap", "2.0", 1);
        mk_archive(&base, &new, "fresh", "1.0", 1);
        mk_archive(&base, &new, "demo", "0.5", 1);
        let new_idx = create_index_with_version(&new, &base.join("new.toml"), None, INDEX_SCHEMA_V2).unwrap();

        let updated = update_index(
            &base_idx,
            IndexDelta {
                add: new_idx.packages.clone(),
                remove_files: vec!["old-1.0-1.nawa.tar.zst".into()],
                drop_packages: vec!["old".into()],
            },
            None,
            1_000,
        ).unwrap();

        // reference: one directory holding (A - old) + new, indexed from scratch
        let full = base.join("full");
        std::fs::create_dir_all(&full).unwrap();
        for f in std::fs::read_dir(&a).unwrap().chain(std::fs::read_dir(&new).unwrap()) {
            let f = f.unwrap().path();
            if f.extension().map(|x| x == "zst").unwrap_or(false)
                && !f.file_name().unwrap().to_string_lossy().starts_with("old-")
            {
                std::fs::copy(&f, full.join(f.file_name().unwrap())).unwrap();
            }
        }
        let reference = create_index_with_version(&full, &base.join("full.toml"), None, INDEX_SCHEMA_V2).unwrap();
        assert_eq!(shape(&updated), shape(&reference));
        assert_eq!(updated.find("swap").unwrap().version, "2.0");
        assert_eq!(updated.find("demo").unwrap().version, "1.0");
        assert!(updated.find("old").is_none());
        assert_eq!(updated.generated, Some(1_000));
        assert_eq!(updated.expires, Some(1_000 + INDEX_VALIDITY_SECS));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn repo_update_without_a_delta_only_refreshes_the_validity_window() {
        let base = scratch("refresh");
        let a = base.join("a");
        std::fs::create_dir_all(&a).unwrap();
        mk_archive(&base, &a, "demo", "1.0", 1);
        let idx = create_index_with_version(&a, &base.join("a.toml"), None, INDEX_SCHEMA_V2).unwrap();
        let later = idx.generated.unwrap() + 80 * 24 * 3600;
        let refreshed = update_index(&idx, IndexDelta::default(), None, later).unwrap();
        assert_eq!(shape(&refreshed), shape(&idx));
        assert_eq!(refreshed.expires, Some(later + INDEX_VALIDITY_SECS));
        assert!(refreshed.expires > idx.expires);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn repo_update_readding_an_entry_is_a_no_op_and_changing_it_is_an_error() {
        let base = scratch("immutable");
        let a = base.join("a");
        std::fs::create_dir_all(&a).unwrap();
        mk_archive(&base, &a, "demo", "1.0", 1);
        let idx = create_index_with_version(&a, &base.join("a.toml"), None, INDEX_SCHEMA_V2).unwrap();
        let same = update_index(
            &idx,
            IndexDelta { add: idx.packages.clone(), ..Default::default() },
            None, 5,
        ).unwrap();
        assert_eq!(shape(&same), shape(&idx));

        let mut changed = idx.packages.clone();
        changed[0].sha256 = "b".repeat(64);
        let err = update_index(&idx, IndexDelta { add: changed, ..Default::default() }, None, 5).unwrap_err();
        assert!(err.to_string().contains("immutable"), "{err}");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn repo_update_refuses_removals_that_would_lose_a_package() {
        let base = scratch("removal");
        let a = base.join("a");
        std::fs::create_dir_all(&a).unwrap();
        mk_archive(&base, &a, "demo", "1.0", 1);
        let idx = create_index_with_version(&a, &base.join("a.toml"), None, INDEX_SCHEMA_V2).unwrap();

        let lose = IndexDelta { remove_files: vec!["demo-1.0-1.nawa.tar.zst".into()], ..Default::default() };
        let err = update_index(&idx, lose, None, 5).unwrap_err();
        assert!(err.to_string().contains("without an entry"), "{err}");

        // explicit drop is fine; so is removing an archive that was never indexed
        let drop = IndexDelta {
            remove_files: vec!["demo-1.0-1.nawa.tar.zst".into()],
            drop_packages: vec!["demo".into()],
            ..Default::default()
        };
        assert!(update_index(&idx, drop, None, 5).unwrap().packages.is_empty());
        let noop = IndexDelta { remove_files: vec!["demo-0.1-1.nawa.tar.zst".into()], ..Default::default() };
        assert_eq!(shape(&update_index(&idx, noop, None, 5).unwrap()), shape(&idx));

        let typo = IndexDelta { drop_packages: vec!["nope".into()], ..Default::default() };
        assert!(update_index(&idx, typo, None, 5).unwrap_err().to_string().contains("not in the base index"));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn repo_update_removal_can_expose_an_older_archive_when_it_is_added() {
        let base = scratch("expose");
        let a = base.join("a");
        std::fs::create_dir_all(&a).unwrap();
        mk_archive(&base, &a, "demo", "1.0", 1);
        mk_archive(&base, &a, "demo", "2.0", 1);
        let idx = create_index_with_version(&a, &base.join("a.toml"), None, INDEX_SCHEMA_V2).unwrap();
        assert_eq!(idx.find("demo").unwrap().version, "2.0");

        // roll back: 2.0 leaves, the caller supplies the 1.0 entry to take over
        let only_old = base.join("only-old");
        std::fs::create_dir_all(&only_old).unwrap();
        std::fs::copy(a.join("demo-1.0-1.nawa.tar.zst"), only_old.join("demo-1.0-1.nawa.tar.zst")).unwrap();
        let old_entry = create_index_with_version(&only_old, &base.join("o.toml"), None, INDEX_SCHEMA_V2).unwrap();
        let rolled = update_index(
            &idx,
            IndexDelta {
                add: old_entry.packages,
                remove_files: vec!["demo-2.0-1.nawa.tar.zst".into()],
                ..Default::default()
            },
            None, 5,
        ).unwrap();
        assert_eq!(rolled.find("demo").unwrap().version, "1.0");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn repo_update_rejects_v1_bases_and_mismatched_file_names() {
        let base = scratch("reject");
        let a = base.join("a");
        std::fs::create_dir_all(&a).unwrap();
        mk_archive(&base, &a, "demo", "1.0", 1);
        let v1 = create_index(&a, &base.join("v1.toml")).unwrap();
        assert!(update_index(&v1, IndexDelta::default(), None, 5).unwrap_err().to_string().contains("schema-2 or schema-3"));

        let v2 = create_index_with_version(&a, &base.join("v2.toml"), None, INDEX_SCHEMA_V2).unwrap();
        let mut bad = v2.packages[0].clone();
        bad.name = "other".into();
        let err = update_index(&v2, IndexDelta { add: vec![bad], ..Default::default() }, None, 5).unwrap_err();
        assert!(err.to_string().contains("expected"), "{err}");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn repo_update_files_fill_v3_urls_from_locations_and_keep_recipes() {
        let base = scratch("files");
        let a = base.join("a");
        std::fs::create_dir_all(&a).unwrap();
        mk_archive(&base, &a, "demo", "1.0", 1);
        let loc = base.join("loc.toml");
        std::fs::write(&loc, "schema = 1\n[package_urls]\n\"demo-1.0-1.nawa.tar.zst\" = \"https://github.com/e/r/releases/download/packages-0001/demo.nawa.tar.zst\"\n\"extra-1.0-1.nawa.tar.zst\" = \"https://github.com/e/r/releases/download/packages-0001/extra.nawa.tar.zst\"\n").unwrap();
        let idx = create_index_with_locations(&a, &base.join("a.toml"), None, INDEX_SCHEMA_V3, Some(&loc)).unwrap();

        let new = base.join("new");
        std::fs::create_dir_all(&new).unwrap();
        mk_archive(&base, &new, "extra", "1.0", 1);
        let new_idx = create_index_with_locations(&new, &base.join("n.toml"), None, INDEX_SCHEMA_V3, Some(&loc)).unwrap();
        // entries without download_url: the manifest must supply it
        let mut entry = new_idx.packages[0].clone();
        entry.download_url = None;
        let entries = base.join("entries.toml");
        std::fs::write(&entries, toml::to_string(&Index {
            generated: None, expires: None, repository: None, recipes: None, packages: vec![entry],
        }).unwrap()).unwrap();

        let out = base.join("out.toml");
        let updated = update_index_files(
            &idx,
            &base,
            &out,
            UpdateIndexFilesInputs {
                entries_path: Some(&entries),
                locations_path: Some(&loc),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(updated.packages.len(), 2);
        assert_eq!(
            updated.find("extra").unwrap().download_url.as_deref(),
            Some("https://github.com/e/r/releases/download/packages-0001/extra.nawa.tar.zst")
        );
        load_index(&out).unwrap();

        // no manifest and no URL in the entry: schema-3 validation refuses
        assert!(update_index_files(
            &idx,
            &base,
            &out,
            UpdateIndexFilesInputs {
                entries_path: Some(&entries),
                ..Default::default()
            },
        )
        .is_err());
        let _ = std::fs::remove_dir_all(&base);
    }
}
