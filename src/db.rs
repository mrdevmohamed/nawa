//! Local package database: plain text files under
//! `<root>/var/lib/nawa/local/<name>/{desc,files}` — same philosophy as
//! pacman/dpkg, easy to inspect and repair by hand.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub name: String,
    pub version: String,
    pub rel: u32,
    #[serde(default)]
    pub arch: String,
    #[serde(default)]
    pub reason: String, // "explicit" | "dependency"
    #[serde(default)]
    pub run: Vec<String>,
    #[serde(default)]
    pub install_date: u64,
    #[serde(default)]
    pub size: u64,
}

impl Entry {
    pub fn version_full(&self) -> String {
        format!("{}-{}-{}", self.name, self.version, self.rel)
    }
}

/// Package names become directory names (db, undo log, hooks) and parts of
/// archive file names, so they must not be able to contain path syntax.
/// Allowed: ASCII letters/digits and `+ _ . -`, starting with a letter/digit.
pub fn valid_name(n: &str) -> bool {
    let mut chars = n.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphanumeric() => {}
        _ => return false,
    }
    n.len() <= 128 && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '_' | '.' | '-'))
}

/// Versions end up in file names too (`<name>-<ver>-<rel>.nawa.tar.zst`).
pub fn valid_version(v: &str) -> bool {
    let mut chars = v.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphanumeric() => {}
        _ => return false,
    }
    v.len() <= 128
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '_' | '.' | '-' | '~' | ':'))
}

pub fn check_name(n: &str) -> Result<()> {
    if !valid_name(n) {
        bail!("invalid package name `{}` (allowed: letters, digits and + _ . -)", n.escape_debug());
    }
    Ok(())
}

pub struct Db {
    root: PathBuf,
}

impl Db {
    pub fn new(root: &Path) -> Self {
        Db { root: root.to_path_buf() }
    }

    pub fn local_dir(&self) -> PathBuf {
        self.root.join("var/lib/nawa/local")
    }

    fn pkg_dir(&self, name: &str) -> PathBuf {
        self.local_dir().join(name)
    }

    /// `pkg_dir` for names that come from outside (archives, CLI): validated.
    fn checked_dir(&self, name: &str) -> Result<PathBuf> {
        check_name(name)?;
        Ok(self.pkg_dir(name))
    }

    pub fn installed(&self) -> Result<Vec<Entry>> {
        let mut out = Vec::new();
        if !self.local_dir().exists() {
            return Ok(out);
        }
        let mut names: Vec<_> = std::fs::read_dir(self.local_dir())?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.join("desc").exists())
            .collect();
        names.sort();
        for p in names {
            let raw = std::fs::read_to_string(p.join("desc"))?;
            let e: Entry = toml::from_str(&raw)
                .with_context(|| format!("corrupt db entry {}", p.display()))?;
            out.push(e);
        }
        Ok(out)
    }

    pub fn entry(&self, name: &str) -> Result<Option<Entry>> {
        let desc = self.checked_dir(name)?.join("desc");
        if !desc.exists() {
            return Ok(None);
        }
        Ok(Some(toml::from_str(&std::fs::read_to_string(desc)?)?))
    }

    pub fn files(&self, name: &str) -> Result<Vec<String>> {
        let f = self.checked_dir(name)?.join("files");
        if !f.exists() {
            return Ok(Vec::new());
        }
        Ok(std::fs::read_to_string(f)?
            .lines()
            .map(|l| l.to_string())
            .filter(|l| !l.is_empty())
            .collect())
    }

    /// sha256 of every /etc file at the time it was installed
    /// ("<hash>  <abs-path>" lines, sha256sum style). Used to tell
    /// locally-modified configs (diverted to .nawa-new) from untouched
    /// ones (overwritten in place) during upgrades.
    pub fn hashes(&self, name: &str) -> Result<HashMap<String, String>> {
        let f = self.checked_dir(name)?.join("hashes");
        let mut map = HashMap::new();
        if !f.exists() {
            return Ok(map);
        }
        for line in std::fs::read_to_string(f)?.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if let Some((h, p)) = line.split_once(char::is_whitespace) {
                map.insert(p.trim().to_string(), h.trim().to_string());
            }
        }
        Ok(map)
    }

    pub fn add(&self, e: &Entry, files: &[String], hashes: &HashMap<String, String>) -> Result<()> {
        let dir = self.checked_dir(&e.name)?;
        std::fs::create_dir_all(&dir)?;
        // write temp + rename so an interrupted install can't leave a
        // half-written entry behind
        let desc = toml::to_string_pretty(e)?;
        std::fs::write(dir.join(".desc.tmp"), desc)?;
        std::fs::rename(dir.join(".desc.tmp"), dir.join("desc"))?;
        std::fs::write(dir.join(".files.tmp"), files.join("\n") + "\n")?;
        std::fs::rename(dir.join(".files.tmp"), dir.join("files"))?;
        let mut h: Vec<String> = hashes
            .iter()
            .map(|(p, s)| format!("{s}  {p}"))
            .collect();
        h.sort();
        std::fs::write(dir.join(".hashes.tmp"), h.join("\n") + "\n")?;
        std::fs::rename(dir.join(".hashes.tmp"), dir.join("hashes"))?;
        Ok(())
    }

    pub fn remove(&self, name: &str) -> Result<()> {
        let dir = self.checked_dir(name)?;
        if dir.exists() {
            std::fs::remove_dir_all(dir)?;
        }
        Ok(())
    }

    /// Map of every installed file -> owning package name.
    pub fn file_owners(&self) -> Result<HashMap<String, String>> {
        let mut map = HashMap::new();
        for e in self.installed()? {
            for f in self.files(&e.name)? {
                map.insert(f, e.name.clone());
            }
        }
        Ok(map)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_with_path_syntax_are_rejected() {
        for ok in ["gzip", "libstdc++", "python-mako", "a.b_c-1", "7zip"] {
            assert!(valid_name(ok), "{ok}");
        }
        for bad in ["", ".", "..", "../x", "a/b", "/etc", "-rf", ".hidden", "a b", "a\nb", "x\0y", "é"] {
            assert!(!valid_name(bad), "{bad:?}");
        }
        assert!(!valid_name(&"a".repeat(129)));
        assert!(valid_version("1.13") && valid_version("2.0.0~rc1") && valid_version("1:2.3"));
        assert!(!valid_version("../1") && !valid_version("1/2") && !valid_version(""));
    }

    #[test]
    fn db_refuses_traversal_names() {
        let base = std::env::temp_dir().join(format!("nawa-db-name-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let db = Db::new(&base);
        assert!(db.entry("../../x").is_err());
        assert!(db.files("..").is_err());
        assert!(db.remove("../..").is_err());
        let e = Entry {
            name: "../evil".into(),
            version: "1".into(),
            rel: 1,
            arch: String::new(),
            reason: String::new(),
            run: vec![],
            install_date: 0,
            size: 0,
        };
        assert!(db.add(&e, &[], &HashMap::new()).is_err());
        assert!(!base.join("var/lib/nawa/evil").exists());
        let _ = std::fs::remove_dir_all(&base);
    }
}
