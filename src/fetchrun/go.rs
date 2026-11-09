//! Go backend (§7).

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::os::unix::fs::PermissionsExt;

use super::common::{ecosystem_key, eco_cache_dir, entry_complete, copy_into_view, publish_entry, cleanup_failed_populate, spawn_fetch_child, fetch_path, fetch_timeout_secs, stream_and_wait};
use super::FetchRun;
use crate::recipe::FetchSpec;

/// The Go backend (§7): plan, populate or hit, then expose the module cache.
pub(crate) fn run_go(r: &FetchRun<'_>, spec: &FetchSpec) -> Result<Vec<(String, String)>> {
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
pub(crate) struct GoPlan {
    /// Host path of the fetch directory.
    dir: PathBuf,
    /// `vendor/modules.txt` exists → build with `-mod=vendor`, no fetch.
    vendored: bool,
    go_mod: Vec<u8>,
    go_sum: Vec<u8>,
}


pub(crate) fn plan_go(r: &FetchRun<'_>, spec: &FetchSpec) -> Result<GoPlan> {
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


/// Make sure the Go module cache entry `key` is published: marker-free fast
/// path when it exists, otherwise take the entry flock, recheck, populate
/// in a 0777 sibling temporary directory, validate, mark, rename (§11/§12).
pub(crate) fn ensure_go_entry(r: &FetchRun, plan: &GoPlan, key: &str) -> Result<()> {
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
        crate::unique::next()
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
pub(crate) fn proxy_and_sumdb() -> Result<(String, &'static str)> {
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


/// The §7 fetch allowlist, parameterised for the run flavor. The build
/// environment (§7 bottom list) is separate — see [`run`].
pub(crate) fn go_fetch_env(
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
pub(crate) fn run_populate(r: &FetchRun, plan: &GoPlan, temp: &Path, label: &str) -> Result<()> {
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

    let status = stream_and_wait(&mut child, Some(&log_path), timeout_secs, label, !r.sandboxed)?;
    if !status.success() {
        bail!("fetch run ({label}) failed with status {status} — log: {}", log_path.display());
    }
    Ok(())
}

