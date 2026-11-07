//! Ecosystem fetch runner (docs/design/fetch-phase.md §7, §11, §12) — Phase 3.
//!
//! For a recipe with `[fetch.go]` this module:
//!
//! 1. plans the fetch round the *downloaded source* (preconditions of §14C:
//!    `go.mod`/`go.sum` exist, no `go.work`/`go.work.sum` anywhere from the
//!    fetch directory up to the workspace root, vendored projects detected
//!    through `vendor/modules.txt`),
//! 2. computes the content-addressed ecosystem key from the lockfile bytes
//!    and maintains `<srccache>/fetch/go/<key>/` entries (§11): a hold-open
//!    `flock` on `<key>.lock` serialises concurrent builds, the entry is
//!    populated in a sibling 0777 temporary directory inside a dedicated
//!    sandbox profile, validated (`go mod verify`), marked with a
//!    completion marker written last and published by an atomic rename,
//! 3. copies the published entry into the per-build view `<work>/fetch/go`
//!    and returns the build-time environment additions (GOMODCACHE,
//!    GOPROXY=off, GOFLAGS, GOTOOLCHAIN, GOWORK, GOCACHE — §7) that the
//!    whitelist joins for this build.
//!
//! Phase scope (§16 delivery plan item 3): the Go backend. `[fetch.cargo]`
//! already parses (§9) and fails here with a clear Phase 4 error. Vendored
//! Go projects skip the fetch entirely and build with `-mod=vendor`.
//!
//! Environment knobs (all documented in §7/§12):
//!
//! * `NAWA_GOPROXY` — module proxy override; `https://` or `file://` only.
//!   A `file://` proxy disables the checksum database (`GOSUMDB=off`), the
//!   hermetic-test clause of the design. A `file://` proxy keeps resolving
//!   through the *filesystem*, so it only reaches a sandboxed fetch when a
//!   test arranges the proxy path inside the sandbox; hermetic integration
//!   tests use `--isolation env`, where the fetch runs as a plain command.
//! * `NAWA_FETCH_TIMEOUT` — wall clock for one fetch run in seconds
//!   (default 1800). Enforced by the sandbox (it kills the PID namespace)
//!   and polled for plain `none`/`env` runs as a second net.
//! * `NAWA_FETCH_PATH` — **test-only**: prepends a PATH element for the
//!   fetch run. The whitelist PATH is `/usr/bin:/usr/sbin`, but a dev host
//!   keeps its toolchain elsewhere (e.g. `/usr/local/go/bin`); production
//!   toolchains are packages and land in `/usr/bin`.
//!
//! None of these reach build sections: they are fetch-run-only values.

use std::collections::BTreeMap;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

use crate::build::BuildOpts;
use crate::recipe::{FetchSpec, Recipe};
use crate::sandbox::spec::ExtraBind;
use crate::sandbox::{BuildPaths, RootSpec, SandboxSpec};

/// How many output lines a failed run quotes.
const FAIL_TAIL_LINES: usize = 30;

/// Fetch-schema version of the Go backend (§10.3): appended to the build id
/// as `fetch-go=<v>` whenever `[fetch.go]` exists, so a change in *how* Nawa
/// fetches invalidates old `--use-cache` sidecars. The lockfile bytes are
/// deliberately NOT an input of the id (§10.3) — the recipe's pinned source
/// digests cover them.
pub const FETCH_GO_SCHEMA_VERSION: &str = "1";
/// Same for `[fetch.cargo]` → `fetch-cargo=<v>`.
pub const FETCH_CARGO_SCHEMA_VERSION: &str = "1";

/// The versioned, unambiguous first field of every ecosystem key (§11).
const FETCH_CACHE_ENCODING: &[u8] = b"nawa-fetch-v1";
/// The completion marker written LAST into a cache entry (§11): a directory
/// without it is never a hit, so an interrupted populate can never be read.
const MARKER: &str = "nawa-complete";
/// §12 fetch profile: 30 s would kill `go`; the timeout is the bound.
const DEFAULT_TIMEOUT_SECS: u64 = 1800;

