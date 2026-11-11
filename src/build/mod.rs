//! Build engine: fetch -> verify -> extract -> [assemble build root] ->
//! run build sections (optionally inside the sandbox) -> stage into pkgdir
//! -> lint -> create archive.

use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use crate::archive;
use crate::buildenv::{self, Isolation};
use crate::fetch;
use crate::recipe::{ExpandCtx, Recipe};
use crate::slim;


pub(crate) use crate::buildenv::{BuildOpts, CheckMode};


/// How many trailing output lines are quoted when a section fails.
const FAIL_TAIL_LINES: usize = 25;


pub(crate) struct BuildResult {
    pub archive: PathBuf,
    #[allow(dead_code)]
    pub pkginfo: String,
}

// ---------------------------------------------------------------------------
// work-tree lifecycle
// ---------------------------------------------------------------------------


/// Delete a previous run's `<work>` tree. The tree may hold *read-only*
/// directories (the assets snapshot keeps its dirs at 0555, §5.3.1) and
/// read-only files (archive-normalized `pkg` contents); `unlink(2)` needs
/// a writable *parent*, so every directory is opened up before the delete.
/// Opening the dirs up is best effort — the delete itself is the real check.
///
/// The chmod must happen *before* the directory is listed: overlayfs leaves
/// its `workdir/work` at mode 000 inside the sandbox scratch (`sb/layer-work`),
/// and a walker that opens a directory before the caller can chmod it (as
/// `walkdir` does) fails with EACCES on exactly that leftover.
pub(crate) fn wipe_work(work: &Path) -> Result<()> {
    wipe_tree(work).with_context(|| format!("wipe the previous work tree {}", work.display()))
}


fn wipe_tree(path: &Path) -> std::io::Result<()> {
    let md = match std::fs::symlink_metadata(path) {
        Ok(md) => md,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    if !md.is_dir() {
        // files and symlinks (never followed) only need a writable parent,
        // which the caller opened up before descending
        return match std::fs::remove_file(path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        };
    }
    // open the directory up first, then list it
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700));
    for entry in std::fs::read_dir(path)? {
        wipe_tree(&entry?.path())?;
    }
    match std::fs::remove_dir(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}


