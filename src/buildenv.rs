//! Build environment: isolation levels and the environment a build sees.
//!
//! Implements the `env` level of docs/design/build-isolation.md §6:
//! at `env` and above, a build section receives *only* the whitelisted
//! variables below (plus the profile's compiler flags) — never nawa's own
//! environment — plus a fixed umask, locale, timezone and
//! `SOURCE_DATE_EPOCH` so that archives become byte-reproducible.
//!
//! Levels are ordered; each includes the previous one:
//!
//! | level      | adds                                                        |
//! |------------|-------------------------------------------------------------|
//! | `none`     | today's behaviour (inherit nawa's environment)                |
//! | `env`      | environment whitelist, umask 022, SOURCE_DATE_EPOCH,         |
//! |            | normalised archives                                          |
//! | `sandbox`  | namespaces over the live root (no network, ro root, …)      |
//! | `closure`  | the root is an assembled build root, not the live system     |

use anyhow::{bail, Result};
use std::path::Path;

/// Isolation level for a build (see the module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Isolation {
    /// Today's behaviour: inherit nawa's environment, no sandbox.
    None,
    /// Whitelisted environment, umask, SOURCE_DATE_EPOCH, normalised archive.
    Env,
    /// User/mount/pid/net/ipc/uts namespaces over the live root.
    Sandbox,
    /// Like `sandbox`, but the root is assembled from the dependency closure.
    Closure,
}

impl Isolation {
    pub fn parse(s: &str) -> Result<Self> {
        Ok(match s.trim() {
            "none" => Isolation::None,
            "env" => Isolation::Env,
            "sandbox" => Isolation::Sandbox,
            "closure" => Isolation::Closure,
            other => bail!(
                "unknown isolation level `{other}` (expected none | env | sandbox | closure)"
            ),
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Isolation::None => "none",
            Isolation::Env => "env",
            Isolation::Sandbox => "sandbox",
            Isolation::Closure => "closure",
        }
    }

    /// A shell-parsable prefix for a build section: `umask 022` is folded into
    /// the script at `env` and above (an environment variable cannot carry it).
    pub fn script_prefix(self) -> &'static str {
        match self {
            Isolation::None => "",
            _ => "umask 022\n",
        }
    }
}

/// Where `HOME` points inside a sandboxed build (bound to an empty dir);
/// the §4.2 default when the build base is the pinned `/build`.
#[allow(dead_code)] // referenced by the whitelist test and the docs
pub const SANDBOX_HOME: &str = "/build/home";
/// Where the sources live inside a sandboxed build.
#[allow(dead_code)] // referenced by the whitelist test and the docs
pub const SANDBOX_SRC: &str = "/build/src";
/// Where `DESTDIR` (`%{pkgdir}`) points inside a sandboxed build.
#[allow(dead_code)] // referenced by the whitelist test and the docs
pub const SANDBOX_PKG: &str = "/build/pkg";

/// Everything a build section sees, as a sorted list of key/value pairs.
///
/// * `profile_env` — CFLAGS/CXXFLAGS/LDFLAGS from the build profile
///   (`profile.rs` already handles "explicit env beats profile").
/// * `srcdir` / `pkgdir` — paths *as the build sees them*: the host paths at
///   `none`/`env`, the `/build/{src,pkg}` bind points at `sandbox`/`closure`.
pub struct EnvSpec<'a> {
    /// Informational: the whitelist is the same from `env` up; kept so the
    /// caller documents what it is building for.
    #[allow(dead_code)]
    pub isolation: Isolation,
    pub profile_env: &'a [(String, String)],
    pub jobs: u32,
    pub srcdir: &'a Path,
    pub pkgdir: &'a Path,
    /// `$HOME` inside the sandbox (the bind of an empty dir).
    pub home: &'a str,
    pub source_date_epoch: u64,
}

/// The whitelist for `env` and above — nothing else is inherited.
pub fn whitelist(spec: &EnvSpec) -> Vec<(String, String)> {
    let mut v: Vec<(String, String)> = vec![
        ("PATH".into(), "/usr/bin:/usr/sbin".into()),
        ("HOME".into(), spec.home.to_string()),
        ("TMPDIR".into(), "/tmp".into()),
        ("LC_ALL".into(), "C".into()),
        ("LANG".into(), "C".into()),
        ("TZ".into(), "UTC".into()),
        ("SHELL".into(), "/bin/bash".into()),
        (
            "SOURCE_DATE_EPOCH".into(),
            spec.source_date_epoch.to_string(),
        ),
    ];
    // profile flags are part of the whitelist, not inherited extras
    v.extend(spec.profile_env.iter().cloned());
    v.push(("NAWA_JOBS".into(), spec.jobs.to_string()));
    v.push(("NAWA_SRCDIR".into(), spec.srcdir.display().to_string()));
    v.push(("NAWA_PKGDIR".into(), spec.pkgdir.display().to_string()));
    v.sort_by(|a, b| a.0.cmp(&b.0));
    v
}

