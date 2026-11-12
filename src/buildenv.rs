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
use std::path::{Path, PathBuf};

/// Isolation level for a build (see the module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Isolation {
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

/// The `PATH` every whitelisted (`env` and above) build section sees, and
/// the lookup path for the host-toolchain identity probe.
pub(crate) const BUILD_PATH: &str = "/usr/bin:/usr/sbin";

/// Where `HOME` points inside a sandboxed build (bound to an empty dir);
/// the §4.2 default when the build base is the pinned `/build`.
pub(crate) const SANDBOX_HOME: &str = "/build/home";
/// Where the sources live inside a sandboxed build.
#[allow(dead_code)] // referenced by the whitelist test and the docs
pub(crate) const SANDBOX_SRC: &str = "/build/src";
/// Where `DESTDIR` (`%{pkgdir}`) points inside a sandboxed build.
#[allow(dead_code)] // referenced by the whitelist test and the docs
pub(crate) const SANDBOX_PKG: &str = "/build/pkg";

/// Everything a build section sees, as a sorted list of key/value pairs.
///
/// * `profile_env` — CFLAGS/CXXFLAGS/LDFLAGS from the build profile
///   (`profile.rs` already handles "explicit env beats profile").
/// * `srcdir` / `pkgdir` — paths *as the build sees them*: the host paths at
///   `none`/`env`, the `/build/{src,pkg}` bind points at `sandbox`/`closure`.
pub(crate) struct EnvSpec<'a> {
    pub profile_env: &'a [(String, String)],
    pub jobs: u32,
    pub srcdir: &'a Path,
    pub pkgdir: &'a Path,
    /// `$HOME` inside the sandbox (the bind of an empty dir).
    pub home: &'a str,
    pub source_date_epoch: u64,
    /// `NAWA_SRCROOT` for recipes using `[[package.sources]]`
    /// (fetch-phase.md §4.3). `None` for legacy recipes — the whitelist
    /// must stay exactly today's set for them.
    pub srcroot: Option<&'a Path>,
    /// `NAWA_ASSETSDIR` for recipes that declare `[assets]`
    /// (fetch-phase.md §5.3). Same legacy rule as `srcroot`.
    pub assetsdir: Option<&'a Path>,
    /// Extra whitelist entries a recipe's `[fetch.*]` tables require
    /// (fetch-phase.md §7: GOMODCACHE, GOPROXY=off, GOFLAGS, GOTOOLCHAIN,
    /// GOWORK, GOCACHE; §8 adds the Cargo names). Empty for every legacy
    /// recipe, so the whitelist stays exactly today's set for them.
    pub fetch_env: Vec<(String, String)>,
}

