//! Local package database: plain text files under
//! `<root>/var/lib/nawa/local/<name>/{desc,files}` — same philosophy as
//! pacman/dpkg, easy to inspect and repair by hand.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Entry {
    pub name: String,
    pub version: String,
    pub rel: u32,
    #[serde(default)]
    pub arch: String,
    #[serde(default)]
    pub reason: String, // "explicit" | "dependency" | "group"
    #[serde(default)]
    pub run: Vec<String>,
    /// Unix SECONDS (the human formatters interpret it that way). Legacy
    /// entries written by pre-roadmap nawa stored nanoseconds here; see
    /// [`normalize_legacy_install_date`].
    #[serde(default)]
    pub install_date: u64,
    #[serde(default)]
    pub size: u64,
    /// SHA-256 of the exact archive bytes this entry was installed from
    /// (a content-addressed object reference, roadmap §4.6). Empty for
    /// legacy entries; never a filename.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub artifact_sha256: String,
}

impl Entry {
    pub fn version_full(&self) -> String {
        format!("{}-{}-{}", self.name, self.version, self.rel)
    }
}

/// Interpret a stored `install_date` as it should be displayed.
///
/// Pre-roadmap nawa wrote a NANOSECOND timestamp into `Entry.install_date`
/// while every formatter read it as Unix seconds, so legacy entries show as
/// far-future dates. A value is unambiguously nanoseconds when it exceeds
/// `u64::MAX / 1000` — converting it by `/1000` can never corrupt a genuine
/// seconds value, because real seconds timestamps stay far below that
/// threshold until the year ~584 billion. Values between one year and that
/// threshold are ambiguous in theory; the conservative rule keeps them as-is
/// (seconds interpretation) and migration only rewrites clearly-nanosecond
/// values, idempotently.
pub(crate) fn normalize_legacy_install_date(raw: u64) -> u64 {
    const NANOS_THRESHOLD: u64 = u64::MAX / 1000;
    if raw > NANOS_THRESHOLD {
        raw / 1_000_000_000
    } else {
        raw
    }
}

/// One-shot, idempotent migration of legacy nanosecond `install_date`
/// values across the whole database (roadmap §4.3):
/// * old descriptions keep deserializing (the field is just a u64);
/// * a clearly-nanosecond value is rewritten to seconds, with a `.bak`
///   copy of the old `desc` kept next to it the first time;
/// * already-correct entries are left untouched, so re-running is a no-op.
///
/// Must be called while holding the exclusive database lock.
pub(crate) fn migrate_install_dates(root: &Path) -> Result<Vec<String>> {
    let db = Db::new(root);
    let mut changed = Vec::new();
    if !db.local_dir().exists() {
        return Ok(changed);
    }
        for entry in db.installed()? {
            let dir = db.local_dir().join(&entry.name);
            let desc = dir.join("desc");
            if !desc.exists() {
                continue;
            }
        let fixed = normalize_legacy_install_date(entry.install_date);
        if fixed == entry.install_date {
            continue;
        }
        // Recoverable backup, only when not already present. The rewrite
        // itself is a durable atomic publication (fsync file + parent dir),
        // so a crash cannot leave a half-written desc behind.
        let bak = dir.join("desc.pre-seconds.bak");
        if !bak.exists() {
            std::fs::copy(&desc, &bak).with_context(|| format!("back up {}", desc.display()))?;
        }
        let mut e = entry.clone();
        e.install_date = fixed;
        let new = toml::to_string_pretty(&e)?;
        crate::transaction::atomic::write_atomic(&desc, new.as_bytes(), 0o644)?;
        changed.push(format!(
            "{}: install date migrated from nanoseconds to seconds",
            e.name
        ));
    }
    Ok(changed)
}

/// Package names become directory names (db, undo log, hooks) and parts of
/// archive file names, so they must not be able to contain path syntax.
/// Allowed: ASCII letters/digits and `+ _ . -`, starting with a letter/digit.
pub(crate) fn valid_name(n: &str) -> bool {
    let mut chars = n.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphanumeric() => {}
        _ => return false,
    }
    n.len() <= 128 && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '_' | '.' | '-'))
}

/// Versions end up in file names too (`<name>-<ver>-<rel>.nawa.tar.zst`).
pub(crate) fn valid_version(v: &str) -> bool {
    let mut chars = v.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphanumeric() => {}
        _ => return false,
    }
    v.len() <= 128
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '_' | '.' | '-' | '~' | ':'))
}