/// Unique suffix for sibling temporary populate directories.
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Everything `build()` hands to the runner once. Paths are host paths.
pub(crate) struct FetchRun<'a> {
    pub rec: &'a Recipe,
    pub opts: &'a BuildOpts,
    /// The canonical per-build workspace `<builddir>/<name>-<version>-<rel>`.
    pub work: &'a Path,
    /// The workspace root `<work>/src` on the host.
    pub src: &'a Path,
    /// The resolved source cache root (`NAWA_SRCCACHE` or
    /// `<builddir>/distfiles`) — the same value source staging used.
    pub srccache: &'a Path,
    /// Whether build sections run inside the sandbox (`sandbox`/`closure`).
    pub sandboxed: bool,
    /// The build base *inside* the sandbox (pinned `/build`, §4.2); empty
    /// when `!sandboxed`.
    pub build_inside: &'a str,
    /// The HOME value the build whitelist carries (`<base>/home` sandboxed,
    /// `buildenv::SANDBOX_HOME` at `env`): GOCACHE lives under it, so the
    /// Go build cache stays inside the per-build scratch (§7).
    pub whitelist_home: &'a str,
    /// The assembled closure root at `closure` isolation — the fetch
    /// sandbox mounts the same tree (the toolchain lives in it).
    pub assembled: Option<&'a crate::closure::AssembledRoot>,
}

/// Run the ecosystem fetch for one recipe (§7). Returns the build-time
/// environment entries to append to the whitelist (already phrased in the
/// frame the build sees: in-sandbox paths when `sandboxed`). An empty
/// return means "no backend declared" — the caller checks first.
pub(crate) fn run(r: &FetchRun<'_>) -> Result<Vec<(String, String)>> {
    // §8 ships with Phase 4: keep the clear parting error, before any work.
    // (A recipe that declares both tables must fail loudly and cheaply here
    // instead of running a useless Go fetch first.)
    if r.rec.fetch.cargo.is_some() {
        bail!(
            "recipe {} declares [fetch.cargo] — the Cargo backend ships with \
             Phase 4 of the fetch phase (docs/design/fetch-phase.md §8, §16). \
             Remove the table or restrict this recipe to [fetch.go] for now.",
            r.rec.package.name
        );
    }
    let Some(spec) = r.rec.fetch.go.as_ref() else {
        return Ok(Vec::new());
    };

    let plan = plan_go(r, spec)?;
    // The per-build view (§6): <work>/fetch/go. It exists in every mode, an
    // empty dir included — the sandbox binder requires it to exist either way.
    let view_host = r.work.join("fetch").join("go");
    std::fs::create_dir_all(&view_host)
        .with_context(|| format!("create the Go fetch view {}", view_host.display()))?;

    let goflags = if plan.vendored {
        // §7 vendored rule: -mod=vendor (never -mod=readonly, which would
        // send an offline build into the empty module cache)
        "-mod=vendor"
    } else {
        let key = ecosystem_key("go", &[&plan.go_mod, &plan.go_sum])?;
        ensure_go_entry(r, &plan, &key)?;
        let entry = go_cache_dir(r, "go").join(&key);
        copy_into_view(&entry, &view_host).with_context(|| {
            format!("populate the per-build Go fetch view {}", view_host.display())
        })?;
        "-mod=readonly"
    };

    // ---- build-time env (§7) ----------------------------------------------
    // The paths are the ones the build sees: the sandbox view when the build
    // runs inside the sandbox, the host view otherwise. GOCACHE stays under
    // the same HOME the whitelist documents.
    let view = if r.sandboxed {
        format!("{}/fetch/go", r.build_inside.trim_end_matches('/'))
    } else {
        view_host.to_string_lossy().into_owned()
    };
    Ok(vec![
        ("GOMODCACHE".into(), view),
        ("GOPROXY".into(), "off".into()),
        ("GOFLAGS".into(), goflags.into()),
        ("GOTOOLCHAIN".into(), "local".into()),
        ("GOWORK".into(), "off".into()),
        ("GOCACHE".into(), format!("{}/gocache", r.whitelist_home.trim_end_matches('/'))),
    ])
}