/// The `SOURCE_DATE_EPOCH` for a build: the newest mtime in the extracted
/// source tree (reproducible: the tarball fixes every mtime), or the value
/// pinned by `[package] source_date_epoch` when the recipe sets one.
///
/// The mtime of the staging *parent* directories is ignored — only the files
/// (and dirs) that came out of the tarball count, so re-extracting at a later
/// time does not change the epoch.
pub fn source_date_epoch(srcdir: &Path, pinned: Option<u64>) -> Result<u64> {
    if let Some(p) = pinned {
        return Ok(p);
    }
    let mut newest: u64 = 1;
    for e in walkdir::WalkDir::new(srcdir).max_depth(usize::MAX) {
        let e = match e {
            Ok(e) => e,
            // dangling symlinks have no metadata to read
            Err(_) => continue,
        };
        // skip the extraction root itself: nawa created it just now, so its
        // mtime is the extraction time, not anything the tarball pinned
        if e.depth() == 0 {
            continue;
        }
        let Ok(md) = e.metadata() else { continue };
        let mtime = md
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        newest = newest.max(mtime);
    }
    // never below the GNU packaging default; 0 would be 1970-01-01 and make
    // some tools (gzip -n, zip) emit timestamps they then refuse to read back
    Ok(newest.max(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile() -> Vec<(String, String)> {
        vec![
            ("CFLAGS".to_string(), "-O2 -pipe".to_string()),
            ("LDFLAGS".to_string(), "-Wl,-O1".to_string()),
        ]
    }

    #[test]
    fn levels_parse_and_order() {
        assert!(Isolation::parse("none").unwrap() == Isolation::None);
        assert!(Isolation::parse("closure").unwrap() == Isolation::Closure);
        assert!(Isolation::parse("bogus").is_err());
        assert!(Isolation::None < Isolation::Env);
        assert!(Isolation::Env < Isolation::Sandbox);
        assert!(Isolation::Sandbox < Isolation::Closure);
    }

    #[test]
    fn whitelist_contains_exactly_the_design_set() {
        let profile = profile();
        let spec = EnvSpec {
            isolation: Isolation::Env,
            profile_env: &profile,
            jobs: 8,
            srcdir: Path::new(SANDBOX_SRC),
            pkgdir: Path::new(SANDBOX_PKG),
            home: SANDBOX_HOME,
            source_date_epoch: 1_700_000_000,
        };
        let env = whitelist(&spec);
        let keys: Vec<&str> = env.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            keys,
            vec![
                "CFLAGS", "HOME", "LANG", "LC_ALL", "LDFLAGS", "NAWA_JOBS", "NAWA_PKGDIR",
                "NAWA_SRCDIR", "PATH", "SHELL", "SOURCE_DATE_EPOCH", "TMPDIR", "TZ",
            ],
            "env whitelist changed — update docs/design/build-isolation.md §6"
        );
        let get = |k: &str| {
            env.iter()
                .find(|(ek, _)| ek == k)
                .map(|(_, v)| v.clone())
                .unwrap()
        };
        assert_eq!(get("LC_ALL"), "C");
        assert_eq!(get("TZ"), "UTC");
        assert_eq!(get("HOME"), SANDBOX_HOME);
        assert_eq!(get("NAWA_JOBS"), "8");
        assert_eq!(get("SOURCE_DATE_EPOCH"), "1700000000");
        assert_eq!(get("CFLAGS"), "-O2 -pipe");
    }

    #[test]
    fn sde_is_newest_mtime_and_pinned_wins() {
        let d = std::env::temp_dir().join(format!("nawa-sde-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("sub")).unwrap();
        std::fs::write(d.join("a"), b"x").unwrap();
        std::fs::write(d.join("sub").join("b"), b"y").unwrap();
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        let newer = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
        let set_mtime = |p: &Path, t: std::time::SystemTime| {
            // opening a directory for write fails (EISDIR); read-only is
            // enough — set_modified(utimensat) works on any fd
            std::fs::File::open(p).unwrap().set_modified(t).unwrap();
        };
        set_mtime(&d.join("a"), old);
        set_mtime(&d.join("sub").join("b"), newer);
        // the dirs themselves carry mtimes in a real extraction; include them
        set_mtime(&d, old);
        set_mtime(&d.join("sub"), old);

        let sde = source_date_epoch(&d, None).unwrap();
        let expect = newer
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert_eq!(sde, expect, "SDE must be the newest source mtime");

        assert_eq!(source_date_epoch(&d, Some(42)).unwrap(), 42);
        // an empty tree (meta packages) falls back to the GNU default, not an
        // error — the recipe is the only input there
        let empty = d.join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        assert_eq!(source_date_epoch(&empty, None).unwrap(), 1);
        let _ = std::fs::remove_dir_all(&d);
    }
}