pub(crate) fn build(rec: &Recipe, opts: &BuildOpts) -> Result<BuildResult> {
    // ---- offline gate (fetch-phase.md §12/§15) ----------------------------
    fetch::set_offline(opts.offline);
    // --offline and network-needing recipes are contradictory by design:
    // one forbids connections, the other requires them mid-build
    if opts.offline && rec.sandbox.network {
        bail!(
            "recipe {} sets [sandbox] network = true, which contradicts --offline \
             (NAWA_OFFLINE); drop one of the two",
            rec.id()
        );
    }

    // §14C: %{srcroot}/NAWA_SRCROOT only exist for new-schema recipes; a
    // legacy recipe using them would silently misbehave. Fails before any
    // download or closure work.
    rec.check_schema_placeholders()?;

    // In live/env modes the toolchain comes from the host root, not a pinned
    // closure archive. Capture the exact fetch PATH versions before cache lookup.
    let toolchain_identity = crate::fetchrun::build_toolchain_identity(rec, opts.isolation)?;

    // ---- build_id cache hit (opt-in, --use-cache) ------------------------
    // Everything the build id covers is known before any heavy work: the
    // recipe bytes, the pinned source sha256, the closure archive *names*
    // (versions) and the profile flags. A matching sidecar means the
    // archive in `out/` was produced from exactly these inputs.
    if opts.use_cache && opts.isolation >= Isolation::Env {
        let closure_paths: Vec<PathBuf> = if opts.isolation == Isolation::Closure {
            let known = crate::recipe::load_all(&opts.recipes_root)?;
            crate::closure::buildroot(rec, &known, opts.check != CheckMode::Off)
                .context("compute build root")?
                .iter()
                .filter_map(|n| known.get(n).map(|r| crate::closure::archive_for(r, &opts.outdir)))
                .collect()
        } else {
            Vec::new()
        };
        if let Some(hit) = try_cache_hit(rec, opts, &closure_paths, &toolchain_identity)? {
            return Ok(hit);
        }
    }

    // make scratch paths absolute — build scripts run with current_dir=srcdir,
    // so relative paths would resolve to the wrong location
    std::fs::create_dir_all(&opts.builddir)?;
    let builddir = std::fs::canonicalize(&opts.builddir)?;
    std::fs::create_dir_all(&opts.outdir)?;
    let outdir = std::fs::canonicalize(&opts.outdir)?;

    let name = &rec.package.name;
    // fail before a possibly hours-long compile, not after
    if rec.build.install.trim().is_empty() {
        bail!("recipe {} must define build.install", name);
    }
    let work = builddir.join(rec.id());
    let src = work.join("src");
    let pkg = work.join("pkg");

    wipe_work(&work)?;
    std::fs::create_dir_all(&src)?;
    std::fs::create_dir_all(&pkg)?;

    // ---- source staging -------------------------------------------------
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

    // ---- isolation prep (docs/design/build-isolation.md) -----------------
    // §6: new-schema recipes carry the epoch from the sources engine (the
    // newest regular-file mtime from the archives, checked before plain
    // files are stamped); legacy recipes keep the full-tree walk
    let sde = match &staged {
        Some(s) => s.source_date_epoch,
        None => buildenv::source_date_epoch(&srcdir, rec.package.source_date_epoch)?,
    };
    let sandboxed = opts.isolation >= Isolation::Sandbox;

    // the design pins /build as the inside build base (§4.2): it exists in
    // the stage chroots and a root nawa can create it. An unprivileged host
    // falls back to a base on the sandbox's private /tmp — recipes always
    // see it through %{srcdir}/%{pkgdir}, never as a hardcoded path.
    // `sandbox` level keeps the live root and mounts a fresh tmpfs on /tmp
    // BEFORE binding the staging dirs, which would hide a build dir under /tmp.
    // (`closure` mounts its tmpfs inside the assembled root and is unaffected.)
    if opts.isolation == Isolation::Sandbox {
        let w = std::fs::canonicalize(&work).unwrap_or_else(|_| work.clone());
        if w.starts_with("/tmp") {
            bail!(
                "--isolation sandbox cannot build under /tmp ({}): the sandbox's private /tmp \
                 would hide the build dir. Use another --builddir (e.g. /build or /var/tmp/nawa)",
                work.display()
            );
        }
    }
    let sandbox_base = if sandboxed { sandbox_inside_base(&work) } else { String::new() };

    // the build sees sandbox paths for %{srcdir}/%{pkgdir}; the spec carries
    // the host paths for the bind mounts
    // §4.3/§5.3: %{srcroot} is the workspace root and %{assetsdir} the
    // assets snapshot — both live at <work>/… and are bound inside the
    // sandbox under <build_inside>/…; inside the sandbox the whole staging
    // dir is bound at <base>/src, so the srcroot view is that same root
    let view_assets = match (&staged_assets, sandboxed) {
        (Some(a), false) => a.dir.clone(),
        (Some(a), true) => {
            let _ = a; // a.dir is `<work>/assets`; the sandbox binds it at <base>/assets
            PathBuf::from(format!("{}/assets", sandbox_base.trim_end_matches('/')))
        }
        (None, _) => PathBuf::new(),
    };
    let view_srcroot = if sandboxed {
        PathBuf::from(format!("{}/src", sandbox_base.trim_end_matches('/')))
    } else {
        src.clone()
    };
    let view_src = if sandboxed {
        // `srcdir` is the unpacked source dir (e.g. src/m4-1.4.19); the sandbox
        // binds the whole staging dir at <base>/src, so keep the sub-path
        let mut v = view_srcroot.clone();
        if let Ok(rel) = srcdir.strip_prefix(&src) {
            v.push(rel);
        }
        v
    } else {
        srcdir.clone()
    };
    let view_pkg = if sandboxed {
        PathBuf::from(format!("{}/pkg", sandbox_base.trim_end_matches('/')))
    } else {
        pkg.clone()
    };

    // ---- assemble the build root (closure level, §5) ----------------------
    let mut assembled: Option<crate::closure::AssembledRoot> = None;
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
            && layer_paths_are_mountable(&members, &known, &opts.outdir, &work)
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
                &outdir,
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
                &outdir,
                &root_dir,
                &opts.root,
                opts.allow_unsigned,
                |pkg| crate::nawa_info!("closure: extracted {pkg}"),
            )?
        });
    }

    // ---- ecosystem fetch (fetch-phase.md §7, §11, §12) --------------------
    // Runs after the closure assembly (the fetch sandbox reuses the same
    // build root: the toolchain lives in it) and before the build
    // environment is computed (the whitelist and the sandbox spec need the
    // per-build fetch view paths).
    let fetch_env: Vec<(String, String)> = if rec.fetch.any() {
        let srccache = crate::config::srccache_dir(&builddir, None);
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
            work: &work,
            src: &src,
            srccache: &srccache,
            sandboxed,
            build_inside: &sandbox_base,
            whitelist_home: &whitelist_home,
            assembled: assembled.as_ref(),
        };
        crate::fetchrun::run(&fr)?
    } else {
        Vec::new()
    };

    // ---- environment ------------------------------------------------------
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
        let spec_srcroot = has_sources.then(|| view_srcroot.clone());
        let spec_assetsdir = staged_assets.as_ref().map(|_| view_assets.clone());
        let spec_env = buildenv::EnvSpec {
            profile_env: &opts.env,
            jobs: opts.jobs,
            srcdir: &view_src,
            pkgdir: &view_pkg,
            home: &home,
            source_date_epoch: sde,
            // new-schema names (§4.3/§5.3); joined to the whitelist only
            // when the recipe enables them
            srcroot: spec_srcroot.as_deref(),
            assetsdir: spec_assetsdir.as_deref(),
            // [fetch.*] whitelist additions (§7): paths already phrased in
            // the build's frame by the fetch runner
            fetch_env: fetch_env.clone(),
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

    // ---- run build sections ---------------------------------------------
    let ctx = ExpandCtx {
        jobs: opts.jobs,
        pkgdir: view_pkg.clone(),
        srcdir: view_src.clone(),
        arch: rec.arch(),
        // §4.3: %{srcroot} exists only for recipes with [[package.sources]]
        // — an assets-only recipe has a staged workspace but no workspace
        // root (the sandbox binds it all the same; the NAME is just off)
        srcroot: (!rec.package.sources.is_empty()).then(|| view_srcroot.clone()),
        // §5.3: same rule for %{assetsdir}
        assetsdir: staged_assets.as_ref().map(|_| view_assets.clone()),
    };
    let arch = rec.arch();
    let sandbox_cfg = if sandboxed {
        let layers: Vec<String> = assembled
            .as_ref()
            .map(|a| {
                a.layer_dirs
                    .iter()
                    .map(|p| p.to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        Some(prepare_sandbox(
            rec, opts, &work, &src, &pkg, &env_vars, &sandbox_base, &view_src, layers,
        )?)
    } else {
        None
    };

    for (label, script) in [
        ("prepare", &rec.build.prepare),
        ("configure", &rec.build.configure),
        ("compile", &rec.build.compile),
        ("check", &rec.build.check),
        ("install", &rec.build.install),
    ] {
        if script.trim().is_empty() {
            continue;
        }
        if label == "check" {
            match opts.check {
                CheckMode::Off => continue,
                CheckMode::Fatal => {}
                CheckMode::Warn => {
                    if let Err(e) = run_section(
                        rec, label, script, &ctx, &srcdir, &work, &env_vars, env_prefix,
                        sandbox_cfg.as_ref(), opts,
                    ) {
                        crate::nawa_warn!("check failed for {} (continuing): {e:#}", rec.id());
                    }
                    continue;
                }
            }
        }
        run_section(
            rec, label, script, &ctx, &srcdir, &work, &env_vars, env_prefix,
            sandbox_cfg.as_ref(), opts,
        )?;
    }

    // ---- slim: cleanup, split off -doc/-lang, strip ----------------------
    let slim_report = if opts.slim {
        slim::run(&pkg, &work.join("split"), &rec.slim)?
    } else {
        slim::Report::default()
    };
    if let Some(line) = slim_report.summary() {
        println!("{line}");
    }
    // a sub-package archive left by an earlier build of this very version
    // must not outlive the rules that produced it
    for suffix in slim::SUFFIXES {
        let stale = slim::subpackage_file(name, suffix, &rec.package.version, rec.package.rel);
        let _ = std::fs::remove_file(outdir.join(stale));
    }

    // ---- collect staged files -------------------------------------------
    let mut files: Vec<String> = Vec::new();
    let mut size: u64 = 0;
    for e in walkdir::WalkDir::new(&pkg).sort_by_file_name() {
        let e = e?;
        let rel = e.path().strip_prefix(&pkg)?;
        if rel.as_os_str().is_empty() {
            continue;
        }
        files.push(format!("/{}", rel.display()));
        if let Ok(m) = e.metadata() {
            if m.is_file() {
                size += m.len();
            }
        }
    }
    if files.is_empty() {
        bail!("build produced no files — check the install section");
    }

    // ---- post-build checks ("lint", §7) ----------------------------------
    if opts.isolation >= Isolation::Env {
        let sonames: BTreeMap<String, String> = match &assembled {
            Some(root) => root.sonames.clone(),
            None => installed_sonames(&opts.root)?,
        };
        // inside the sandbox the build only ever sees /build/... (or the
        // fallback base); outside it the host build dir must not leak either
        let host_builddir: Option<String> = if sandboxed {
            if sandbox_base == "/build" {
                None // the hardcoded /build/src, /build/pkg checks cover it
            } else {
                Some(sandbox_base.clone())
            }
        } else {
            // Match this build's own work dir (e.g. /build/perl-5.44.0-1),
            // not the bare builddir: with builddir=/build the bare path is a
            // substring of innocent doc text ("/build", "/builddir"...) and
            // flagged 24 false positives in perl's man pages and modules.
            Some(work.to_string_lossy().into_owned())
        };
        let ctx = crate::lint::LintCtx {
            pkgdir: &pkg,
            rundeps: &rec.depends.run,
            sonames: &sonames,
            host_builddir: host_builddir.as_deref(),
            // §5.3/§6: the staged snapshot and the per-build fetch views
            // are <build_inside>/assets and <build_inside>/fetch/… inside
            // the sandbox; recipes that enable them must not leak those
            // paths into the archive
            extra_leak_paths: {
                let mut leaks: Vec<&'static str> = Vec::new();
                if staged_assets.is_some() {
                    leaks.push("/build/assets");
                }
                if rec.fetch.any() {
                    leaks.push("/build/fetch");
                }
                leaks
            },
        };
        let rep = crate::lint::run(&ctx);
        for w in &rep.warnings {
            crate::nawa_warn!("lint: {w}");
        }
        for e in &rep.errors {
            crate::nawa_info_stderr!("lint: {e}");
        }
        if !rep.ok() {
            bail!(
                "{} lint error(s) in {} — the archive was NOT created",
                rep.errors.len(),
                rec.id()
            );
        }
        if rep.elf_count > 0 {
            crate::nawa_info!("lint: checked {} ELF file(s)", rep.elf_count);
        }
    }

    // ---- .PKGINFO + archive ----------------------------------------------
    let mut pkginfo = format!(
        "pkgname = {}\npkgver = {}\npkgrel = {}\narch = {}\nrundeps = {}\nsize = {}\n",
        name,
        rec.package.version,
        rec.package.rel,
        arch,
        rec.depends.run.join(","),
        size,
    );
    if opts.isolation >= Isolation::Env {
        // deterministic inputs instead of a wall clock (§6): install time
        // lives in the database (install_date), not in the archive
        pkginfo.push_str(&format!("source_date_epoch = {sde}\n"));
        let closure_shas: Vec<PathBuf> = assembled
            .as_ref()
            .map(|a| a.archives.clone())
            .unwrap_or_default();
        let bid = build_id_for(rec, opts, &closure_shas, staged_assets.as_ref().map(|a| a.digest.as_str()), &toolchain_identity);
        pkginfo.push_str(&format!("build_id = {bid}\n"));
        if opts.allow_network && rec.sandbox.network {
            pkginfo.push_str("impure = network\n");
        }
    } else {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        pkginfo.push_str(&format!("build_date = {now}\n"));
    }

    let out_path = outdir.join(format!("{}.nawa.tar.zst", rec.id()));
    let hooks = archive::Hooks {
        post_install: rec.hooks.post_install.clone(),
        pre_remove: rec.hooks.pre_remove.clone(),
        post_remove: rec.hooks.post_remove.clone(),
    };

    crate::nawa_info!(
        "[{}] isolation={} sde={}",
        rec.id(),
        opts.isolation.as_str(),
        sde
    );
    if opts.isolation >= Isolation::Env {
        archive::create_normalized(
            &pkg,
            &pkginfo,
            Some(&hooks),
            &out_path,
            &archive::Normalize { source_date_epoch: sde },
        )?;
        // the cache-hit sidecar (--use-cache): the build id this archive
        // was produced from, next to the archive itself
        let bid = pkginfo
            .split("build_id = ")
            .nth(1)
            .and_then(|s| s.lines().next())
            .unwrap_or("");
        let sidecar = buildid_sidecar(&out_path);
        let _ = std::fs::write(&sidecar, format!("{bid}\n"));
    } else {
        archive::create_with_hooks(&pkg, &pkginfo, Some(&hooks), &out_path)?;
    }

    // ---- sub-packages (-doc, -lang) --------------------------------------
    // Same version/rel, build inputs and normalisation as the main archive;
    // each depends on it. repo-create / sign / upload pick them up by name.
    for sp in &slim_report.subpackages {
        let sub_name = format!("{}-{}", name, sp.suffix);
        let sub_info = slim::derive_pkginfo(&pkginfo, name, &sub_name, sp.size);
        let sub_path = outdir.join(slim::subpackage_file(
            name,
            sp.suffix,
            &rec.package.version,
            rec.package.rel,
        ));
        if opts.isolation >= Isolation::Env {
            archive::create_normalized(
                &sp.dir,
                &sub_info,
                None,
                &sub_path,
                &archive::Normalize { source_date_epoch: sde },
            )?;
        } else {
            archive::create_with_hooks(&sp.dir, &sub_info, None, &sub_path)?;
        }
        crate::nawa_success!(
            "built {sub_name}-{}-{} ({} files) -> {}",
            rec.package.version,
            rec.package.rel,
            sp.files,
            sub_path.display()
        );
    }

    // v1/v2 assembly cleanup: the extract-per-build root is per-build
    // scratch; a layered root is only the empty mountpoint
    if let Some(root) = &assembled {
        let _ = std::fs::remove_dir_all(&root.dir);
    }

    crate::nawa_success!(
        "built {} ({} files){} -> {}",
        rec.id(),
        files.len(),
        if assembled.as_ref().is_some_and(|a| a.layered) {
            " [build root: layer cache]"
        } else {
            ""
        },
        out_path.display()
    );

    Ok(BuildResult { archive: out_path, pkginfo })
}


/// Make the staged source tree writable for the build itself under
/// `sandbox`/`closure` isolation: GNU tar run as root preserves the tarball's
/// upstream ownership (many tarballs ship uid 1000/501), and the sandbox
/// drops every capability (§4.1) — a tree owned by a foreign uid then yields
/// plain `EACCES` for namespace-root (`Configure` must create files inside
/// its own source dir). The engine still runs privileged here: normalise
/// ownership to the extracting user and grant `u+rwX` (owner read/write
/// everywhere, +x only on dirs and executables). Best effort: a rootless
/// engine keeps the extracted tree as-is (it already owns it).
///
/// This never removes permission bits, never touches mtimes (so
/// `SOURCE_DATE_EPOCH` is unaffected) and never runs through a symlink.
pub(crate) fn grant_source_write(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let euid = rustix::process::geteuid().as_raw();
    let root = euid == 0;
    for e in walkdir::WalkDir::new(dir).into_iter().flatten() {
        // chmod(2)/chown(2) would follow the link — never grant through one
        if e.file_type().is_symlink() {
            continue;
        }
        let path = e.path();
        if root {
            let _ = std::os::unix::fs::chown(path, Some(euid), Some(euid));
        }
        let Ok(md) = e.metadata() else { continue };
        let mode = md.permissions().mode() & 0o7777;
        let x = if md.is_dir() || mode & 0o111 != 0 { 0o100 } else { 0o000 };
        let want = (mode & !0o700) | 0o600 | x;
        if want != mode {
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(want));
        }
    }
}


/// Where the sandbox's build base lives (§4.2): `/build` — recipes always
/// see `%{srcdir}`/`%{pkgdir}` as `/build/src`, `/build/pkg`, never a host
/// path — with a deterministic fallback for hosts where `/build` cannot
/// exist (rootless).
///
/// Even when the staging dirs live *under* `/build` (the bootstrap stages
/// use `--builddir /build`) the base stays `/build`: the sandbox covers the
/// base with a fresh tmpfs and the staging binds go through pre-opened
/// O_PATH handles (sandbox.rs `StagingHandles`), so the cover no longer
/// hides the bind sources — the old "base must move elsewhere" workaround
/// leaked `/tmp/.nawa-sandbox` into recipe artifacts (`bash`'s installed
/// Makefile.inc records its build tree) and the lint rightly flagged it.
pub(crate) fn sandbox_inside_base(_work: &Path) -> String {
    if Path::new("/build").is_dir() || std::fs::create_dir("/build").is_ok() {
        return "/build".to_string();
    }
    eprintln!(
        "nawa: sandbox: note: /build is not creatable on this host — \
         staging under the sandbox's private /tmp instead"
    );
    "/tmp/.nawa-sandbox".to_string()
}

// ---------------------------------------------------------------------------
// Phase 5: layer cache + build_id cache hits (§5.3 v2, §7)
// ---------------------------------------------------------------------------


/// `nawa build --rebuild-check` (§6): build twice in different directories at
/// different times and compare archive hashes — the regression test for the
/// whole reproducibility pipeline.
pub(crate) fn rebuild_check(rec: &Recipe, opts: &BuildOpts) -> Result<()> {
    let mut results: Vec<(PathBuf, String)> = Vec::new();
    for tag in ["rebuild-a", "rebuild-b"] {
        let mut o = opts.clone();
        o.builddir = opts.builddir.join(tag);
        o.isolation = o.isolation.max(Isolation::Env);
        // --rebuild-check exists to catch non-reproducible builds; a cache
        // hit would compare the archive with itself and prove nothing
        o.use_cache = false;
        crate::nawa_info!("rebuild-check: building {tag} in {}", o.builddir.display());
        let r = build(rec, &o)?;
        let mut h = sha2::Sha256::new();
        use sha2::Digest;
        h.update(std::fs::read(&r.archive)?);
        results.push((r.archive, hex::encode(h.finalize())));
        // let the wall clock move so a non-normalised timestamp would show
        std::thread::sleep(std::time::Duration::from_secs(2));
    }
    let (a_path, a_hash) = &results[0];
    let (b_path, b_hash) = &results[1];
    if a_hash == b_hash {
        crate::nawa_info!("rebuild-check: OK — {a_hash} ({})", a_path.display());
        return Ok(());
    }
    crate::nawa_info_stderr!("rebuild-check: MISMATCH");
    eprintln!("  run A: {a_hash} {}", a_path.display());
    eprintln!("  run B: {b_hash} {}", b_path.display());
    let diffs = diff_archives(a_path, b_path)?;
    if diffs.is_empty() {
        eprintln!("  (archives differ byte-wise but member hashes match — compression or ordering is not deterministic)");
    } else {
        eprintln!("  differing members:");
        for d in diffs {
            eprintln!("    {d}");
        }
    }
    bail!("rebuild-check failed for {}", rec.id());
}


pub mod cache;
pub mod sandbox;
pub mod sections;

#[allow(unused_imports)]
pub(crate) use cache::{build_id_for, try_cache_hit, buildid_sidecar, fetch_ecosystems};
#[allow(unused_imports)]
pub(crate) use sandbox::{layer_cache_dir, layer_paths_are_mountable, overlay_probe_ok, prepare_sandbox};
#[allow(unused_imports)]
pub(crate) use sections::{diff_archives, installed_sonames, run_section};

#[cfg(test)]
mod fetch_toolchain_identity_tests {
    use super::*;

    fn fetch_recipe() -> Recipe {
        let raw = r#"
[package]
name = "fetch-id-test"
version = "1.0"
rel = 1
arch = ["x86_64"]
source = "local:/nawa-schema-2-requires-a-newer-nawa"

[fetch.go]
dir = "."
"#;
        let mut rec: Recipe = toml::from_str(raw).unwrap();
        rec.raw = raw.to_string();
        rec
    }

    #[test]
    fn host_toolchain_version_changes_build_identity() {
        let rec = fetch_recipe();
        let opts = BuildOpts::default();
        let go_a = vec![("go", "go version go1.26.0 linux/amd64".to_string())];
        let go_b = vec![("go", "go version go1.27.0 linux/amd64".to_string())];
        let id_a = build_id_for(&rec, &opts, &[], None, &go_a);
        let id_b = build_id_for(&rec, &opts, &[], None, &go_b);
        assert_ne!(id_a, id_b, "a Go toolchain update must invalidate --use-cache");
    }

    #[test]
    fn cargo_toolchain_version_changes_build_identity() {
        let raw = r#"
[package]
name = "fetch-cargo-id-test"
version = "1.0"
rel = 1
arch = ["x86_64"]
source = "local:/nawa-schema-2-requires-a-newer-nawa"

[fetch.cargo]
dir = "."
"#;
        let mut rec: Recipe = toml::from_str(raw).unwrap();
        rec.raw = raw.to_string();
        let opts = BuildOpts::default();
        let cargo_a = vec![("cargo", "cargo 1.88.0 (test-a)".to_string())];
        let cargo_b = vec![("cargo", "cargo 1.89.0 (test-b)".to_string())];
        let id_a = build_id_for(&rec, &opts, &[], None, &cargo_a);
        let id_b = build_id_for(&rec, &opts, &[], None, &cargo_b);
        assert_ne!(id_a, id_b, "a Cargo toolchain update must invalidate --use-cache");
    }

    #[test]
    fn closure_toolchain_identity_is_carried_by_archives_not_host_versions() {
        let rec = fetch_recipe();
        let opts = BuildOpts::default();
        let archives = vec![PathBuf::from("golang-1.27.3-1.nawa.tar.zst")];
        let id_a = build_id_for(&rec, &opts, &archives, None, &[]);
        let id_b = build_id_for(&rec, &opts, &archives, None, &[]);
        assert_eq!(id_a, id_b);
        let updated_archives = vec![PathBuf::from("golang-1.27.4-1.nawa.tar.zst")];
        let id_updated = build_id_for(&rec, &opts, &updated_archives, None, &[]);
        assert_ne!(id_a, id_updated, "a closure toolchain archive update must invalidate the id");
    }
}

#[cfg(test)]
mod wipe_tests {
    use super::wipe_work;
    use std::os::unix::fs::PermissionsExt;

    /// overlayfs leaves `<workdir>/work` at mode 000 (and the assets snapshot
    /// keeps 0555 dirs); neither may stop the next build from wiping its tree.
    #[test]
    fn wipe_removes_mode_000_and_read_only_trees() {
        let root = std::env::temp_dir().join(format!("nawa-wipe-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let hidden = root.join("sb/layer-work/work/index");
        std::fs::create_dir_all(&hidden).unwrap();
        std::fs::write(hidden.join("f"), b"x").unwrap();
        std::fs::set_permissions(&hidden, std::fs::Permissions::from_mode(0o000)).unwrap();
        std::fs::set_permissions(
            root.join("sb/layer-work/work"),
            std::fs::Permissions::from_mode(0o000),
        )
        .unwrap();
        let ro = root.join("src/assets/files");
        std::fs::create_dir_all(&ro).unwrap();
        std::fs::write(ro.join("a"), b"y").unwrap();
        std::fs::set_permissions(ro.join("a"), std::fs::Permissions::from_mode(0o444)).unwrap();
        std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o555)).unwrap();

        wipe_work(&root).unwrap();
        assert!(!root.exists());
        // a missing tree is not an error
        wipe_work(&root).unwrap();
    }
}

#[cfg(test)]
mod installed_sonames_tests {
    use super::installed_sonames;
    use crate::db::{Db, Entry};

    fn entry(name: &str) -> Entry {
        Entry {
            name: name.into(),
            version: "1.0".into(),
            rel: 1,
            arch: String::new(),
            reason: "explicit".to_string(),
            run: vec![],
            install_date: 0,
            size: 0,
            artifact_sha256: String::new(),
        }
    }

    #[test]
    fn sonames_map_libs_to_their_owners() {
        let root = std::env::temp_dir().join(format!(
            "nawa-sonames-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let db = Db::new(&root);
        db.add(
            &entry("hello"),
            &[
                "/usr/lib/libfoo.so.1".to_string(),
                "/usr/bin/hello".to_string(),
            ],
            &Default::default(),
        )
        .unwrap();
        db.add(
            &entry("world"),
            &[
                "/lib64/libbar.so".to_string(),
                "/usr/lib/libbaz.so.a".to_string(),
                "/usr/lib/notalib".to_string(),
                "/etc/world.conf".to_string(),
            ],
            &Default::default(),
        )
        .unwrap();

        let map = installed_sonames(&root).unwrap();
        assert_eq!(map.get("libfoo.so.1").map(String::as_str), Some("hello"));
        assert_eq!(map.get("libbar.so").map(String::as_str), Some("world"));
        assert!(!map.contains_key("libbaz.so.a"), ".a files are not sonames: {map:?}");
        assert_eq!(map.len(), 2, "only lib sonames, owners correct: {map:?}");

        let _ = std::fs::remove_dir_all(&root);
    }
}
