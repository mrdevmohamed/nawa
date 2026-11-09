//! Sandbox preparation.

use anyhow::Result;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::BuildOpts;
use crate::recipe::Recipe;
use crate::buildenv::Isolation;
use crate::build::cache::fetch_ecosystems;

/// The layer cache root (§5.3 v2): `NAWA_LAYER_CACHE` or `<root>/var/cache/nawa`;
/// one extracted directory per archive goes under it at `<cache>/layers/<sha>`
/// (`assemble_cached`). `None` when it cannot exist (unwritable root, or the
/// user disabled it with `NAWA_NO_LAYER_CACHE=1`) — the build then falls back
/// to extract-per-build assembly.
pub(crate) fn layer_cache_dir(opts: &BuildOpts) -> Option<PathBuf> {
    if std::env::var_os("NAWA_NO_LAYER_CACHE").is_some() {
        return None;
    }
    let dir = std::env::var_os("NAWA_LAYER_CACHE")
        .map(PathBuf::from)
        .unwrap_or_else(|| opts.root.join("var/cache/nawa"));
    match std::fs::create_dir_all(&dir) {
        Ok(()) => Some(dir),
        Err(e) => {
            eprintln!(
                "nawa: closure: note: layer cache {} unusable ({e}) — \
                 using extract-per-build assembly",
                dir.display()
            );
            None
        }
    }
}


/// overlayfs data strings cannot escape ',' and ':' — every path that goes
/// into the mount (layers, scratch, root) must be free of them.
pub(crate) fn layer_paths_are_mountable(
    members: &[String],
    known: &std::collections::BTreeMap<String, Recipe>,
    outdir: &Path,
    work: &Path,
) -> bool {
    let bad = |p: &Path, what: &str| -> bool {
        let s = p.to_string_lossy();
        if s.contains(',') || s.contains(':') {
            eprintln!(
                "nawa: closure: note: {what} {} contains ',' or ':' — \
                 overlayfs cannot express it; using extract-per-build assembly",
                p.display()
            );
            return false;
        }
        true
    };
    if !bad(outdir, "the output directory") || !bad(work, "the build directory") {
        return false;
    }
    for m in members {
        if let Some(r) = known.get(m) {
            if !bad(&crate::closure::archive_for(r, outdir), "a closure archive") {
                return false;
            }
        }
    }
    true
}


/// Does this kernel allow overlayfs inside a user namespace (Phase 5)?
/// Asked in a subprocess (`nawa sandbox-probe overlay`): the probe creates
/// namespaces, and nesting them in this process would corrupt later
/// sandbox spawns.
pub(crate) fn overlay_probe_ok() -> bool {
    let Ok(exe) = std::env::current_exe() else { return false };
    let Ok(out) = Command::new(exe).args(["sandbox-probe", "overlay"]).output() else {
        return false;
    };
    out.status.success()
}


/// Everything the sandbox needs for one build, prepared once.
pub(crate) struct SandboxCfg {
    pub(crate) spec_path: PathBuf,
    pub(crate) base: crate::sandbox::SandboxSpec,
}


#[allow(clippy::too_many_arguments)]
pub(crate) fn prepare_sandbox(
    rec: &Recipe,
    opts: &BuildOpts,
    work: &Path,
    src: &Path,
    pkg: &Path,
    env_vars: &[(String, String)],
    build_inside: &str,
    workdir: &Path,
    layers: Vec<String>,
) -> Result<SandboxCfg> {
    let scratch = work.join("sb");
    let home = scratch.join("home");
    std::fs::create_dir_all(&home)?;

    // host paths to hide in live mode (§4.2): the build dir first (other
    // packages' scratch and the archive cache), then the usual suspects
    let mut mask: Vec<String> = vec![
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
    if let Some(bb) = work.parent() {
        let bbs = bb.to_string_lossy().into_owned();
        // The builddir — mask even though src/pkg are bound from under it.
        // One exception: when the pinned base IS that path (/build, §4.2)
        // the base tmpfs already hides everything but the three staging
        // binds — masking again would cover those binds themselves.
        if bbs != build_inside {
            mask.push(bbs);
        }
    }

    let base = crate::sandbox::SandboxSpec {
        version: 1,
        root: if opts.isolation == Isolation::Closure {
            crate::sandbox::RootSpec {
                mode: "dir".into(),
                dir: work.join("buildroot").to_string_lossy().into_owned(),
                // Phase 5: non-empty = the sandbox mounts the root as an
                // overlayfs over these layers instead of binding it
                layers,
            }
        } else {
            crate::sandbox::RootSpec::default()
        },
        build: crate::sandbox::BuildPaths {
            src_host: src.to_string_lossy().into_owned(),
            pkg_host: pkg.to_string_lossy().into_owned(),
            home_host: home.to_string_lossy().into_owned(),
            scratch: scratch.to_string_lossy().into_owned(),
        },
        env: env_vars.iter().cloned().collect(),
        script: String::new(), // filled per section
        timeout_secs: opts.timeout_secs,
        network: rec.sandbox.network && opts.allow_network,
        sysfs: rec.sandbox.sysfs,
        allow_degraded: opts.allow_degraded,
        mask,
        build_inside: build_inside.to_string(),
        workdir: workdir.to_string_lossy().into_owned(),
        run_uid: None,
        run_gid: None,
        memory_limit_bytes: 0,
        process_limit: 0,
        file_size_limit_bytes: 0,
        open_files_limit: 0,
        readonly_paths: Vec::new(),
        // §5.3: scripts see the snapshot at <build_inside>/assets — the
        // same dir NAWA_ASSETSDIR/%{assetsdir} name. `<work>/assets` was
        // filled by assets::snapshot before the sandbox is prepared, so
        // the O_PATH handles resolve. §8/§11: the per-build fetch views
        // join as writable binds at <build_inside>/fetch/<eco> — the same
        // dirs NAWA_GOMODCACHE/… name (created by the fetch step before
        // the sandbox is prepared, vendored recipes included).
        extra_binds: {
            let mut binds: Vec<crate::sandbox::spec::ExtraBind> = Vec::new();
            if !rec.assets.include.is_empty() {
                binds.push(crate::sandbox::spec::ExtraBind {
                    host: work.join("assets").to_string_lossy().into_owned(),
                    inside: format!("{}/assets", build_inside),
                    readonly: true,
                    kind: "dir".into(),
                });
            }
            for eco in fetch_ecosystems(rec) {
                binds.push(crate::sandbox::spec::ExtraBind {
                    host: work.join("fetch").join(eco).to_string_lossy().into_owned(),
                    inside: format!("{}/fetch/{eco}", build_inside),
                    readonly: false,
                    kind: "dir".into(),
                });
            }
            binds
        },
        strict_readonly_root: false,
        writable_paths: Vec::new(),
    };
    if rec.sandbox.network && !opts.allow_network {
        eprintln!(
            "nawa: sandbox: recipe {} requests [sandbox] network = true; ignored \
             (pass --allow-network to permit it, the archive records impure = network)",
            rec.id()
        );
    }
    let spec_path = work.join("sandbox-spec.toml");
    Ok(SandboxCfg { spec_path, base })
}