/// The whitelist for `env` and above — nothing else is inherited.
pub(crate) fn whitelist(spec: &EnvSpec) -> Vec<(String, String)> {
    let mut v: Vec<(String, String)> = vec![
        ("PATH".into(), BUILD_PATH.into()),
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
    // new-schema names join the whitelist only when the recipe enables
    // them (§4.3/§5.3): a legacy recipe's whitelist is unchanged
    if let Some(p) = spec.srcroot {
        v.push(("NAWA_SRCROOT".into(), p.display().to_string()));
    }
    if let Some(p) = spec.assetsdir {
        v.push(("NAWA_ASSETSDIR".into(), p.display().to_string()));
    }
    // [fetch.*] whitelist additions (§7): joined only when the backend is
    // declared; the paths they carry are already in the frame the build sees
    v.extend(spec.fetch_env.iter().cloned());
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
pub(crate) fn source_date_epoch(srcdir: &Path, pinned: Option<u64>) -> Result<u64> {
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

/// The new-schema variant (fetch-phase.md §6): only **regular files** feed
/// the epoch. Directories are ignored — merging and later child creation
/// bump them — and symlinks/special files never carry a trustworthy mtime.
///
/// The caller must have already created every source in the workspace and
/// must compute this *before* stamping `unpack = false` files (their copy
/// carries the staging time, §6). Unlike the legacy variant this errors
/// when the workspace has no regular file at all: with no tarball there is
/// nothing reproducible to derive the epoch from, and the recipe must pin
/// `package.source_date_epoch` instead.
pub(crate) fn source_date_epoch_from_files(srcdir: &Path) -> Result<u64> {
    let mut newest: Option<u64> = None;
    for e in walkdir::WalkDir::new(srcdir).max_depth(usize::MAX) {
        let e = match e {
            Ok(e) => e,
            // dangling symlinks have no metadata to read
            Err(_) => continue,
        };
        // the workspace root itself was just created by the staging engine,
        // never by the sources
        if e.depth() == 0 {
            continue;
        }
        let Ok(md) = e.metadata() else { continue };
        // WalkDir without follow_links returns symlink_metadata: a symlink
        // to a regular file does not pass the is_file test either
        if !md.is_file() {
            continue;
        }
        let mtime = md
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs());
        newest = match (newest, mtime) {
            (_, Some(m)) => Some(newest.unwrap_or(m).max(m)),
            (n, None) => n,
        };
    }
    match newest {
        Some(t) => Ok(t.max(1)),
        None => bail!(
            "no regular source files under {} to derive SOURCE_DATE_EPOCH from — \
             a recipe with no archive sources must pin package.source_date_epoch \
             (fetch-phase.md §6)",
            srcdir.display()
        ),
    }
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
            profile_env: &profile,
            jobs: 8,
            srcdir: Path::new(SANDBOX_SRC),
            pkgdir: Path::new(SANDBOX_PKG),
            home: SANDBOX_HOME,
            source_date_epoch: 1_700_000_000,
            srcroot: None,
            assetsdir: None,
            fetch_env: Vec::new(),
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

    /// The two new-schema names join the whitelist only when the recipe
    /// enables them — and then extend, never replace, today's set.
    #[test]
    fn whitelist_gains_srcroot_and_assetsdir_only_when_enabled() {
        let profile = profile();
        let mk = |srcroot: Option<&Path>, assetsdir: Option<&Path>, fetch_env: &[(String, String)]| {
            let spec = EnvSpec {
                profile_env: &profile,
                jobs: 8,
                srcdir: Path::new(SANDBOX_SRC),
                pkgdir: Path::new(SANDBOX_PKG),
                home: SANDBOX_HOME,
                source_date_epoch: 1_700_000_000,
                srcroot,
                assetsdir,
                fetch_env: fetch_env.to_vec(),
            };
            whitelist(&spec)
        };
        // enabled: exactly the legacy set plus the two names, sorted in
        let env = mk(
            Some(Path::new("/build/src/kernel")),
            Some(Path::new("/build/assets")),
            &[("GOMODCACHE".to_string(), "/build/fetch/go".to_string())],
        );
        let keys: Vec<&str> = env.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            keys,
            vec![
                "CFLAGS", "GOMODCACHE", "HOME", "LANG", "LC_ALL", "LDFLAGS", "NAWA_ASSETSDIR",
                "NAWA_JOBS", "NAWA_PKGDIR", "NAWA_SRCDIR", "NAWA_SRCROOT", "PATH", "SHELL",
                "SOURCE_DATE_EPOCH", "TMPDIR", "TZ",
            ],
            "new-schema whitelist = legacy set + NAWA_SRCROOT + NAWA_ASSETSDIR + [fetch] env"
        );
        let get = |k: &str| {
            env.iter().find(|(ek, _)| *ek == k).map(|(_, v)| v.clone()).unwrap()
        };
        assert_eq!(get("NAWA_SRCROOT"), "/build/src/kernel");
        assert_eq!(get("NAWA_ASSETSDIR"), "/build/assets");
        assert_eq!(get("GOMODCACHE"), "/build/fetch/go");
        // each name alone is possible (sources-only or assets-only recipe)
        for (srcroot, assetsdir) in
            [(Some(Path::new("/r")), None), (None, Some(Path::new("/a")))]
        {
            let keys: Vec<String> =
                mk(srcroot, assetsdir, &[]).into_iter().map(|(k, _)| k).collect();
            assert!(
                keys.iter().any(|k| k == "NAWA_SRCROOT") == srcroot.is_some(),
                "NAWA_SRCROOT presence must follow the field"
            );
            assert!(
                keys.iter().any(|k| k == "NAWA_ASSETSDIR") == assetsdir.is_some(),
                "NAWA_ASSETSDIR presence must follow the field"
            );
            // fetch entries are absent without them — legacy recipes keep
            // exactly today's whitelist (§16 backward compatibility 3)
            assert!(
                !keys.iter().any(|k| k == "GOMODCACHE"),
                "a legacy whitelist must not gain Go names"
            );
        }
    }

    /// The `[fetch.*]` whitelist extensions (fetch-phase.md §7): they ride
    /// the same allowlist — nothing Go-related is inherited from nawa's own
    /// environment.
    #[test]
    fn fetch_env_joins_the_whitelist_only_when_present() {
        let profile = profile();
        let base = |fetch_env: Vec<(String, String)>| {
            let spec = EnvSpec {
                profile_env: &profile,
                jobs: 8,
                srcdir: Path::new(SANDBOX_SRC),
                pkgdir: Path::new(SANDBOX_PKG),
                home: SANDBOX_HOME,
                source_date_epoch: 1_700_000_000,
                srcroot: None,
                assetsdir: None,
                fetch_env,
            };
            whitelist(&spec)
        };
        let env = base(Vec::new());
        assert!(
            !env.iter().any(|(k, _)| k == "GOMODCACHE" || k == "GOCACHE"),
            "an empty fetch table must not add build-time env"
        );
        let env = base(vec![
            ("GOMODCACHE".into(), "/build/fetch/go".into()),
            ("GOCACHE".into(), "/build/home/gocache".into()),
        ]);
        let get = |k: &str| env.iter().find(|(ek, _)| *ek == k).map(|(_, v)| v.clone()).unwrap();
        assert_eq!(get("GOMODCACHE"), "/build/fetch/go");
        assert_eq!(get("GOCACHE"), "/build/home/gocache");
        assert_eq!(env.len(), 15, "two extra keys, rest identical to legacy");
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

    /// §6 new-schema rules: directories and symlinks are ignored — only
    /// regular files count — and a workspace with none is a hard error
    /// (the recipe must pin the epoch).
    #[test]
    fn epoch_from_files_counts_regular_files_only() {
        let d = std::env::temp_dir().join(format!("nawa-sdef-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("only-dirs").join("deep")).unwrap();
        std::os::unix::fs::symlink("elsewhere", d.join("link")).unwrap();

        // nothing regular: the pinned-epoch demand (§6)
        let err = source_date_epoch_from_files(&d).unwrap_err();
        assert!(
            err.to_string().contains("source_date_epoch"),
            "got: {err}"
        );

        // one regular file makes the epoch — the newest regular-file mtime
        let f = d.join("x");
        std::fs::write(&f, b"data").unwrap();
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        std::fs::File::open(&f).unwrap().set_modified(old).unwrap();
        let newest = d.join("only-dirs").join("deep").join("f");
        std::fs::write(&newest, b"data").unwrap();
        let expect = newest
            .metadata()
            .unwrap()
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert_eq!(source_date_epoch_from_files(&d).unwrap(), expect);
        let _ = std::fs::remove_dir_all(&d);
    }
}

/// What to do with a recipe's `[build] check` section (its test suite).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CheckMode {
    /// Skip it (default — test suites are slow and some, like glibc's, have
    /// known failures).
    Off,
    /// Run it after `compile`; a failure aborts the build.
    Fatal,
    /// Run it after `compile`; a failure only prints a warning.
    Warn,
}


#[derive(Debug, Clone)]
pub(crate) struct BuildOpts {
    pub jobs: u32,
    pub builddir: PathBuf,
    pub outdir: PathBuf,
    /// Extra environment for every build section (from the build profile).
    pub env: Vec<(String, String)>,
    pub check: CheckMode,
    // ---- isolation (docs/design/build-isolation.md) ----------------------
    pub isolation: Isolation,
    /// Per-section wall-clock limit; only enforced inside the sandbox.
    pub timeout_secs: u64,
    /// `[sandbox] network = true` is not enough — the user must pass this.
    pub allow_network: bool,
    /// Continue when the host kernel refuses an isolation feature.
    pub allow_degraded: bool,
    /// Target root (trust store for closure archive verification).
    pub root: PathBuf,
    /// Recipes root (closure resolution).
    pub recipes_root: PathBuf,
    /// Accept unsigned dependency archives when trust keys exist.
    pub allow_unsigned: bool,
    /// Effective profile flags, for the build id.
    pub profile_flags: String,
    /// Opt-in (`nawa build --use-cache`, §7/§10 Phase 5): when an archive for
    /// this package already exists and its recorded `build_id` matches the
    /// one the current inputs would produce, skip the build entirely.
    /// The id covers the recipe bytes, source sha256, closure archive set
    /// and profile flags — not the wall clock, not the environment.
    pub use_cache: bool,
    /// Refuse every download instead of opening a connection (`--offline`,
    /// `NAWA_OFFLINE`; fetch-phase.md §12/§15). Missing inputs are reported
    /// with the command that populates them. A recipe that asks for
    /// `[sandbox] network = true` contradicts this and is rejected.
    pub offline: bool,
    /// Post-install slimming (strip, cleanup, `-doc`/`-lang` split); see
    /// `slim.rs`. On by default; a recipe can opt out of single passes.
    pub slim: bool,
}


impl Default for BuildOpts {
    fn default() -> Self {
        BuildOpts {
            jobs: 1,
            builddir: PathBuf::from("build"),
            outdir: PathBuf::from("build/out"),
            env: vec![],
            check: CheckMode::Off,
            isolation: Isolation::None,
            timeout_secs: 0,
            allow_network: false,
            allow_degraded: false,
            root: PathBuf::from("/"),
            recipes_root: PathBuf::from("recipes"),
            allow_unsigned: false,
            profile_flags: String::new(),
            use_cache: false,
            offline: false,
            slim: true,
        }
    }
}
