//! Build phase 2: assemble the build root, run the ecosystem fetch phase and
//! compute the environment the build sections run with.

use anyhow::{Context, Result};
use std::path::Path;

use super::sandbox::{layer_cache_dir, layer_paths_are_mountable, overlay_probe_ok};
use super::stage::BuildView;
use super::{BuildOpts, CheckMode};
use crate::buildenv::{self, Isolation};
use crate::closure::AssembledRoot;
use crate::recipe::Recipe;

/// Assemble the closure-level build root (§5): cached layers + one overlayfs
/// mount when the kernel allows it, extract-per-build otherwise. `None` for
/// every other isolation level.
pub(super) fn assemble_build_root(
    rec: &Recipe,
    opts: &BuildOpts,
    work: &Path,
    outdir: &Path,
) -> Result<Option<AssembledRoot>> {
    let mut assembled: Option<AssembledRoot> = None;
    if opts.isolation == Isolation::Closure {
        let known = crate::recipe::load_all(&opts.recipes_root)?;
        let with_check = opts.check != CheckMode::Off;
        let members =
            crate::closure::buildroot(rec, &known, with_check).context("compute build root")?;
        crate::nawa_info!(
            "closure: {} package(s) in the build root ({}{}): {}",
            members.len(),
            crate::closure::BASE_DEVEL,
            if with_check { " + check deps" } else { "" },
            members.join(" ")
        );
        let root_dir = work.join("buildroot");
        let _ = std::fs::remove_dir_all(&root_dir);

        // Phase 5 (§5.3 v2): assemble from cached layers + one overlayfs
        // mount when the kernel allows overlayfs in a user namespace; fall
        // back to extract-per-build (v1) otherwise — same result, slower.
        // The layer cache must also be traversable by the sandbox's mapped
        // identity (§4.3): the overlayfs lowerdirs are resolved by mount(2)
        // *inside* the sandbox, where a home directory with mode 700 owned
        // by an unmapped uid blocks nawa's own caches (`sudo nawa` with the
        // rootfs under $HOME is the common case). Filtering here keeps an
        // unreachable cache from failing the build after extraction.
        let layer_cache = layer_cache_dir(opts).and_then(|dir| {
            // `layer_cache_dir` has created `dir`; everything below it
            // (`layers/<sha>`) is created inside `assemble_cached` under the
            // same parents, so the cache root is the right check target
            // (then a brand-new cache is not read as "unreachable").
            match crate::sandbox::first_untraversable_ancestor(&dir) {
                None => Some(dir),
                Some(blocker) => {
                    eprintln!(
                        "nawa: closure: note: the layer cache {} is not \
                         reachable from inside the sandbox, because {} is \
                         not traversable by the sandbox's mapped identity \
                         (§4.3 maps only the caller's own id — a home \
                         directory with mode 700 blocks `sudo` runs). \
                         Using extract-per-build assembly; to use the \
                         layer cache, keep the rootfs under the build dir \
                         (or otherwise outside such trees), point \
                         NAWA_LAYER_CACHE at a reachable path, or run nawa \
                         as a normal user (rootless runs map their own \
                         identity)",
                        dir.display(),
                        blocker.display()
                    );
                    None
                }
            }
        });
        let layered = layer_cache.is_some()
            && layer_paths_are_mountable(&members, &known, &opts.outdir, work)
            && {
                let ok = overlay_probe_ok();
                if !ok {
                    eprintln!(
                        "nawa: closure: note: this kernel does not allow \
                         overlayfs in a user namespace (needs ≥ 5.11) — \
                         using extract-per-build assembly"
                    );
                }
                ok
            };
        assembled = Some(if layered {
            crate::nawa_info!("closure: assembling the build root from the layer cache (overlayfs)");
            let cache = layer_cache.expect("checked above");
            crate::closure::assemble_cached(
                &members,
                &known,
                outdir,
                &root_dir,
                &cache,
                &opts.root,
                opts.allow_unsigned,
                |pkg| crate::nawa_info!("closure: extracted {pkg} (new layer)"),
            )?
        } else {
            crate::nawa_info!("closure: assembling the build root (extract-per-build)");
            crate::closure::assemble(
                &members,
                &known,
                outdir,
                &root_dir,
                &opts.root,
                opts.allow_unsigned,
                |pkg| crate::nawa_info!("closure: extracted {pkg}"),
            )?
        });
    }
    Ok(assembled)
}

