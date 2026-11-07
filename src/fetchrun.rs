//! Ecosystem fetch runner (docs/design/fetch-phase.md §7, §8, §11, §12).
//!
//! For a recipe with `[fetch.go]` / `[fetch.cargo]` this module:
//!
//! 1. plans the fetch round the *downloaded source* (preconditions of §14C:
//!    for Go `go.mod`/`go.sum` exist and no `go.work`/`go.work.sum` anywhere
//!    from the fetch directory up to the workspace root; for Cargo
//!    `Cargo.toml`/`Cargo.lock` exist and the source-tree Cargo config obeys
//!    the §8 v1 rules), detecting vendored layouts (`vendor/modules.txt`,
//!    a `crates-io` replacement by an in-tree `directory` source),
//! 2. computes the content-addressed ecosystem key over the backend's §11
//!    field set (Go: `go.mod`+`go.sum` bytes; Cargo: `Cargo.lock` bytes plus
//!    the `cargo --version` string) and maintains
//!    `<srccache>/fetch/<eco>/<key>/` entries: a hold-open `flock` on
//!    `<key>.lock` serialises concurrent builds, the entry is populated in a
//!    sibling 0777 temporary directory inside a dedicated sandbox profile,
//!    validated (`go mod verify`; `cargo fetch --locked` is its own
//!    validation), marked with a completion marker written last and
//!    published by an atomic rename,
//! 3. copies the published entry into the per-build view `<work>/fetch/<eco>`
//!    (never hard-linked: Cargo may write `.package-cache` even offline, §8)
//!    and returns the build-time environment additions that the whitelist
//!    joins for this build:
//!    * Go (§7): GOMODCACHE, GOPROXY=off, GOFLAGS, GOTOOLCHAIN, GOWORK,
//!      GOCACHE,
//!    * Cargo (§8): CARGO_HOME, CARGO_NET_OFFLINE=true.
//!
//! Vendored projects skip the fetch entirely and build as today
//! (`-mod=vendor`, the in-tree directory source).
//!
//! Environment knobs (all documented in §7/§8/§12):
//!
//! * `NAWA_GOPROXY` — Go module proxy override; `https://` or `file://`
//!   only. A `file://` proxy disables the checksum database (`GOSUMDB=off`),
//!   the hermetic-test clause of the design. A `file://` proxy keeps
//!   resolving through the *filesystem*, so it only reaches a sandboxed
//!   fetch when a test arranges the proxy path inside the sandbox; hermetic
//!   integration tests use `--isolation env`, where the fetch runs as a
//!   plain command. Cargo has no such override: in v1 the source-tree
//!   *registry* is fixed by the scan rules of §8 (redirects are errors), so
//!   a hermetic real `cargo fetch` exercises git dependencies instead.
//! * `NAWA_FETCH_TIMEOUT` — wall clock for one fetch run in seconds
//!   (default 1800). Enforced by the sandbox (it kills the PID namespace)
//!   and polled for plain `none`/`env` runs as a second net.
//! * `NAWA_FETCH_PATH` — **test-only**: prepends a PATH element for the
//!   fetch run. The whitelist PATH is `/usr/bin:/usr/sbin`, but a dev host
//!   keeps its toolchain elsewhere (e.g. `/usr/local/go/bin`,
//!   `~/.cargo/bin`); production toolchains are packages and land in
//!   `/usr/bin`.
//!
//! None of these reach build sections: they are fetch-run-only values.

use std::collections::BTreeMap;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, ExitStatus, Stdio};
use std::os::unix::process::CommandExt;
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

/// Run the ecosystem fetch for one recipe (§7, §8). Returns the build-time
/// environment entries to append to the whitelist (already phrased in the
/// frame the build sees: in-sandbox paths when `sandboxed`). Both tables may
/// coexist; the returned additions concatenate in table order. An empty
/// return means "no backend declared" — the caller checks first.
pub(crate) fn run(r: &FetchRun<'_>) -> Result<Vec<(String, String)>> {
    let mut env = Vec::new();
    if let Some(spec) = r.rec.fetch.go.as_ref() {
        env.extend(run_go(r, spec)?);
    }
    if let Some(spec) = r.rec.fetch.cargo.as_ref() {
        env.extend(run_cargo(r, spec)?);
    }
    Ok(env)
}