pub(crate) fn check_name(n: &str) -> Result<()> {
    if !valid_name(n) {
        bail!("invalid package name `{}` (allowed: letters, digits and + _ . -)", n.escape_debug());
    }
    Ok(())
}

pub(crate) struct Db {
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

    /// The (validated) db directory of `name`, for transaction snapshots.
    pub fn entry_dir(&self, name: &str) -> Result<PathBuf> {
        self.checked_dir(name)
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
        // Durable atomic publication for each file (follows
        // transaction/atomic.rs): a crash leaves the previous, fully-valid
        // entry or the new, fully-valid entry — never a half-written one.
        // `write_atomic` creates parent directories durably, so no
        // separate `create_dir_all` is needed here.
        let desc = toml::to_string_pretty(e)?;
        crate::transaction::atomic::write_atomic(&dir.join("desc"), desc.as_bytes(), 0o644)?;
        crate::transaction::atomic::write_atomic(
            &dir.join("files"),
            (files.join("\n") + "\n").as_bytes(),
            0o644,
        )?;
        let mut h: Vec<String> = hashes
            .iter()
            .map(|(p, s)| format!("{s}  {p}"))
            .collect();
        h.sort();
        crate::transaction::atomic::write_atomic(
            &dir.join("hashes"),
            (h.join("\n") + "\n").as_bytes(),
            0o644,
        )?;
        Ok(())
    }

    pub fn remove(&self, name: &str) -> Result<()> {
        let dir = self.checked_dir(name)?;
        if dir.exists() {
            std::fs::remove_dir_all(dir)?;
        }
        Ok(())
    }

    /// Every path listed by an installed package other than `name`.
    ///
    /// A path can legitimately be claimed by two packages (a directory such
    /// as `/etc`, or a merged-usr symlink like `/bin` that one package ships
    /// and another package's directory entry also names). Removing or
    /// obsoleting such a path on behalf of one package would pull it out from
    /// under the other, so callers leave these paths alone.
    pub fn paths_claimed_by_others(&self, name: &str) -> Result<std::collections::HashSet<String>> {
        let mut set = std::collections::HashSet::new();
        for e in self.installed()? {
            if e.name != name {
                set.extend(self.files(&e.name)?);
            }
        }
        Ok(set)
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
            artifact_sha256: String::new(),
        };
        assert!(db.add(&e, &[], &HashMap::new()).is_err());
        assert!(!base.join("var/lib/nawa/evil").exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn legacy_nanosecond_dates_are_normalized_conservatively() {
        // a genuine seconds timestamp (year 2026) is kept
        assert_eq!(normalize_legacy_install_date(1_762_000_000), 1_762_000_000);
        // a nanosecond timestamp (same instant) is converted
        assert_eq!(normalize_legacy_install_date(1_762_000_000_000_000_000), 1_762_000_000);
        // zero/unknown stays
        assert_eq!(normalize_legacy_install_date(0), 0);
    }

    #[test]
    fn migration_is_idempotent_and_backs_up() {
        let base = std::env::temp_dir().join(format!("nawa-db-migrate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let db = Db::new(&base);
        let e = Entry {
            name: "old".into(),
            version: "1".into(),
            rel: 1,
            arch: String::new(),
            reason: "explicit".into(),
            run: vec![],
            // what pre-roadmap nawa actually wrote (nanos)
            install_date: 1_762_000_000_000_000_000,
            size: 1,
            artifact_sha256: String::new(),
        };
        db.add(&e, &[], &HashMap::new()).unwrap();
        let changed = migrate_install_dates(&base).unwrap();
        assert_eq!(changed.len(), 1);
        let after = db.entry("old").unwrap().unwrap();
        assert_eq!(after.install_date, 1_762_000_000);
        // a .bak copy of the original desc exists
        assert!(db.local_dir().join("old/desc.pre-seconds.bak").exists());
        // second run: nothing left to do
        assert!(migrate_install_dates(&base).unwrap().is_empty());
        assert_eq!(db.entry("old").unwrap().unwrap().install_date, 1_762_000_000);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn artifact_reference_field_is_optional_for_legacy_entries() {
        let raw = "name = \"x\"\nversion = \"1\"\nrel = 1\n";
        let e: Entry = toml::from_str(raw).unwrap();
        assert_eq!(e.artifact_sha256, "");
        let serialized = toml::to_string_pretty(&e).unwrap();
        assert!(!serialized.contains("artifact_sha256"), "legacy shape preserved on write");
    }
}