/// Precondition checks of §14C plus the vendored detection (§7). All of this
/// happens after source staging, so the files exist when checked.
struct GoPlan {
    /// Host path of the fetch directory.
    dir: PathBuf,
    /// `vendor/modules.txt` exists → build with `-mod=vendor`, no fetch.
    vendored: bool,
    go_mod: Vec<u8>,
    go_sum: Vec<u8>,
}

fn plan_go(r: &FetchRun<'_>, spec: &FetchSpec) -> Result<GoPlan> {
    let dir = if spec.dir == "." || spec.dir.is_empty() {
        r.src.to_path_buf()
    } else {
        r.src.join(&spec.dir)
    };
    if !dir.is_dir() {
        bail!(
            "recipe {}: the [fetch.go] dir `{}` is {}, not a directory in the workspace",
            r.rec.package.name,
            spec.dir,
            dir.display()
        );
    }
    // go.work / go.work.sum anywhere from the fetch dir up to the workspace
    // root is an error — never silently ignored (§7); GOWORK=off additionally
    // keeps a workspace file *outside* the tree from applying.
    let mut scan = dir.clone();
    loop {
        for name in ["go.work", "go.work.sum"] {
            let p = scan.join(name);
            if p.exists() {
                bail!(
                    "recipe {}: {name} is present at {} — a Go workspace file would change \
                     how `go mod download` and the build select modules (fetch-phase.md §7). \
                     Remove it, or fetch a non-workspace layout.",
                    r.rec.package.name,
                    p.display()
                );
            }
        }
        if scan == r.src {
            break;
        }
        match scan.parent() {
            Some(parent) => scan = parent.to_path_buf(),
            None => break,
        }
    }
    for name in ["go.mod", "go.sum"] {
        let p = dir.join(name);
        if !p.is_file() {
            bail!(
                "recipe {}: {name} is missing under {} — a `[fetch.go]` recipe requires it \
                 (fetch-phase.md §7, §14)",
                r.rec.package.name,
                dir.display()
            );
        }
    }
    let vendored = dir.join("vendor").join("modules.txt").is_file();
    let go_mod = std::fs::read(dir.join("go.mod"))
        .with_context(|| format!("read {}", dir.join("go.mod").display()))?;
    let go_sum = std::fs::read(dir.join("go.sum"))
        .with_context(|| format!("read {}", dir.join("go.sum").display()))?;
    Ok(GoPlan { dir, vendored, go_mod, go_sum })
}