/// Run the per-ecosystem fetch phase (fetch-phase.md §7, §11, §12) after the
/// closure assembly (the fetch sandbox reuses the same build root: the
/// toolchain lives in it) and before the build environment is computed (the
/// whitelist and the sandbox spec need the per-build fetch view paths).
#[allow(clippy::too_many_arguments)]
pub(super) fn run_fetch_phase(
    rec: &Recipe,
    opts: &BuildOpts,
    builddir: &Path,
    work: &Path,
    src: &Path,
    sandboxed: bool,
    sandbox_base: &str,
    assembled: Option<&AssembledRoot>,
) -> Result<Vec<(String, String)>> {
    let fetch_env: Vec<(String, String)> = if rec.fetch.any() {
        let srccache = crate::config::srccache_dir(builddir, None);
        // GOCACHE lives "under the HOME scratch" (§7): inside the sandbox
        // that is the mounted per-build home; at `env` the build runs on
        // the host and sees host paths (like NAWA_SRCDIR), so the scratch
        // is the real workspace one.
        let whitelist_home = if sandboxed {
            format!("{}/home", sandbox_base.trim_end_matches('/'))
        } else {
            work.join("sb").join("home").to_string_lossy().into_owned()
        };
        let fr = crate::fetchrun::FetchRun {
            rec,
            opts,
            work,
            src,
            srccache: &srccache,
            sandboxed,
            build_inside: sandbox_base,
            whitelist_home: &whitelist_home,
            assembled,
        };
        crate::fetchrun::run(&fr)?
    } else {
        Vec::new()
    };
    Ok(fetch_env)
}

/// The environment of the build sections and the script prefix: the
/// whitelist at `env` level and above, the profile env as-is at `none`.
#[allow(clippy::too_many_arguments)]
pub(super) fn build_env(
    rec: &Recipe,
    opts: &BuildOpts,
    sandboxed: bool,
    sandbox_base: &str,
    view: &BuildView,
    has_assets: bool,
    sde: u64,
    fetch_env: &[(String, String)],
) -> (Vec<(String, String)>, &'static str) {
    let _ = rec; // kept for symmetry with the other phases
    let (env_vars, env_prefix) = if opts.isolation >= Isolation::Env {
        let home = if sandboxed {
            format!("{}/home", sandbox_base.trim_end_matches('/'))
        } else {
            buildenv::SANDBOX_HOME.to_string()
        };
        // §4.3/§5.3: the new names follow the *tables*, not the staging
        // structs — an assets-only recipe stages `staged` (an empty
        // workspace) but must NOT gain NAWA_SRCROOT, exactly as
        // `check_schema_placeholders` refuses it in raw bytes
        let has_sources = !rec.package.sources.is_empty();
        let spec_srcroot = has_sources.then(|| view.srcroot.clone());
        let spec_assetsdir = has_assets.then(|| view.assets.clone());
        let spec_env = buildenv::EnvSpec {
            profile_env: &opts.env,
            jobs: opts.jobs,
            srcdir: &view.src,
            pkgdir: &view.pkg,
            home: &home,
            source_date_epoch: sde,
            // new-schema names (§4.3/§5.3); joined to the whitelist only
            // when the recipe enables them
            srcroot: spec_srcroot.as_deref(),
            assetsdir: spec_assetsdir.as_deref(),
            // [fetch.*] whitelist additions (§7): paths already phrased in
            // the build's frame by the fetch runner
            fetch_env: fetch_env.to_vec(),
        };
        (buildenv::whitelist(&spec_env), opts.isolation.script_prefix())
    } else {
        // `none`: the build runs with the profile env as-is, but the fetch
        // phase's additions (CARGO_HOME, GOFLAGS, ...) still have to reach
        // the sections, or an offline build cannot see what was fetched.
        let mut v = opts.env.clone();
        v.extend(fetch_env.iter().cloned());
        (v, "")
    };

    (env_vars, env_prefix)
}
