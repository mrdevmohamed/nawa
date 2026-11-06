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
    pub reason: String, // "explicit" | "dependency" | "keep"(installer-internal sentinel)
    #[serde(default)]
    pub run: Vec<String>,
    #[serde(default)]
    pub install_date: u64,
    #[serde(default)]
    pub size: u64,
    /// SHA-256 of the exact archive bytes that produced this installation —
    /// the immutable object identity. Optional: legacy desc files without it
    /// stay valid.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_sha256: Option<String>,
    /// Provenance record pointer in `sha256:<digest>` form. Optional as well;
    /// the full authentication evidence lives in the object store sidecar,
    /// never copied into every package description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_record: Option<String>,
}

impl Entry {
    pub fn version_full(&self) -> String {
        format!("{}-{}-{}", self.name, self.version, self.rel)
    }

    /// In-memory fix-up for legacy entries that stored nanoseconds instead of
    /// Unix seconds in `install_date`. See [`normalize_install_date`].
    pub fn with_normalized_date(mut self) -> Self {
        if let Some(secs) = normalize_install_date(self.install_date) {
            self.install_date = secs;
        }
        self
    }
}

/// `install_date` holds **Unix seconds**.
///
/// Releases before the timestamp fix stored nanoseconds there, while
/// `humanize::format_date` (and every viewer) interpreted the field as
/// seconds, which produced absurd dates. Values at or above this threshold
/// cannot be a plausible Unix-seconds timestamp (it is astronomically in the
/// future) but match a nanosecond stamp of any realistic wall-clock age, so
/// they are classified as legacy nanoseconds.
///
/// Values below the threshold stay seconds: `as_nanos()` for the first
/// 27.7 hours since the epoch falls below 1e14 too, but no database can
/// have been written with such stamps (nawa did not exist yet), so the
/// conversion is one-way, conservative, and never touches genuinely small
/// explicit values (tests write 0 or 1).
pub const NANOS_THRESHOLD: u64 = 100_000_000_000_000; // 1e14

/// Legacy nanosecond `install_date` -> Unix seconds; `None` when the value
/// is already a seconds timestamp (or the "never recorded" 0).
pub fn normalize_install_date(raw: u64) -> Option<u64> {
    (raw >= NANOS_THRESHOLD).then(|| raw / 1_000_000_000)
}

impl Db {
    /// Durable repair for legacy nanosecond `install_date` values. Run while
    /// holding the exclusive database lock (see `install::recover`), which
    /// both call sites already guarantee.
    ///
    /// For every package whose desc contains a detected legacy stamp, the
    /// original file is kept as a recoverable backup (`desc.legacy-nanos`)
    /// before the corrected value is written, and existing backups are never
    /// overwritten — the conversion never destroys the raw data it judged
    /// about. Idempotent: after conversion the value is below the threshold,
    /// so a second run does nothing.
    pub fn repair_legacy_dates(&self) -> Result<u32> {
        let mut fixed = 0u32;
        if !self.local_dir().exists() {
            return Ok(fixed);
        }
        for entry_path in self.db_desc_files()? {
            let raw = std::fs::read_to_string(&entry_path)?;
            let parsed: Entry = match toml::from_str(&raw) {
                Ok(e) => e,
                Err(_) => continue, // a broken desc is not a timestamp problem
            };
            let secs = match normalize_install_date(parsed.install_date) {
                Some(s) => s,
                None => continue,
            };
            let backup = entry_path.with_file_name("desc.legacy-nanos");
            if !backup.exists() {
                std::fs::copy(&entry_path, &backup)?;
            }
            let mut fixed_entry = parsed;
            fixed_entry.install_date = secs;
            let desc = toml::to_string_pretty(&fixed_entry)?;
            let tmp = entry_path.with_file_name(".desc.tmp");
            std::fs::write(&tmp, desc)?;
            std::fs::rename(&tmp, &entry_path)?;
            fixed += 1;
        }
        Ok(fixed)
    }