/// sha256 over the length-prefixed key fields (§11): `nawa-fetch-v1`, the
/// ecosystem, then the backend's field set. Length-prefixing makes the
/// encoding unambiguous — no separator can be forged by payload bytes.
fn ecosystem_key(eco: &str, fields: &[&[u8]]) -> Result<String> {
    let mut hasher = Sha256::new();
    // fixed header first: the encoding version, then the ecosystem
    for field in [FETCH_CACHE_ENCODING, eco.as_bytes()] {
        hasher.update((field.len() as u64).to_le_bytes());
        hasher.update(field);
    }
    for field in fields {
        hasher.update((field.len() as u64).to_le_bytes());
        hasher.update(field);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// `<srccache>/fetch/<eco>` — the ecosystem cache root (§11).
fn go_cache_dir(r: &FetchRun, eco: &str) -> PathBuf {
    r.srccache.join("fetch").join(eco)
}

/// A cache entry is a hit only with its completion marker (§11).
fn entry_complete(entry: &Path) -> bool {
    entry.join(MARKER).is_file()
}

use std::os::unix::fs::PermissionsExt;

/// Make sure the Go module cache entry `key` is published: marker-free fast
/// path when it exists, otherwise take the entry flock, recheck, populate
/// in a 0777 sibling temporary directory, validate, mark, rename (§11/§12).
fn ensure_go_entry(r: &FetchRun, plan: &GoPlan, key: &str) -> Result<()> {
    let cache = go_cache_dir(r, "go");
    std::fs::create_dir_all(&cache)
        .with_context(|| format!("create the Go fetch cache {}", cache.display()))?;
    let entry = cache.join(key);
    if entry_complete(&entry) {
        return Ok(());
    }
    if crate::fetch::offline() {
        bail!(
            "offline: the Go module cache entry is not populated: {} — run a \
             build once online, or use `nawa fetch` (fetch-phase.md §12)",
            entry.display()
        );
    }
    let lock_path = cache.join(format!("{key}.lock"));
    let _guard = crate::fetch::lock_digest_entry(&lock_path)?;
    // runner-up recheck: the other build may have published while we waited
    if entry_complete(&entry) {
        return Ok(());
    }
    // the populate dir is a private temporary with mode 0777, so the dropped
    // sandbox identity can write it when Nawa runs as root (§12)
    let temp = cache.join(format!(
        "{}.tmp-{}-{}",
        key,
        std::process::id(),
        TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&temp);
    std::fs::create_dir_all(&temp)
        .with_context(|| format!("create {}", temp.display()))?;
    std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o777))?;

    let outcome = run_populate(r, plan, &temp, "fetch-go")
        .and_then(|()| publish_entry(&temp, &entry));
    if let Err(err) = outcome {
        cleanup_failed_populate(&temp);
        return Err(err.context("populating the Go module cache (fetch-phase.md §11)"));
    }
    Ok(())
}

/// `NAWA_GOPROXY` handling (§7): `https://` or `file://`, defaults to the
/// Go proxy *without* a `,direct` fallback — direct would run VCS tools
/// against arbitrary hosts. A `file://` proxy implies `GOSUMDB=off`
/// (the hermetic-test clause).
fn proxy_and_sumdb() -> Result<(String, &'static str)> {
    let proxy = match std::env::var("NAWA_GOPROXY") {
        Ok(v) if !v.trim().is_empty() => {
            let v = v.trim().to_string();
            if !(v.starts_with("https://") || v.starts_with("file://")) {
                bail!(
                    "NAWA_GOPROXY must be an `https://` or `file://` URL \
                     (fetch-phase.md §7), got `{v}`"
                );
            }
            v
        }
        _ => "https://proxy.golang.org".to_string(),
    };
    let sumdb = if proxy.starts_with("file://") { "off" } else { "sum.golang.org" };
    Ok((proxy, sumdb))
}

/// §12: `NAWA_FETCH_TIMEOUT`, default 1800. Invalid values fail loud.
fn fetch_timeout_secs() -> Result<u64> {
    match std::env::var("NAWA_FETCH_TIMEOUT") {
        Ok(v) if !v.trim().is_empty() => v
            .trim()
            .parse::<u64>()
            .with_context(|| format!("NAWA_FETCH_TIMEOUT \"{v}\" is not a second count")),
        _ => Ok(DEFAULT_TIMEOUT_SECS),
    }
}

/// The fetch PATH (§7): the whitelist PATH; `NAWA_FETCH_PATH` can prepend
/// a test-only element on dev hosts whose toolchain lives elsewhere.
fn fetch_path() -> String {
    match std::env::var("NAWA_FETCH_PATH") {
        Ok(p) if !p.trim().is_empty() => format!("{}:/usr/bin:/usr/sbin", p.trim()),
        _ => "/usr/bin:/usr/sbin".to_string(),
    }
}

/// The §7 fetch allowlist, parameterised for the run flavor. The build
/// environment (§7 bottom list) is separate — see [`run`].
fn go_fetch_env(
    proxy: &str,
    sumdb: &str,
    home: &str,
    gomodcache: &str,
) -> Vec<(String, String)> {
    vec![
        ("PATH".into(), fetch_path()),
        ("TMPDIR".into(), "/tmp".into()),
        ("LC_ALL".into(), "C".into()),
        ("TZ".into(), "UTC".into()),
        ("HOME".into(), home.into()),
        ("GOPATH".into(), home.into()),
        ("GOTOOLCHAIN".into(), "local".into()),
        ("GOFLAGS".into(), "-mod=readonly -modcacherw".into()),
        ("GOWORK".into(), "off".into()),
        ("GOMODCACHE".into(), gomodcache.into()),
        ("GOPROXY".into(), proxy.into()),
        ("GOSUMDB".into(), sumdb.into()),
    ]
}

/// The fixed commands of the Go backend (§7), run in the fetch directory.
const GO_POPULATE_SCRIPT: &str = "go mod download && go mod verify";

/// Run [`GO_POPULATE_SCRIPT`] against the temporary entry directory,
/// sandboxed at `sandbox`/`closure` (the §12 fetch profile: strict read-only
/// root, dropped identity when root, `network = true` regardless of
/// `--allow-network`/`[sandbox] network`, the per-profile rlimits), as a
/// plain cleared-environment command at `none`/`env` (§12: no isolation,
/// never to be described as network-isolated).
fn run_populate(r: &FetchRun, plan: &GoPlan, temp: &Path, label: &str) -> Result<()> {
    let (proxy, sumdb) = proxy_and_sumdb()?;
    let timeout_secs = fetch_timeout_secs()?;
    let log_path = r.work.join(format!("{label}.log"));
    let script = format!("exec 2>&1\n{GO_POPULATE_SCRIPT}");

    let status = if r.sandboxed {
        let base = r.build_inside.trim_end_matches('/');
        // the fetch scratch is the per-build home bind; the temporary entry
        // is a writable extra bind at <base>/fetch-out (§12)
        let env: BTreeMap<String, String> =
            go_fetch_env(&proxy, sumdb, &format!("{base}/home"), &format!("{base}/fetch-out"))
                .into_iter()
                .collect();
        let spec = fetch_sandbox_spec(r, env, &script, plan, temp, timeout_secs)?;
        let spec_path = r.work.join("fetch-sandbox-spec.toml");
        std::fs::write(&spec_path, toml::to_string_pretty(&spec)?)
            .with_context(|| format!("write {}", spec_path.display()))?;
        let exe = std::env::current_exe().context("locate the nawa binary")?;
        let (mut child, sock) = crate::sandbox::spawn_init(&exe, &spec_path)
            .context("spawn nawa sandbox-init for the fetch run")?;
        // install the ranged uid map when we are root, then release the child
        crate::sandbox::parent_sync(&child, sock);
        stream_and_wait(&mut child, &log_path, timeout_secs, label)
    } else {
        // no sandbox (§12): the host toolchain, a cleared environment, the
        // per-build scratch as HOME
        let home_host = r.work.join("sb").join("home");
        std::fs::create_dir_all(&home_host)
            .with_context(|| format!("create {}", home_host.display()))?;
        let envs = go_fetch_env(&proxy, sumdb, &home_host.to_string_lossy(), &temp.to_string_lossy());
        let mut cmd = std::process::Command::new("bash");
        cmd.args(["-e", "-c", GO_POPULATE_SCRIPT])
            .current_dir(&plan.dir)
            .env_clear()
            .envs(envs.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .stdin(Stdio::null())
            .stdout(Stdio::piped());
        let mut child = cmd.spawn().context("failed to spawn the fetch run")?;
        stream_and_wait(&mut child, &log_path, timeout_secs, label)
    }?;
    if !status.success() {
        bail!("fetch run ({label}) failed with status {status} — log: {}", log_path.display());
    }
    Ok(())
}

/// The sandbox spec of one fetch run (§12). Clones the shape of the build
/// spec (same staging binds, same mask) and then hardens it: strict
/// read-only root, read-only src, a writable `/tmp` + home + fetch-out,
/// real /etc/resolv.conf + /etc/hosts copies bound read-only, the fetch
/// profile limits, network always on.
fn fetch_sandbox_spec(
    r: &FetchRun,
    env: BTreeMap<String, String>,
    script: &str,
    plan: &GoPlan,
    temp: &Path,
    timeout_secs: u64,
) -> Result<SandboxSpec> {
    let base = r.build_inside;
    let scratch = r.work.join("sb");
    let home = scratch.join("home");
    std::fs::create_dir_all(&home)
        .with_context(|| format!("create the fetch scratch home {}", home.display()))?;

    // host paths to hide in live mode (§4.2), as the build spec does
    let mut mask: Vec<String> = [
        "/mydistro",
        "/opt/seed",
        "/root",
        "/home",
        "/mnt",
        "/var/lib/nawa",
        "/var/cache/nawa",
        "/run",
    ]
    .into_iter()
    .map(String::from)
    .collect();
    if let Some(bb) = r.work.parent() {
        let bbs = bb.to_string_lossy().into_owned();
        if bbs != r.build_inside {
            mask.push(bbs);
        }
    }

    // §12: the working directory of the fixed command is the fetch dir,
    // seen through the staging bind
    let rel = plan.dir.strip_prefix(r.src).unwrap_or(Path::new(""));
    let workdir = format!("{}/src/{}", base.trim_end_matches('/'), rel.display())
        .trim_end_matches('/')
        .to_string();

    let mut extra_binds = vec![ExtraBind {
        // the temporary entry being populated; published after the run
        host: temp.to_string_lossy().into_owned(),
        inside: format!("{}/fetch-out", base.trim_end_matches('/')),
        readonly: false,
        kind: "dir".into(),
    }];
    extra_binds.extend(resolver_binds(r)?);

    Ok(SandboxSpec {
        version: 1,
        root: if r.opts.isolation == crate::buildenv::Isolation::Closure {
            RootSpec {
                mode: "dir".into(),
                dir: r.work.join("buildroot").to_string_lossy().into_owned(),
                layers: r
                    .assembled
                    .map(|a| a.layer_dirs.iter().map(|p| p.to_string_lossy().into_owned()).collect())
                    .unwrap_or_default(),
            }
        } else {
            RootSpec::default()
        },
        build: BuildPaths {
            src_host: r.src.to_string_lossy().into_owned(),
            pkg_host: r.work.join("pkg").to_string_lossy().into_owned(),
            home_host: home.to_string_lossy().into_owned(),
            scratch: scratch.to_string_lossy().into_owned(),
        },
        env,
        script: script.to_string(),
        timeout_secs,
        network: true,
        sysfs: false,
        allow_degraded: r.opts.allow_degraded,
        mask,
        build_inside: base.to_string(),
        workdir,
        // §12: drop privileges for the fetch process when we are root
        run_uid: privileged().then_some(65534),
        run_gid: privileged().then_some(65534),
        memory_limit_bytes: 0,     // RLIMIT_AS breaks Go; the timeout is the bound
        process_limit: 4096,       // threads count
        file_size_limit_bytes: 0,  // a module may exceed any small cap
        open_files_limit: 4096,    // parallel downloads
        readonly_paths: vec![format!("{}/src", base.trim_end_matches('/'))],
        extra_binds,
        strict_readonly_root: true,
        // /tmp (fresh tmpfs), the home bind and the populate dir stay
        // writable; strict mode remounts everything else read-only
        writable_paths: vec![
            "/tmp".to_string(),
            format!("{}/home", base.trim_end_matches('/')),
            format!("{}/fetch-out", base.trim_end_matches('/')),
        ],
    })
}

/// §12: Nawa follows the symlink chain of the host's `/etc/resolv.conf`,
/// copies its content together with `/etc/hosts` into `<work>/fetch/etc/`,
/// then binds the copies read-only as `file` binds — binding the host path
/// directly is wrong twice over (no `/etc/resolv.conf` in an assembled
/// closure root; systemd-resolved's path is a symlink into the masked
/// `/run`). Missing files are skipped with a warning.
fn resolver_binds(r: &FetchRun) -> Result<Vec<ExtraBind>> {
    let etc = r.work.join("fetch").join("etc");
    std::fs::create_dir_all(&etc)
        .with_context(|| format!("create {}", etc.display()))?;
    let mut out = Vec::new();
    for name in ["resolv.conf", "hosts"] {
        let host = Path::new("/etc").join(name);
        let Ok(content) = std::fs::read(&host) else {
            eprintln!(
                "nawa: fetch: warning: {} is missing on the host — DNS for the \
                 fetch run may fail if the sandbox root does not provide it",
                host.display()
            );
            continue;
        };
        let copy = etc.join(name);
        std::fs::write(&copy, &content)
            .with_context(|| format!("copy {} to {}", host.display(), copy.display()))?;
        out.push(ExtraBind {
            host: copy.to_string_lossy().into_owned(),
            inside: format!("/etc/{name}"),
            readonly: true,
            kind: "file".into(),
        });
    }
    Ok(out)
}

fn privileged() -> bool {
    rustix::process::geteuid().is_root()
}

/// Publication (§11): normalise ownership (the sandbox identity may have
/// written the entry) and strip write permission (dirs 0555, files 0444,
/// symlinks untouched), then write the completion marker LAST, normalise
/// the marked root itself and rename the sibling into place atomically.
fn publish_entry(temp: &Path, entry: &Path) -> Result<()> {
    let euid = rustix::process::geteuid().as_raw();
    let egid = rustix::process::getegid().as_raw();
    use std::os::unix::fs::PermissionsExt;

    let normalise = |p: &Path, is_dir: bool| -> Result<()> {
        let md = std::fs::symlink_metadata(p).with_context(|| format!("stat {}", p.display()))?;
        if md.file_type().is_symlink() {
            return Ok(());
        }
        let _ = std::os::unix::fs::chown(p, Some(euid), Some(egid));
        let mode = if is_dir { 0o555 } else { 0o444 };
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode))
            .with_context(|| format!("chmod {:o} {}", mode, p.display()))?;
        Ok(())
    };

    // children first, every one of them
    for e in WalkDir::new(temp).into_iter().filter_map(|e| e.ok()) {
        if e.depth() == 0 {
            continue;
        }
        // WalkDir without follow_links yields symlink entries as files;
        // normalise() ignores them like grant_source_write does
        normalise(e.path(), e.file_type().is_dir())?;
    }
    // the marker is written LAST: a reader before it either sees no entry
    // (rename pending) or an incomplete temp dir it must never hit (§11)
    let marker = temp.join(MARKER);
    std::fs::write(&marker, b"").with_context(|| format!("write {}", marker.display()))?;
    normalise(&marker, false)?;
    normalise(temp, true)?;

    // a stale unlocked leftover cannot exist under our lock, but be strict
    // about garbage from a pre-lock crash: it is never a valid entry
    if entry.exists() {
        cleanup_failed_populate(entry);
    }
    std::fs::rename(temp, entry)
        .with_context(|| format!("publish {} -> {}", temp.display(), entry.display()))?;
    Ok(())
}

