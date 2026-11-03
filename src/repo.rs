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
    let raw = toml::to_string_pretty(&index)?;
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
    write_result?;
    println!("nawa: wrote index {} ({} packages, schema v{})", out.display(), index.packages.len(), index_version);
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
}
