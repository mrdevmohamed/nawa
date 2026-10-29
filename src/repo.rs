//! Repository index creation: scan a directory of archives and write
//! `index.toml` with name/version/path/sha256 for each package.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::archive;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoEntry {
    pub name: String,
    pub version: String,
    pub rel: u32,
    pub arch: String,
    pub file: String, // file name inside the repo dir
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Index {
    /// unix time the index was generated; part of the signed file, lets
    /// clients refuse an older index than one they have already seen
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generated: Option<u64>,
    /// unix time after which clients must not trust this index (freeze
    /// protection); `repo-create` sets it 90 days after `generated`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires: Option<u64>,
    #[serde(rename = "package")]
    pub packages: Vec<RepoEntry>,
}

/// How long a freshly created index stays valid (90 days).
pub const INDEX_VALIDITY_SECS: u64 = 90 * 24 * 3600;

/// Load a repository index from `index.toml`.
pub fn load_index(path: &Path) -> Result<Index> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("read index {}", path.display()))?;
    let idx: Index = toml::from_str(&raw)
        .with_context(|| format!("parse index {}", path.display()))?;
    Ok(idx)
}

impl Index {
    pub fn find(&self, name: &str) -> Option<&RepoEntry> {
        self.packages.iter().find(|p| p.name == name)
    }
}

/// Build an index from a directory containing `*.nawa.tar.zst` archives.
pub fn create_index(dir: &Path, out: &Path) -> Result<Index> {
    let mut packages = Vec::new();
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .with_context(|| format!("read {}", dir.display()))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().map(|x| x == "zst").unwrap_or(false))
        .collect();
    entries.sort();

    for path in entries {
        let raw = archive::read_pkginfo(&path)?;
        let info = archive::parse_pkginfo(&raw)?;
        let name = info.get("pkgname").cloned().unwrap_or_default();
        let version = info.get("pkgver").cloned().unwrap_or_default();
        let rel: u32 = info.get("pkgrel").and_then(|v| v.parse().ok()).unwrap_or(1);
        let arch = info.get("arch").cloned().unwrap_or_default();
        let sha256 = crate::fetch::sha256_of_file(&path)?;
        println!("nawa: index -> {}-{}-{}", name, version, rel);
        packages.push(RepoEntry {
            name,
            version,
            rel,
            arch,
            file: path.file_name().unwrap().to_string_lossy().to_string(),
            sha256,
        });
    }

    let idx = Index { generated: None, expires: None, packages };
    // a repo exposes ONE entry per package — the newest archive
    // (older archives may sit in the dir but never enter the index)
    let mut newest: std::collections::BTreeMap<String, RepoEntry> = std::collections::BTreeMap::new();
    for p in idx.packages {
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
    let idx = Index {
        generated: Some(now),
        expires: Some(now + INDEX_VALIDITY_SECS),
        packages: newest.into_values().collect(),
    };
    std::fs::write(out, toml::to_string_pretty(&idx)?)?;
    println!("nawa: wrote index {} ({} packages)", out.display(), idx.packages.len());
    Ok(idx)
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
        assert_eq!(idx.packages.len(), 1, "index must expose one entry per package");
        assert_eq!(idx.packages[0].version, "2.0");
        assert_eq!(idx.packages[0].rel, 1);
        assert_eq!(idx.find("demo").unwrap().version, "2.0");
        let _ = std::fs::remove_dir_all(&base);
    }
}