/// A failed/needs-removal populate dir or stale entry: restore write
/// permission over the tree (publication strips it) so the removal can
/// actually delete children, then remove. Best effort — cleanup that
/// failed to clean is a cache-hygiene smell, not a build failure.
fn cleanup_failed_populate(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755));
    for e in WalkDir::new(dir).into_iter().flatten() {
        if e.file_type().is_symlink() {
            continue;
        }
        let mode = if e.file_type().is_dir() { 0o755 } else { 0o644 };
        let _ = std::fs::set_permissions(e.path(), std::fs::Permissions::from_mode(mode));
    }
    let _ = std::fs::remove_dir_all(dir);
}

/// Copy a published cache entry into the per-build view (§11: never
/// hard-linked — a write through the view must not reach the shared entry).
/// The copy is normalised writable: dirs 0755, files 0644 (a published
/// entry is 0444/0555, and `fs::copy` would carry those bits).
fn copy_into_view(entry: &Path, view: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    for e in WalkDir::new(entry).into_iter().filter_map(|e| e.ok()) {
        let rel = e.path().strip_prefix(entry).unwrap_or(Path::new(""));
        // the publication marker is cache metadata, not module-cache
        // content (§11): it stays in the entry, never in the build view
        if rel == Path::new(MARKER) {
            continue;
        }
        let dst = view.join(rel);
        let ft = e.file_type();
        if ft.is_dir() {
            std::fs::create_dir_all(&dst)
                .with_context(|| format!("create view dir {}", dst.display()))?;
        } else if ft.is_symlink() {
            let link = std::fs::read_link(e.path())
                .with_context(|| format!("read link {}", e.path().display()))?;
            let _ = std::os::unix::fs::symlink(&link, &dst);
        } else {
            std::fs::copy(e.path(), &dst)
                .with_context(|| format!("copy {} -> {}", e.path().display(), dst.display()))?;
            let md = std::fs::symlink_metadata(e.path())?;
            let mode = md.permissions().mode() & 0o7777;
            // strip the read-only bits the publication stamped on
            let want = if mode & 0o111 != 0 { mode | 0o222 | 0o755 } else { mode | 0o222 | 0o644 };
            std::fs::set_permissions(&dst, std::fs::Permissions::from_mode(want))
                .with_context(|| format!("mode {}", dst.display()))?;
        }
    }
    Ok(())
}

