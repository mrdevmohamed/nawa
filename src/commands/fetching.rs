//! `nawa fetch` — pre-seed the caches offline builds consume (§15).
//!
//! Per recipe: check assets, download and verify every external source into
//! the source cache, then run the declared ecosystem backends
//! (`[fetch.go]`/`[fetch.cargo]`) — the same runners the build drives, so a
//! fetched cache entry is byte-identical to the one a build would publish.
//! `--sources-only` stops after the sources (no toolchain needed). With
//! `--offline` nothing is downloaded: existing artifacts are verified and a
//! missing entry is a hard error naming it — the CI gate before
//! `build --offline`.
//!
//! At `none`/`env` isolation the fetches run as plain / cleared-environment
//! commands (§12); at `sandbox` they go through the fetch sandbox spec, and
//! at `closure` the same build root a build would assemble is prepared first
//! (the toolchain lives in it).
use crate::ctx::Ctx;
use crate::assets;
use crate::build;
use crate::buildenv;
use crate::closure;
use crate::fetch;
use crate::fetchrun;
use crate::recipe;
use crate::sources;

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

#[allow(clippy::too_many_arguments)]
pub(crate) fn cmd_fetch(
    ctx: &Ctx,
    names: &[String],
    all: bool,
    isolation: Option<&str>,
    sources_only: bool,
    offline_flag: bool,
    allow_degraded: bool,
) -> Result<()> {
    let offline = offline_flag || super::offline_from_env()?;
    crate::fetch::set_offline(offline);

    // --offline contradicts recipes that need the network mid-build, the
    // same way build refuses them (§12)
    let load = |wanted: &dyn Fn(&recipe::Recipe) -> bool| -> Result<Vec<recipe::Recipe>> {
        let known = ctx.load_all()?;
        let mut recs: Vec<recipe::Recipe> = known
            .values()
            .filter(|r| wanted(r))
            .cloned()
            .collect();
        recs.sort_by(|a, b| a.package.name.cmp(&b.package.name));
        Ok(recs)
    };

    let recs: Vec<recipe::Recipe> = if all {
        // §15: every recipe with a remote source or a fetch table; legacy
        // recipes included (their remote tarball is downloaded and pinned
        // through the same cache the build re-verifies from).
        load(&|r: &recipe::Recipe| {
            fetchable_legacy(r) || !r.package.sources.is_empty() || r.fetch.any()
        })?
    } else {
        let mut recs = Vec::new();
        for name in names {
            let rec = recipe::load_by_name(&ctx.recipes, name)?;
            if !fetchable_legacy(&rec) && rec.package.sources.is_empty() && !rec.fetch.any() {
                bail!(
                    "recipe {name} has no remote source and no [fetch.*] table — \
                     there is nothing to fetch (legacy local sources are staged \
                     straight into the build)"
                );
            }
            recs.push(rec);
        }
        recs
    };
    if recs.is_empty() {
        crate::nawa_info!("fetch: nothing to fetch (--all found no recipe with a remote source or a fetch table)");
        return Ok(());
    }

    // Fetch runners execute Cargo from an isolated temporary working
    // directory. Resolve every host-side workspace path before handing it
    // to them; relative CARGO_HOME, manifest and output paths would otherwise
    // be interpreted relative to that temporary cwd.
    let cwd = std::env::current_dir().context("resolve the fetch working directory")?;
    let builddir = if ctx.builddir.is_absolute() {
        ctx.builddir.clone()
    } else {
        cwd.join(&ctx.builddir)
    };
    std::fs::create_dir_all(&builddir)
        .with_context(|| format!("create build directory {}", builddir.display()))?;
    let builddir = std::fs::canonicalize(&builddir)
        .with_context(|| format!("resolve build directory {}", builddir.display()))?;

    let srccache = crate::config::srccache_dir(&builddir, Some(&cwd));
    info_mode(&recs, sources_only, offline);

    for rec in &recs {
        fetch_one(ctx, &builddir, rec, &srccache, isolation, sources_only, offline, allow_degraded)
            .with_context(|| format!("fetching sources for {}", rec.id()))?;
    }
    crate::nawa_info!(
        "fetch: {} recipe(s) done{}",
        recs.len(),
        if sources_only {
            " (sources only)"
        } else {
            ""
        }
    );
    Ok(())
}

/// A legacy recipe's fetchable part is a *remote* tarball: download + sha256
/// verification. `local:` paths are host files and meta recipes have none.
fn fetchable_legacy(rec: &recipe::Recipe) -> bool {
    let s = &rec.package.source;
    !s.is_empty() && !s.starts_with("local:") && !rec.is_new_schema()
}

fn info_mode(recs: &[recipe::Recipe], sources_only: bool, offline: bool) {
    let what = if sources_only { "sources" } else { "sources + ecosystems" };
    let mode = if offline { " (offline: verify only)" } else { "" };
    crate::nawa_info!(
        "fetch: {} recipe(s) — {}: {}{mode}",
        recs.len(),
        what,
        recs.iter().map(|r| r.package.name.clone()).collect::<Vec<_>>().join(", ")
    );
}