    /// Every `<pkg>/desc` under the local database, sorted for determinism.
    fn db_desc_files(&self) -> Result<Vec<PathBuf>> {
        let mut out = Vec::new();
        for p in std::fs::read_dir(self.local_dir())?.flatten() {
            let desc = p.path().join("desc");
            if desc.is_file() {
                out.push(desc);
            }
        }
        out.sort();
        Ok(out)
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
            out.push(e.with_normalized_date());
        }
        Ok(out)
    }

    pub fn entry(&self, name: &str) -> Result<Option<Entry>> {
        let desc = self.checked_dir(name)?.join("desc");
        if !desc.exists() {
            return Ok(None);
        }
        let e: Entry = toml::from_str(&std::fs::read_to_string(desc)?)?;
        Ok(Some(e.with_normalized_date()))
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

    // ---- timestamp unit migration ------------------------------------------

    #[test]
    fn legacy_nanosecond_dates_are_recognized_as_seconds_after_conversion() {
        // ~2026-11-06 in nanoseconds, as old releases stored it
        let nanos: u64 = 1_791_388_800_000_000_000;
        let secs = normalize_install_date(nanos).expect("classified as nanos");
        assert_eq!(secs, 1_791_388_800);
        // any realistic nanosecond stamp converts below the threshold
        assert!(secs < NANOS_THRESHOLD);
        // plausibly seconds-shaped values and the "never recorded" 0 stay
        assert_eq!(normalize_install_date(0), None);
        assert_eq!(normalize_install_date(1), None);
        assert_eq!(normalize_install_date(1_791_388_800), None);
        assert_eq!(normalize_install_date(NANOS_THRESHOLD - 1), None);
    }

    #[test]
    fn repair_rewrites_legacy_dates_and_keeps_a_recoverable_backup_once() {
        let base = std::env::temp_dir().join(format!("nawa-db-dates-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let db = Db::new(&base);
        let e = Entry {
            name: "old".into(),
            version: "1.0".into(),
            rel: 1,
            arch: "x86_64".into(),
            reason: "explicit".into(),
            run: vec![],
            // written by a pre-fix release: nanoseconds
            install_date: 1_791_388_800_000_000_000,
            artifact_sha256: None,
            artifact_record: None,
            size: 5,
        };
        db.add(&e, &["/usr/bin/old".to_string()], &HashMap::new()).unwrap();

        // first run: converts in place, keeps the raw bytes
        assert_eq!(db.repair_legacy_dates().unwrap(), 1);
        let fixed = db.entry("old").unwrap().unwrap();
        assert_eq!(fixed.install_date, 1_791_388_800);
        let backup = db
            .local_dir()
            .join("old")
            .join("desc.legacy-nanos");
        assert!(backup.exists());
        let raw_backup = std::fs::read_to_string(&backup).unwrap();
        assert!(raw_backup.contains("1791388800000000000"), "{raw_backup}");

        // idempotent: a second run changes nothing, and the backup survives
        assert_eq!(db.repair_legacy_dates().unwrap(), 0);
        assert!(backup.exists());

        // a stamp small enough to be in the first 27.7h of the epoch is left
        // alone (not destructively rewritten)
        let tiny = db
            .local_dir()
            .join("old")
            .join("desc");
        let mut ambiguous = fixed.clone();
        ambiguous.install_date = 123_456_789; // 1973-11-29, plausible seconds
        std::fs::write(&tiny, toml::to_string_pretty(&ambiguous).unwrap()).unwrap();
        assert_eq!(db.repair_legacy_dates().unwrap(), 0);
        assert_eq!(db.entry("old").unwrap().unwrap().install_date, 123_456_789);
        let _ = std::fs::remove_dir_all(&base);
    }

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
            artifact_sha256: None,
            artifact_record: None,
            size: 0,
        };
        assert!(db.add(&e, &[], &HashMap::new()).is_err());
        assert!(!base.join("var/lib/nawa/evil").exists());
        let _ = std::fs::remove_dir_all(&base);
    }
}