/// The Go backend (§7): plan, populate or hit, then expose the module cache.
fn run_go(r: &FetchRun<'_>, spec: &FetchSpec) -> Result<Vec<(String, String)>> {
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
        let entry = eco_cache_dir(r, "go").join(&key);
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

// ===========================================================================
// §8 Cargo backend
// ===========================================================================

/// The §8 precondition + detection result. Everything is checked around the
/// *downloaded source*, after staging.
struct CargoPlan {
    /// Host path of the fetch directory (the crate/workspace root).
    dir: PathBuf,
    /// The tree replaces the crates-io registry with an in-tree `directory`
    /// source that exists: skip the fetch, build as today (§8).
    vendored: bool,
    /// The bytes of the mandatory `Cargo.lock` — a key input (§11) and the
    /// source of the git-dependency warning (§8).
    lock: Vec<u8>,
    /// `source = "git+…"` values of the lock: warned about (§8/§10.2).
    git_deps: Vec<String>,
}

fn plan_cargo(r: &FetchRun<'_>, spec: &FetchSpec) -> Result<CargoPlan> {
    let dir = if spec.dir == "." || spec.dir.is_empty() {
        r.src.to_path_buf()
    } else {
        r.src.join(&spec.dir)
    };
    if !dir.is_dir() {
        bail!(
            "recipe {}: the [fetch.cargo] dir `{}` is {}, not a directory in the workspace",
            r.rec.package.name,
            spec.dir,
            dir.display()
        );
    }
    // §8: Cargo.lock is mandatory (checksums for registry crates, commit
    // pins for git ones); the manifest feeds --manifest-path itself.
    for name in ["Cargo.toml", "Cargo.lock"] {
        let p = dir.join(name);
        if !p.is_file() {
            bail!(
                "recipe {}: {name} is missing under {} — a `[fetch.cargo]` recipe \
                 requires it (fetch-phase.md §8)",
                r.rec.package.name,
                dir.display()
            );
        }
    }
    // §8 source-tree config rules: any [source.*] other than the
    // crates-io → in-tree directory replacement, and any [registries],
    // [http], [net] setting, is refused in v1.
    let vendored = scan_cargo_configs(r, &dir)?;
    let lock = std::fs::read(dir.join("Cargo.lock"))
        .with_context(|| format!("read {}", dir.join("Cargo.lock").display()))?;
    let text = std::str::from_utf8(&lock)
        .map_err(anyhow::Error::from)
        .with_context(|| format!("recipe {}: Cargo.lock is not UTF-8", r.rec.package.name))?;
    let git_deps = text
        .lines()
        .filter_map(|l| {
            let t = l.trim();
            t.strip_prefix("source = \"git+").map(|s| s.trim_end_matches('"').to_string())
        })
        .collect();
    Ok(CargoPlan { dir, vendored, lock, git_deps })
}

/// The §8 config scan: from `dir` up to the workspace root, every
/// `.cargo/config.toml` / `.cargo/config` that Cargo itself would load gets
/// the v1 rules. Returns whether ANY of them makes the tree vendored.
fn scan_cargo_configs(r: &FetchRun, dir: &Path) -> Result<bool> {
    let mut vendored = false;
    let mut scan = dir.to_path_buf();
    loop {
        for name in [".cargo/config.toml", ".cargo/config"] {
            let cfg = scan.join(name);
            // a config cargo could not even read is also invisible to it
            let Ok(text) = std::fs::read_to_string(&cfg) else {
                continue;
            };
            if cargo_config_is_vendored(r, &cfg, &text)? {
                vendored = true;
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
    Ok(vendored)
}

/// The v1 rules for ONE config file (§8):
///
/// * `[source.crates-io]` may only `replace-with` a `[source.X]` chain that
///   ends in a `directory` source **inside the downloaded tree** → vendored.
/// * any other `[source.*]` shape — a registry redirect, a directory that
///   does not exist or escapes the tree, unused sources — is an error.
/// * `[registries]`, `[http]`, `[net]` are refused.
fn cargo_config_is_vendored(
    r: &FetchRun,
    cfg: &Path,
    text: &str,
) -> Result<bool> {
    let name = &r.rec.package.name;
    // relative directory values resolve against the parent of the `.cargo`
    // directory that holds the config (probed: independent of the cwd)
    let base = match cfg.parent().and_then(Path::parent) {
        Some(b) => b.to_path_buf(),
        None => return Ok(false),
    };
    let cfg = cfg.display();
    let value: toml::Value = toml::from_str(text).map_err(|e| {
        anyhow::Error::msg(format!(
            "recipe {name}: {cfg} is not valid TOML ({e}); the §8 config rules \
             cannot evaluate what cargo would load"
        ))
    })?;

    for table in ["registries", "http", "net"] {
        if value.get(table).is_some() {
            bail!(
                "recipe {name}: {cfg} sets [{table}] — refused in v1 because it changes \
                 how Cargo reaches the registry (fetch-phase.md §8). Remove it from the \
                 downloaded tree."
            );
        }
    }

    let Some(sources) = value.get("source").and_then(|v| v.as_table()) else {
        return Ok(false);
    };

    let mut referenced: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    referenced.insert("crates-io".into());

    if let Some(crates_io) = sources.get("crates-io").and_then(|v| v.as_table()) {
        if let Some(evil) = crates_io.keys().find(|k| *k != "replace-with") {
            bail!(
                "recipe {name}: {cfg} redirects [source.crates-io] through `{evil}` — a \
                 registry redirect is refused in v1 (fetch-phase.md §8); the only allowed \
                 replacement is an in-tree `directory` source."
            );
        }
        let Some(toml::Value::String(target)) = crates_io.get("replace-with") else {
            bail!(
                "recipe {name}: {cfg}: [source.crates-io] must carry only the \
                 replace-with chain to an in-tree directory source (fetch-phase.md §8)"
            );
        };
        let mut cur = target.clone();
        loop {
            // every hop must appear exactly once in the chain (no cycles)
            if !referenced.insert(cur.clone()) {
                bail!(
                    "recipe {name}: {cfg}: the source replacement chain revisits `{cur}` \
                     (fetch-phase.md §8)"
                );
            }
            match sources.get(&cur) {
                Some(toml::Value::Table(hop)) => {
                    if hop.get("registry").is_some() {
                        bail!(
                            "recipe {name}: {cfg} redirects through the registry source \
                             `{cur}` — refused in v1 (fetch-phase.md §8)"
                        );
                    }
                    if let Some(toml::Value::String(d)) = hop.get("directory") {
                        if hop.keys().count() > 1 {
                            bail!(
                                "recipe {name}: {cfg}: the source `{cur}` mixes `directory` \
                                 with other keys (fetch-phase.md §8)"
                            );
                        }
                        // cargo resolves it against `base` (§8 vendored rule:
                        // the directory must exist *in the tree*)
                        let resolved = base.join(d);
                        let inside = if resolved.strip_prefix(r.src).is_ok() {
                            resolved.is_dir()
                        } else {
                            false
                        };
                        if !inside {
                            bail!(
                                "recipe {name}: {cfg} replaces crates-io with the directory \
                                 source `{cur}` pointing at {} — it must be an existing \
                                 directory inside the downloaded tree (fetch-phase.md §8)",
                                resolved.display()
                            );
                        }
                        return Ok(true);
                    }
                    if let Some(toml::Value::String(next)) = hop.get("replace-with") {
                        cur = next.clone();
                        continue;
                    }
                    bail!(
                        "recipe {name}: {cfg}: the source `{cur}` is neither a directory \
                         nor a replacement chain (fetch-phase.md §8)"
                    );
                }
                _ => bail!(
                    "recipe {name}: {cfg} replaces crates-io with `{cur}`, which is not \
                     defined as a source table in this config (fetch-phase.md §8)"
                ),
            }
        }
    }

    // any [source.X] outside the (absent) crates-io chain is an error
    for id in sources.keys() {
        if id == "crates-io" {
            continue;
        }
        if !referenced.contains(id) {
            bail!(
                "recipe {name}: {cfg} defines the source `{id}` — any [source.*] other \
                 than the crates-io → in-tree directory replacement is refused in v1 \
                 (fetch-phase.md §8)"
            );
        }
    }
    Ok(false)
}

/// The `[fetch.cargo]` backend (§8): plan, populate-or-hit, expose the
/// Cargo home. Build-time additions: `CARGO_HOME` (the per-build view)
/// and `CARGO_NET_OFFLINE=true` — recipes keep their own commands, e.g.
/// `cargo build --release --locked --offline`.
fn run_cargo(r: &FetchRun<'_>, spec: &FetchSpec) -> Result<Vec<(String, String)>> {
    let plan = plan_cargo(r, spec)?;
    // The per-build view (§6): <work>/fetch/cargo. It exists for every mode,
    // vendored ones included — the sandbox binder requires it either way.
    let view_host = r.work.join("fetch").join("cargo");
    std::fs::create_dir_all(&view_host)
        .with_context(|| format!("create the Cargo fetch view {}", view_host.display()))?;

    if !plan.vendored {
        // §11: the key covers the Cargo.lock bytes and the Cargo version
        // string (registry directory names embed a hash that changes
        // between Cargo releases). The version query runs one cheap
        // `cargo --version` in the same execution context as the fetch.
        let version = cargo_version_query(r, &plan)?;
        let key = ecosystem_key("cargo", &[&plan.lock, version.as_bytes()])?;
        ensure_cargo_entry(r, &plan, &key)?;
        let entry = eco_cache_dir(r, "cargo").join(&key);
        copy_into_view(&entry, &view_host).with_context(|| {
            format!("populate the per-build Cargo fetch view {}", view_host.display())
        })?;
    }

    // §8 answers cache mutation with the per-build copy (§11: never
    // hard-linked): `.package-cache`, `.global-cache` and friends are
    // written into the view — disposable storage — while the published
    // entry stays pristine. Nothing else is needed at build time: the
    // vendored case reads its crates straight from the tree.
    let view = if r.sandboxed {
        format!("{}/fetch/cargo", r.build_inside.trim_end_matches('/'))
    } else {
        view_host.to_string_lossy().into_owned()
    };
    Ok(vec![
        ("CARGO_HOME".into(), view),
        ("CARGO_NET_OFFLINE".into(), "true".into()),
    ])
}

/// Populated-or-hit discipline for a Cargo key (§11), mirroring
/// [`ensure_go_entry`]: marker fast path, offline guard before anything is
/// created, hold-open flock, 0777 sibling temporary, publish, cleanup.
fn ensure_cargo_entry(r: &FetchRun, plan: &CargoPlan, key: &str) -> Result<()> {
    let cache = eco_cache_dir(r, "cargo");
    let entry = cache.join(key);
    if entry_complete(&entry) {
        return Ok(());
    }
    if crate::fetch::offline() {
        bail!(
            "offline: the Cargo cache entry is not populated: {} — run a build \
             once online, or use `nawa fetch` (fetch-phase.md §8, §12)",
            entry.display()
        );
    }
    std::fs::create_dir_all(&cache)
        .with_context(|| format!("create the Cargo fetch cache {}", cache.display()))?;
    let lock_path = cache.join(format!("{key}.lock"));
    let _guard = crate::fetch::lock_digest_entry(&lock_path)?;
    // runner-up recheck: the other build may have published while we waited
    if entry_complete(&entry) {
        return Ok(());
    }
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

    let outcome = run_populate_cargo(r, plan, &temp)
        .and_then(|()| publish_entry(&temp, &entry));
    if let Err(err) = outcome {
        cleanup_failed_populate(&temp);
        return Err(err.context("populating the Cargo fetch cache (fetch-phase.md §8, §11)"));
    }
    Ok(())
}

/// The fixed command of the Cargo backend (§8): `--locked` must fail rather
/// than update the lock (Cargo's own contract), and no `--target` filter —
/// v1 fetches all targets of the lockfile.
const CARGO_POPULATE_SCRIPT: &str = "cargo fetch --locked --manifest-path \"$NAWA_CARGO_MANIFEST\"";

/// §8: git dependencies are pinned by commit but carry no registry
/// checksums, and `cargo fetch` invokes `git` itself. Warn at fetch time,
/// before the populate actually runs.
const CARGO_VERSION_SCRIPT: &str = "cargo --version > \"$NAWA_CARGO_VER_OUT\"";

fn run_populate_cargo(r: &FetchRun, plan: &CargoPlan, temp: &Path) -> Result<()> {
    if !plan.git_deps.is_empty() {
        eprintln!(
            "nawa: fetch: warning: Cargo.lock uses git dependencies without registry \
             checksums ({}); `cargo fetch` needs git in the fetch root, and such \
             dependencies are verified only by their pinned commit (fetch-phase.md §8, §10.2)",
            plan.git_deps.join(", ")
        );
    }
    let timeout_secs = fetch_timeout_secs()?;
    let label = "fetch-cargo";
    let log_path = r.work.join(format!("{label}.log"));
    let script = format!("exec 2>&1\n{CARGO_POPULATE_SCRIPT}");
    let manifest_rel = plan.dir.strip_prefix(r.src).unwrap_or(Path::new("")).join("Cargo.toml");

    let mut child = if r.sandboxed {
        let base = r.build_inside.trim_end_matches('/');
        let env = [
            cargo_fetch_base_env(&format!("{base}/home")),
            vec![
                ("CARGO_HOME".into(), format!("{base}/fetch-out")),
                (
                    "NAWA_CARGO_MANIFEST".into(),
                    format!("{}/src/{}", base.trim_end_matches('/'), manifest_rel.display()),
                ),
            ],
        ]
        .concat();
        spawn_fetch_child(r, &script, env, &plan.dir, Some(temp), timeout_secs)?
    } else {
        let home_host = r.work.join("sb").join("home");
        std::fs::create_dir_all(&home_host)
            .with_context(|| format!("create {}", home_host.display()))?;
        let env = [
            cargo_fetch_base_env(&home_host.to_string_lossy()),
            vec![
                ("CARGO_HOME".into(), temp.to_string_lossy().into_owned()),
                (
                    "NAWA_CARGO_MANIFEST".into(),
                    plan.dir
                        .join("Cargo.toml")
                        .to_string_lossy()
                        .into_owned(),
                ),
            ],
        ]
        .concat();
        spawn_fetch_child(r, &script, env, &plan.dir, None, timeout_secs)?
    };

    let status = stream_and_wait(&mut child, Some(&log_path), timeout_secs, label, !r.sandboxed)?;
    if !status.success() {
        bail!("fetch run ({label}) failed with status {status} — log: {}", log_path.display());
    }
    Ok(())
}

/// One cheap `cargo --version` in the fetch context (§11: the version
/// string enters the ecosystem key). The script writes the version into
/// the fetch-out bind (sandbox flavor) or the scratch dir (plain flavor);
/// the caller reads it back from the host side.
fn cargo_version_query(r: &FetchRun, plan: &CargoPlan) -> Result<String> {
    let timeout_secs = fetch_timeout_secs()?;
    let label = "fetch-cargo-version";
    let out_dir = r.work.join("fetch").join("verq");
    std::fs::create_dir_all(&out_dir)
        .with_context(|| format!("create {}", out_dir.display()))?;
    let out_host = out_dir.join("version");
    let script = format!("exec 2>&1\n{CARGO_VERSION_SCRIPT}");

    let mut child = if r.sandboxed {
        let base = r.build_inside.trim_end_matches('/');
        let env = [
            cargo_fetch_base_env(&format!("{base}/home")),
            vec![(
                "NAWA_CARGO_VER_OUT".into(),
                format!("{base}/fetch-out/version"),
            )],
        ]
        .concat();
        spawn_fetch_child(r, &script, env, &plan.dir, Some(&out_dir), timeout_secs)?
    } else {
        let home_host = r.work.join("sb").join("home");
        std::fs::create_dir_all(&home_host)
            .with_context(|| format!("create {}", home_host.display()))?;
        let env = [
            cargo_fetch_base_env(&home_host.to_string_lossy()),
            vec![(
                "NAWA_CARGO_VER_OUT".into(),
                out_host.to_string_lossy().into_owned(),
            )],
        ]
        .concat();
        spawn_fetch_child(r, &script, env, &plan.dir, None, timeout_secs)?
    };

    let status = stream_and_wait(&mut child, None, timeout_secs, label, !r.sandboxed)
        .with_context(|| "the cargo --version probe failed (fetch-phase.md §11)")?;
    if !status.success() {
        bail!("fetch run ({label}) failed with status {status}");
    }
    let Ok(version) = std::fs::read_to_string(&out_host) else {
        bail!(
            "the cargo --version probe wrote no version file at {} — is there a \
             runnable `cargo` on the fetch PATH? (fetch-phase.md §11)",
            out_host.display()
        );
    };
    let version = version.trim().to_string();
    if version.is_empty() {
        bail!(
            "the cargo --version probe produced an empty version string \
             (fetch-phase.md §11)"
        );
    }
    Ok(version)
}

/// The §8 fetch base allowlist ("from scratch, same base as §7"): the Go
/// names are absent by construction. The backend adds its own keys.
fn cargo_fetch_base_env(home: &str) -> Vec<(String, String)> {
    vec![
        ("PATH".into(), fetch_path()),
        ("TMPDIR".into(), "/tmp".into()),
        ("LC_ALL".into(), "C".into()),
        ("TZ".into(), "UTC".into()),
        ("HOME".into(), home.into()),
    ]
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
fn eco_cache_dir(r: &FetchRun, eco: &str) -> PathBuf {
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
    let cache = eco_cache_dir(r, "go");
    let entry = cache.join(key);
    if entry_complete(&entry) {
        return Ok(());
    }
    // the offline guard comes before anything is created: a miss offline
    // must name the entry and touch nothing (§12)
    if crate::fetch::offline() {
        bail!(
            "offline: the Go module cache entry is not populated: {} — run a \
             build once online, or use `nawa fetch` (fetch-phase.md §12)",
            entry.display()
        );
    }
    std::fs::create_dir_all(&cache)
        .with_context(|| format!("create the Go fetch cache {}", cache.display()))?;
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

    let mut child = if r.sandboxed {
        let base = r.build_inside.trim_end_matches('/');
        // the fetch scratch is the per-build home bind; the temporary entry
        // is a writable extra bind at <base>/fetch-out (§12)
        let env = go_fetch_env(&proxy, sumdb, &format!("{base}/home"), &format!("{base}/fetch-out"));
        spawn_fetch_child(r, &script, env, &plan.dir, Some(temp), timeout_secs)?
    } else {
        // no sandbox (§12): the host toolchain, a cleared environment, the
        // per-build scratch as HOME
        let home_host = r.work.join("sb").join("home");
        std::fs::create_dir_all(&home_host)
            .with_context(|| format!("create {}", home_host.display()))?;
        let env = go_fetch_env(&proxy, sumdb, &home_host.to_string_lossy(), &temp.to_string_lossy());
        spawn_fetch_child(r, &script, env, &plan.dir, None, timeout_secs)?
    };

    let status = stream_and_wait(&mut child, Some(&log_path), timeout_secs, label)?;
    if !status.success() {
        bail!("fetch run ({label}) failed with status {status} — log: {}", log_path.display());
    }
    Ok(())
}

/// Spawn one fetch command in the right flavor: sandboxed (`sandbox`/
/// `closure`) through the §12 fetch spec — configured once, via the same
/// sandbox profile as the populate run and for `cargo --version` alike —
/// or a plain cleared-environment `bash` at `none`/`env` (§12: no
/// isolation, never described as network-isolated).
///
/// * `script` — the shell body (starts with `exec 2>&1`).
/// * `env` — the run's allowlist, already in the flavor's frame.
/// * `workdir` — the fetch directory *in the host frame*; the sandbox sees
///   `<base>/src/<rel>`.
/// * `out_host` — when set and sandboxed, is bound writable at
///   `<base>/fetch-out` (the populate dir, a version probe's scratch…).
fn spawn_fetch_child(
    r: &FetchRun,
    script: &str,
    env: Vec<(String, String)>,
    workdir: &Path,
    out_host: Option<&Path>,
    timeout_secs: u64,
) -> Result<Child> {
    if r.sandboxed {
        let env: BTreeMap<String, String> = env.into_iter().collect();
        let spec = fetch_sandbox_spec(
            r,
            env,
            script,
            workdir,
            out_host.unwrap_or(r.work),
            timeout_secs,
        )?;
        let spec_path = r.work.join("fetch-sandbox-spec.toml");
        std::fs::write(&spec_path, toml::to_string_pretty(&spec)?)
            .with_context(|| format!("write {}", spec_path.display()))?;
        let exe = std::env::current_exe().context("locate the nawa binary")?;
        let (child, sock) = crate::sandbox::spawn_init(&exe, &spec_path)
            .context("spawn nawa sandbox-init for the fetch run")?;
        // install the ranged uid map when we are root, then release the child
        crate::sandbox::parent_sync(&child, sock);
        Ok(child)
    } else {
        std::process::Command::new("bash")
            .args(["-e", "-c", script])
            .current_dir(workdir)
            .env_clear()
            .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .context("failed to spawn the fetch run")
    }
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
    workdir_host: &Path,
    out_host: &Path,
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
    let rel = workdir_host.strip_prefix(r.src).unwrap_or(Path::new(""));
    let workdir = format!("{}/src/{}", base.trim_end_matches('/'), rel.display())
        .trim_end_matches('/')
        .to_string();

    let mut extra_binds = vec![ExtraBind {
        // the temporary entry (or the probe scratch) being written;
        // published after the run for populate outputs
        host: out_host.to_string_lossy().into_owned(),
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
            // the view is normalised writable (§11: the published entry is
            // 0444/0555): dirs and set executables 0755, plain files 0644
            let want = if mode & 0o111 != 0 { 0o755 } else { 0o644 };
            std::fs::set_permissions(&dst, std::fs::Permissions::from_mode(want))
                .with_context(|| format!("mode {}", dst.display()))?;
        }
    }
    Ok(())
}

/// Stream a fetch child's combined stdout (both run flavors exec
/// `exec 2>&1` first) to the console — and to `<work>/<label>.log` when a
/// log path is given (populate runs; the version probe stays silent) —
/// quoting the last lines on failure. Enforces the wall clock by polling:
/// a plain run has no supervisor, and a sandboxed run gets the same second
/// net (the sandbox itself kills the PID namespace, §12).
fn stream_and_wait(
    child: &mut Child,
    log_path: Option<&Path>,
    timeout_secs: u64,
    label: &str,
    kill_process_group: bool,
) -> Result<ExitStatus> {
    let mut log = match log_path {
        Some(p) => Some(std::fs::File::create(p).with_context(|| format!("create {}", p.display()))?),
        None => None,
    };

    // Keep the reader independent so a descendant retaining stdout cannot
    // make join() block beyond the fetch timeout after the shell exits.
    let stdout: ChildStdout = child.stdout.take().context("no stdout pipe")?;
    let tail = std::sync::Arc::new(std::sync::Mutex::new(
        std::collections::VecDeque::<String>::with_capacity(FAIL_TAIL_LINES + 1),
    ));
    let tail_writer = std::sync::Arc::clone(&tail);
    let (reader_done_tx, reader_done_rx) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        let mut r = std::io::BufReader::new(stdout);
        let mut buf = Vec::new();
        while r.read_until(b'\n', &mut buf).unwrap_or(0) > 0 {
            if let Some(f) = log.as_mut() { let _ = f.write_all(&buf); }
            {
                let mut out = std::io::stdout().lock();
                let _ = out.write_all(&buf);
                let _ = out.flush();
            }
            let mut t = tail_writer.lock().unwrap();
            if t.len() == FAIL_TAIL_LINES { t.pop_front(); }
            t.push_back(String::from_utf8_lossy(&buf).trim_end().to_string());
            buf.clear();
        }
        let _ = reader_done_tx.send(());
    });

    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    loop {
        if let Some(status) = child.try_wait().context("waiting for the fetch run")? {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if reader_done_rx.recv_timeout(remaining).is_err() {
                terminate_fetch_process(child, kill_process_group);
                let _ = reader.join();
                let t: Vec<String> = tail.lock().unwrap().drain(..).collect();
                bail!("{label} exceeded NAWA_FETCH_TIMEOUT ({timeout_secs}s); a descendant retained stdout and the process tree was killed\n--- last output ---\n{}", t.join("\n"));
            }
            reader.join().map_err(|_| anyhow::Error::msg("fetch log reader crashed"))?;
            let t: Vec<String> = tail.lock().unwrap().drain(..).collect();
            if !status.success() {
                bail!("{label} failed with status {status}\n--- last output ---\n{}", t.join("\n"));
            }
            return Ok(status);
        }
        if Instant::now() >= deadline {
            terminate_fetch_process(child, kill_process_group);
            let _ = reader.join();
            let t: Vec<String> = tail.lock().unwrap().drain(..).collect();
            bail!("{label} exceeded NAWA_FETCH_TIMEOUT ({timeout_secs}s) and was killed\n--- last output ---\n{}", t.join("\n"));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Kill the plain fetch process group, including Go/Cargo descendants. Sandboxed
/// runs are supervised by the PID namespace and must not signal the caller.
fn terminate_fetch_process(child: &mut Child, kill_process_group: bool) {
    if kill_process_group {
        unsafe { libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL); }
    } else {
        let _ = child.kill();
    }
    let _ = child.wait();
}