#[allow(clippy::too_many_arguments)]
fn fetch_one(
    ctx: &Ctx,
    builddir: &Path,
    rec: &recipe::Recipe,
    srccache: &std::path::Path,
    isolation: Option<&str>,
    sources_only: bool,
    offline: bool,
    allow_degraded: bool,
) -> Result<()> {
    if offline && rec.sandbox.network {
        bail!(
            "recipe {} sets [sandbox] network = true, which contradicts --offline \
             (NAWA_OFFLINE); drop one of the two",
            rec.id()
        );
    }
    // Assets (§15): a fetched state must satisfy exactly the asset rules a
    // build would enforce before any download — missing paths, symlinks and
    // specials fail here, the snapshot digest is recomputed (never cached).
    if !rec.assets.include.is_empty() {
        assets::precheck(rec)
            .with_context(|| format!("assets of recipe {}", rec.package.name))?;
    }

    let work = builddir.join(rec.id());
    let src = work.join("src");

    // ---- sources ----------------------------------------------------------
    let mut staged_srcdir: Option<PathBuf> = None;
    if !rec.package.sources.is_empty() {
        // New-schema sources: downloads + verification run content-addressed
        // into <srccache>/sha256/<digest> inside stage_sources; the workspace
        // extraction is what the ecosystem backends plan around.
        build::wipe_work(&work)?;
        std::fs::create_dir_all(&src)?;
        let s = sources::stage_sources(rec, &work, &src, srccache, crate::default_jobs())?;
        build::grant_source_write(&s.srcroot);
        staged_srcdir = Some(s.srcdir);
        crate::nawa_info!("fetch: staged {} source(s)", rec.package.sources.len());
    } else if fetchable_legacy(rec) {
        fetch_legacy_source(rec, ctx, srccache)?;
    }

    // ---- ecosystems (§7/§8) ----------------------------------------------
    if sources_only || !rec.fetch.any() {
        return Ok(());
    }
    // A fetch table without a staged source has nothing to plan around:
    // the backends walk the downloaded source tree (§7/§8).
    if staged_srcdir.is_none() {
        bail!(
            "recipe {} has a [fetch.*] table but no [[package.sources]] — \
             the fetch backends plan around a downloaded source",
            rec.package.name
        );
    }
    let opts = super::prepare_build(
        ctx,
        rec,
        build::CheckMode::Off,
        isolation,
        false,
        allow_degraded,
        None,
        false, // use_cache: fetch runs are never cached
        offline,
    )?;
    let sandboxed = opts.isolation >= buildenv::Isolation::Sandbox;
    let sandbox_base = if sandboxed {
        build::sandbox_inside_base(&work)
    } else {
        String::new()
    };
    let whitelist_home = if sandboxed {
        format!("{}/home", sandbox_base.trim_end_matches('/'))
    } else {
        work.join("sb").join("home").to_string_lossy().into_owned()
    };

    // closure: assemble the same build root a build would (§15) — the
    // fetch sandbox reuses it for the toolchain. sandbox: fetch runs
    // against the live root (RootSpec::default()); none/env: no sandbox.
    let mut assembled: Option<closure::AssembledRoot> = None;
    if opts.isolation == buildenv::Isolation::Closure {
        let known = ctx.load_all()?;
        let members =
            closure::buildroot(rec, &known, false).context("compute build root")?;
        crate::nawa_info!(
            "fetch: closure: {} package(s) in the build root: {}",
            members.len(),
            members.join(" ")
        );
        let root_dir = work.join("buildroot");
        let _ = std::fs::remove_dir_all(&root_dir);
        assembled = Some(
            closure::assemble(
                &members,
                &known,
                &builddir.join("out"),
                &root_dir,
                &ctx.root,
                false,
                |pkg| crate::nawa_info!("fetch: closure: extracted {pkg}"),
            )
            .with_context(|| format!("assemble the fetch build root for {}", rec.id()))?,
        );
    }

    let fr = fetchrun::FetchRun {
        rec,
        opts: &opts,
        work: &work,
        src: &src,
        srccache,
        sandboxed,
        build_inside: &sandbox_base,
        whitelist_home: &whitelist_home,
        assembled: assembled.as_ref(),
    };
    let env = fetchrun::run(&fr).context("the ecosystem fetch failed")?;
    for (k, v) in &env {
        crate::nawa_info!("fetch: {k}={v}");
    }
    Ok(())
}

/// Legacy recipe: download + verify the pinned tarball into the shared
/// cache (`<sha16>-<file name>`, §1.1), no extraction — the build unpacks
/// from the verified cache as today. Cached hits are re-verified, corrupt
/// entries re-downloaded.
fn fetch_legacy_source(
    rec: &recipe::Recipe,
    ctx: &Ctx,
    cache_dir: &std::path::Path,
) -> Result<()> {
    std::fs::create_dir_all(cache_dir)
        .with_context(|| format!("create source cache {}", cache_dir.display()))?;
    let ectx = recipe::ExpandCtx {
        jobs: ctx.jobs.unwrap_or(4),
        pkgdir: PathBuf::from("/"),
        srcdir: PathBuf::from("/"),
        arch: rec.arch(),
        srcroot: None,
        assetsdir: None,
    };
    let source = rec.expand(&rec.package.source, &ectx);
    let fname = source.rsplit('/').next().unwrap_or("source.tar").to_string();
    let sha = rec.package.sha256.as_str();
    let tarball = cache_dir.join(format!("{}-{}", sha.get(..16).unwrap_or("nosha"), fname));
    if tarball.exists() && fetch::verify_sha256(&tarball, sha).is_ok() {
        crate::nawa_info!("source cached: {}", tarball.display());
        return Ok(());
    }
    let mirrors: Vec<String> = rec.package.mirrors.iter().map(|m| rec.expand(m, &ectx)).collect();
    fetch::download_cached_with_mirrors(&source, &mirrors, &tarball, Some(sha))?;
    fetch::verify_sha256(&tarball, sha)
        .with_context(|| format!("verify the fetched source of recipe {}", rec.package.name))?;
    crate::nawa_info!("source verified: {}", tarball.display());
    Ok(())
}
