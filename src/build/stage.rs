//! Build phase 1: stage the sources and assets, and describe the workspace
//! as the build sections will see it.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

use super::{grant_source_write, sandbox_inside_base, BuildOpts};
use crate::buildenv::Isolation;
use crate::fetch;
use crate::recipe::{ExpandCtx, Recipe};

/// What source staging produced.
pub(super) struct StagedSource {
    /// The directory the build sections run in.
    pub(super) srcdir: PathBuf,
    /// The new-schema source workspace (`[[package.sources]]`), if any.
    pub(super) sources: Option<crate::sources::SourcesStaged>,
    /// The `[assets]` snapshot, if the recipe has any.
    pub(super) assets: Option<crate::assets::AssetsStaged>,
}

/// Download, verify and extract the recipe's sources (and snapshot its
/// assets) into `<work>/src`. Fails before the build root is assembled.
pub(super) fn stage_source(
    rec: &Recipe,
    opts: &BuildOpts,
    builddir: &Path,
    work: &Path,
    src: &Path,
    pkg: &Path,
) -> Result<StagedSource> {
    let (builddir, work, src, pkg) =
        (builddir.to_path_buf(), work.to_path_buf(), src.to_path_buf(), pkg.to_path_buf());
    let name = &rec.package.name;
    let ctx = ExpandCtx {
        jobs: opts.jobs,
        pkgdir: pkg.clone(),
        srcdir: src.clone(),
        arch: rec.arch(),
        // new-schema fields (§4.3/§5.3); wired up with the sources schema
        srcroot: None,
        assetsdir: None,
    };

    let source = &rec.package.source;
    let mut staged: Option<crate::sources::SourcesStaged> = None;
    let mut staged_assets: Option<crate::assets::AssetsStaged> = None;
    let srcdir: PathBuf = if rec.is_new_schema() {
        // ---- new-schema staging (fetch-phase.md §4.2, §5, §6, §11) -------
        // §5.2: an unreadable/missing asset path fails BEFORE any download,
        // by the same sweep the snapshot will run (§16 Assets 3).
        if !rec.assets.include.is_empty() {
            crate::assets::precheck(rec)?;
        }
        // All sources are downloaded, verified and extracted by the sources
        // engine; the workspace is `<work>/src` and `[package] workdir`
        // selects the section cwd through `staged.srcdir`.
        let cache_dir = crate::config::srccache_dir(&builddir, None);
        let s = crate::sources::stage_sources(rec, &work, &src, &cache_dir, opts.jobs)?;
        // normalise ownership/permissions for the sandbox's mapped identity
        grant_source_write(&s.srcroot);
        let sub_srcdir = s.srcdir.clone();
        staged = Some(s);
        // the asset snapshot runs after the sources (which may have failed
        // with a wrong hash) but still before the build root is assembled
        // (§3): the digest comes from the copy, and only the snapshot is
        // copied, never the checkout again
        if !rec.assets.include.is_empty() {
            let a = crate::assets::snapshot(rec, &work)
                .with_context(|| format!("stage assets of recipe {}", name))?;
            staged_assets = Some(a);
        }
        sub_srcdir
    } else if source.is_empty() {
        src.clone() // meta package: run scripts directly in src
    } else if let Some(local) = source.strip_prefix("local:") {
        let p = PathBuf::from(local);
        if !p.exists() {
            bail!("local source {} does not exist", p.display());
        }
        fetch::copy_local(&p, &src)?;
        grant_source_write(&src);  // host copies can carry foreign owners too
        src.clone()
    } else {
        // expand %{version} & co. in the source URL itself
        let source = rec.expand(source, &ctx);
        let fname = source.rsplit('/').next().unwrap_or("source.tar");
        // Source cache: the work dir is wiped on every build, so a tarball
        // kept inside it was downloaded again each time. Keep it in a
        // persistent, content-keyed cache instead ($NAWA_SRCCACHE, default
        // <builddir>/distfiles). A hit is re-verified against the recipe's
        // sha256, so a corrupt/truncated entry is re-downloaded.
        let cache_dir = crate::config::srccache_dir(&builddir, None);
        std::fs::create_dir_all(&cache_dir)
            .with_context(|| format!("create source cache {}", cache_dir.display()))?;
        let sha = rec.package.sha256.as_str();
        let tarball = cache_dir.join(format!("{}-{}", sha.get(..16).unwrap_or("nosha"), fname));
        if tarball.exists() && fetch::verify_sha256(&tarball, sha).is_ok() {
            crate::nawa_info!("source cached: {}", tarball.display());
        } else {
            crate::nawa_info!("downloading {source}");
        }
        let mirrors: Vec<String> =
            rec.package.mirrors.iter().map(|m| rec.expand(m, &ctx)).collect();
        fetch::download_cached_with_mirrors(&source, &mirrors, &tarball, Some(sha))?;
        fetch::verify_sha256(&tarball, sha)?;

        // tarball or single file (e.g. a pinned CA bundle)? Detection is by
        // content (magic bytes + tar header), not by file name.
        let is_tar = fetch::detect_source_format(&tarball).is_some();
        if !is_tar {
            // single-file source: stage it as-is into srcdir
            std::fs::copy(&tarball, src.join(fname))
                .with_context(|| format!("stage single-file source {}", fname))?;
            src.clone()
        } else {
            // In-process extraction (tar crate + native decoders) with GNU
            // tar --no-same-owner semantics: ownership is never restored and
            // suid bits are dropped, so the tree stays writable for the build
            // (the sandbox §4.1 drops every capability, and the source write
            // pass below re-applies u+w on top).
            fetch::extract_source_archive(&tarball, &src)
                .with_context(|| format!("failed to extract {}", tarball.display()))?;
            grant_source_write(&src);
            let dir = if rec.package.source_dir.is_empty() {
                format!("{}-{}", name, rec.package.version)
            } else {
                rec.package.source_dir.clone()
            };
            let candidate = src.join(rec.expand(&dir, &ctx));
            if candidate.is_dir() { candidate } else { src.clone() }
        }
    };

    Ok(StagedSource { srcdir, sources: staged, assets: staged_assets })
}