/// Stream a fetch child's combined stdout (both run flavors exec
/// `exec 2>&1` first) to the console and to `<work>/<label>.log`, quoting
/// the last lines on failure. Enforces the wall clock by polling: a plain
/// run has no supervisor, and a sandboxed run gets the same second net
/// (the sandbox itself kills the PID namespace, §12).
fn stream_and_wait(
    child: &mut Child,
    log_path: &Path,
    timeout_secs: u64,
    label: &str,
) -> Result<ExitStatus> {
    let mut log = std::fs::File::create(log_path)
        .with_context(|| format!("create {}", log_path.display()))?;

    // the reader thread owns the pipe so the main thread can poll the
    // child's status while output keeps flowing
    let stdout: ChildStdout = child.stdout.take().context("no stdout pipe")?;
    let tail = std::sync::Arc::new(std::sync::Mutex::new(
        std::collections::VecDeque::<String>::with_capacity(FAIL_TAIL_LINES + 1),
    ));
    let tail_writer = std::sync::Arc::clone(&tail);
    let reader = std::thread::spawn(move || {
        let mut r = std::io::BufReader::new(stdout);
        let mut buf = Vec::new();
        // read_until is tolerant of non-UTF-8 bytes and of very long lines
        while r.read_until(b'\n', &mut buf).unwrap_or(0) > 0 {
            let _ = log.write_all(&buf);
            {
                let mut out = std::io::stdout().lock();
                let _ = out.write_all(&buf);
                let _ = out.flush();
            }
            let mut t = tail_writer.lock().unwrap();
            if t.len() == FAIL_TAIL_LINES {
                t.pop_front();
            }
            t.push_back(String::from_utf8_lossy(&buf).trim_end().to_string());
            buf.clear();
        }
    });

    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    loop {
        if let Some(status) = child.try_wait().context("waiting for the fetch run")? {
            reader.join().map_err(|_| anyhow::Error::msg("fetch log reader crashed"))?;
            let t: Vec<String> = tail.lock().unwrap().drain(..).collect();
            if !status.success() {
                bail!(
                    "{label} failed with status {status}\n--- last output ---\n{}",
                    t.join("\n")
                );
            }
            return Ok(status);
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            reader.join().map_err(|_| anyhow::Error::msg("fetch log reader crashed"))?;
            let t: Vec<String> = tail.lock().unwrap().drain(..).collect();
            bail!(
                "{label} exceeded NAWA_FETCH_TIMEOUT ({timeout_secs}s) and was killed\n\
                 --- last output ---\n{}",
                t.join("\n")
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}