/// Where the sandbox's build base lives for this build; empty when the build
/// is not sandboxed. `sandbox` level keeps the live root and mounts a fresh
/// tmpfs on /tmp BEFORE binding the staging dirs, which would hide a build
/// dir under /tmp — so that combination is refused up front.
pub(super) fn sandbox_base_for(opts: &BuildOpts, work: &Path) -> Result<String> {
    if opts.isolation == Isolation::Sandbox {
        let w = std::fs::canonicalize(work).unwrap_or_else(|_| work.to_path_buf());
        if w.starts_with("/tmp") {
            bail!(
                "--isolation sandbox cannot build under /tmp ({}): the sandbox's private /tmp \
                 would hide the build dir. Use another --builddir (e.g. /build or /var/tmp/nawa)",
                work.display()
            );
        }
    }
    let sandboxed = opts.isolation >= Isolation::Sandbox;
    Ok(if sandboxed { sandbox_inside_base(work) } else { String::new() })
}

/// The build's own view of the workspace paths (`%{srcdir}`, `%{pkgdir}`,
/// `%{srcroot}`, `%{assetsdir}`): sandbox paths when sandboxed, host paths
/// otherwise. The sandbox spec carries the host paths for the bind mounts.
pub(super) struct BuildView {
    pub(super) srcroot: PathBuf,
    pub(super) src: PathBuf,
    pub(super) pkg: PathBuf,
    pub(super) assets: PathBuf,
}

pub(super) fn build_view(
    sandboxed: bool,
    sandbox_base: &str,
    assets_dir: Option<&Path>,
    src: &Path,
    srcdir: &Path,
    pkg: &Path,
) -> BuildView {
    let base = sandbox_base.trim_end_matches('/');
    // §4.3/§5.3: %{srcroot} is the workspace root and %{assetsdir} the
    // assets snapshot — both live at <work>/… and are bound inside the
    // sandbox under <build_inside>/…; inside the sandbox the whole staging
    // dir is bound at <base>/src, so the srcroot view is that same root
    let assets = match (assets_dir, sandboxed) {
        (Some(dir), false) => dir.to_path_buf(),
        // `<work>/assets` is bound at <base>/assets inside the sandbox
        (Some(_), true) => PathBuf::from(format!("{base}/assets")),
        (None, _) => PathBuf::new(),
    };
    let srcroot = if sandboxed {
        PathBuf::from(format!("{base}/src"))
    } else {
        src.to_path_buf()
    };
    let view_src = if sandboxed {
        // `srcdir` is the unpacked source dir (e.g. src/m4-1.4.19); the sandbox
        // binds the whole staging dir at <base>/src, so keep the sub-path
        let mut v = srcroot.clone();
        if let Ok(rel) = srcdir.strip_prefix(src) {
            v.push(rel);
        }
        v
    } else {
        srcdir.to_path_buf()
    };
    let view_pkg = if sandboxed {
        PathBuf::from(format!("{base}/pkg"))
    } else {
        pkg.to_path_buf()
    };
    BuildView { srcroot, src: view_src, pkg: view_pkg, assets }
}
