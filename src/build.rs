//! Build engine: fetch -> verify -> extract -> [assemble build root] ->
//! run build sections (optionally inside the sandbox) -> stage into pkgdir
//! -> lint -> create archive.

use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::archive;
use crate::buildenv::{self, Isolation};
use crate::fetch;
use crate::recipe::{ExpandCtx, Recipe};

/// What to do with a recipe's `[build] check` section (its test suite).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckMode {
    /// Skip it (default — test suites are slow and some, like glibc's, have
    /// known failures).
    Off,
    /// Run it after `compile`; a failure aborts the build.
    Fatal,
    /// Run it after `compile`; a failure only prints a warning.
    Warn,
}

#[derive(Debug, Clone)]
pub struct BuildOpts {
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
        }
    }
}

/// How many trailing output lines are quoted when a section fails.
const FAIL_TAIL_LINES: usize = 25;

pub struct BuildResult {
    pub archive: PathBuf,
    #[allow(dead_code)]
    pub pkginfo: String,
}

pub fn build(rec: &Recipe, opts: &BuildOpts) -> Result<BuildResult> {
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
        if let Some(hit) = try_cache_hit(rec, opts, &closure_paths)? {
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

    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&src)?;
    std::fs::create_dir_all(&pkg)?;

    // ---- source staging -------------------------------------------------
    let ctx = ExpandCtx {
        jobs: opts.jobs,
        pkgdir: pkg.clone(),
        srcdir: src.clone(),
        arch: rec.arch(),
    };

    let source = &rec.package.source;
    let srcdir: PathBuf = if source.is_empty() {
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
        let cache_dir = std::env::var_os("NAWA_SRCCACHE")
            .map(PathBuf::from)
            .unwrap_or_else(|| builddir.join("distfiles"));
        std::fs::create_dir_all(&cache_dir)
            .with_context(|| format!("create source cache {}", cache_dir.display()))?;
        let sha = rec.package.sha256.as_str();
        let tarball = cache_dir.join(format!("{}-{}", sha.get(..16).unwrap_or("nosha"), fname));
        if tarball.exists() && fetch::verify_sha256(&tarball, sha).is_ok() {
            println!("nawa: source cached: {}", tarball.display());
        } else {
            println!("nawa: downloading {source}");
        }
        fetch::download_cached(&source, &tarball, Some(sha))?;
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
    let sde = buildenv::source_date_epoch(&srcdir, rec.package.source_date_epoch)?;
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
    let view_src = if sandboxed {
        // `srcdir` is the unpacked source dir (e.g. src/m4-1.4.19); the sandbox
        // binds the whole staging dir at <base>/src, so keep the sub-path
        let mut v = PathBuf::from(format!("{}/src", sandbox_base.trim_end_matches('/')));
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
        println!(
            "nawa: closure: {} package(s) in the build root ({}{}): {}",
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
            println!("nawa: closure: assembling the build root from the layer cache (overlayfs)");
            let cache = layer_cache.expect("checked above");
            crate::closure::assemble_cached(
                &members,
                &known,
                &outdir,
                &root_dir,
                &cache,
                &opts.root,
                opts.allow_unsigned,
                |pkg| println!("nawa: closure: extracted {pkg} (new layer)"),
            )?
        } else {
            println!("nawa: closure: assembling the build root (extract-per-build)");
            crate::closure::assemble(
                &members,
                &known,
                &outdir,
                &root_dir,
                &opts.root,
                opts.allow_unsigned,
                |pkg| println!("nawa: closure: extracted {pkg}"),
            )?
        });
    }

    // ---- environment ------------------------------------------------------
    let (env_vars, env_prefix) = if opts.isolation >= Isolation::Env {
        let home = if sandboxed {
            format!("{}/home", sandbox_base.trim_end_matches('/'))
        } else {
            buildenv::SANDBOX_HOME.to_string()
        };
        let spec_env = buildenv::EnvSpec {
            isolation: opts.isolation,
            profile_env: &opts.env,
            jobs: opts.jobs,
            srcdir: &view_src,
            pkgdir: &view_pkg,
            home: &home,
            source_date_epoch: sde,
        };
        (buildenv::whitelist(&spec_env), opts.isolation.script_prefix())
    } else {
        (opts.env.clone(), "")
    };

    // ---- run build sections ---------------------------------------------
    let ctx = ExpandCtx {
        jobs: opts.jobs,
        pkgdir: view_pkg.clone(),
        srcdir: view_src.clone(),
        arch: rec.arch(),
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
                        eprintln!("nawa: warning: check failed for {} (continuing): {e:#}", rec.id());
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

    // ---- collect staged files -------------------------------------------
    let mut files: Vec<String> = Vec::new();
    for e in walkdir::WalkDir::new(&pkg).sort_by_file_name() {
        let e = e?;
        let rel = e.path().strip_prefix(&pkg)?;
        if rel.as_os_str().is_empty() {
            continue;
        }
        files.push(format!("/{}", rel.display()));
    }
    if files.is_empty() {
        bail!("build produced no files — check the install section");
    }

    // ---- post-build checks ("lint", §7) ----------------------------------
    if opts.isolation >= Isolation::Env {
        let sonames: BTreeMap<String, String> = match &assembled {
            Some(root) => root.sonames.clone(),
            None => installed_sonames(&opts.root).unwrap_or_default(),
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
        };
        let rep = crate::lint::run(&ctx);
        for w in &rep.warnings {
            eprintln!("nawa: lint: warning: {w}");
        }
        for e in &rep.errors {
            eprintln!("nawa: lint: {e}");
        }
        if !rep.ok() {
            bail!(
                "{} lint error(s) in {} — the archive was NOT created",
                rep.errors.len(),
                rec.id()
            );
        }
        if rep.elf_count > 0 {
            println!("nawa: lint: checked {} ELF file(s)", rep.elf_count);
        }
    }

    // ---- .PKGINFO + archive ----------------------------------------------
    let size: u64 = walkdir::WalkDir::new(&pkg)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter_map(|e| e.metadata().ok())
        .filter(|m| m.is_file())
        .map(|m| m.len())
        .sum();

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
        let bid = build_id_for(rec, opts, &closure_shas);
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

    println!(
        "nawa: [{}] isolation={} sde={}",
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

    // v1/v2 assembly cleanup: the extract-per-build root is per-build
    // scratch; a layered root is only the empty mountpoint
    if let Some(root) = &assembled {
        let _ = std::fs::remove_dir_all(&root.dir);
    }

    println!(
        "nawa: built {} ({} files){} -> {}",
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
fn grant_source_write(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let euid = unsafe { libc::geteuid() };
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
fn sandbox_inside_base(_work: &Path) -> String {
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

/// The build id for `rec` as `build()` would record it: recipe bytes, pinned
/// source sha256, the closure archive set (names pin versions) and flags.
fn build_id_for(rec: &Recipe, opts: &BuildOpts, closure_archives: &[PathBuf]) -> String {
    crate::lint::build_id(
        rec.raw.as_bytes(),
        &rec.package.sha256,
        closure_archives,
        &opts.profile_flags,
    )
}

/// `<out>/<archive>.buildid` — the sidecar `--use-cache` compares against.
fn buildid_sidecar(out_path: &Path) -> PathBuf {
    out_path.with_file_name(format!(
        "{}.buildid",
        out_path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default()
    ))
}

/// Return the existing archive when its sidecar build id matches the id the
/// current inputs would produce (§7: "is this archive up to date?" becomes
/// a hash comparison). `Ok(None)` = no hit, build as usual.
fn try_cache_hit(
    rec: &Recipe,
    opts: &BuildOpts,
    closure_archives: &[PathBuf],
) -> Result<Option<BuildResult>> {
    let out_path = opts.outdir.join(format!("{}.nawa.tar.zst", rec.id()));
    let sidecar = buildid_sidecar(&out_path);
    if !out_path.is_file() || !sidecar.is_file() {
        return Ok(None);
    }
    let stored = std::fs::read_to_string(&sidecar).unwrap_or_default();
    let bid = build_id_for(rec, opts, closure_archives);
    if stored.trim() != bid {
        println!("nawa: build cache: {} changed (build_id mismatch) — rebuilding", rec.id());
        return Ok(None);
    }
    let pkginfo = archive::read_pkginfo(&out_path)?;
    println!(
        "nawa: build cache hit: {} (build_id {bid}) — nothing to do",
        out_path.display()
    );
    Ok(Some(BuildResult { archive: out_path, pkginfo }))
}

/// The layer cache root (§5.3 v2): `NAWA_LAYER_CACHE` or `<root>/var/cache/nawa`;
/// one extracted directory per archive goes under it at `<cache>/layers/<sha>`
/// (`assemble_cached`). `None` when it cannot exist (unwritable root, or the
/// user disabled it with `NAWA_NO_LAYER_CACHE=1`) — the build then falls back
/// to extract-per-build assembly.
fn layer_cache_dir(opts: &BuildOpts) -> Option<PathBuf> {
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
fn layer_paths_are_mountable(
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
fn overlay_probe_ok() -> bool {
    let Ok(exe) = std::env::current_exe() else { return false };
    let Ok(out) = Command::new(exe).args(["sandbox-probe", "overlay"]).output() else {
        return false;
    };
    out.status.success()
}

/// `nawa build --rebuild-check` (§6): build twice in different directories at
/// different times and compare archive hashes — the regression test for the
/// whole reproducibility pipeline.
pub fn rebuild_check(rec: &Recipe, opts: &BuildOpts) -> Result<()> {
    let mut results: Vec<(PathBuf, String)> = Vec::new();
    for tag in ["rebuild-a", "rebuild-b"] {
        let mut o = opts.clone();
        o.builddir = opts.builddir.join(tag);
        o.isolation = o.isolation.max(Isolation::Env);
        // --rebuild-check exists to catch non-reproducible builds; a cache
        // hit would compare the archive with itself and prove nothing
        o.use_cache = false;
        println!("nawa: rebuild-check: building {tag} in {}", o.builddir.display());
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
        println!("nawa: rebuild-check: OK — {a_hash} ({})", a_path.display());
        return Ok(());
    }
    eprintln!("nawa: rebuild-check: MISMATCH");
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

/// Walk two archives in lockstep and list members whose content hashes
/// differ (name, hash A, hash B).
fn diff_archives(a: &Path, b: &Path) -> Result<Vec<String>> {
    use sha2::Digest;
    let index = |p: &Path| -> Result<BTreeMap<String, String>> {
        let f = std::fs::File::open(p)?;
        let dec = zstd::Decoder::new(f)?;
        let mut ar = tar::Archive::new(dec);
        let mut out = BTreeMap::new();
        for entry in ar.entries()? {
            let mut entry = entry?;
            let name = entry.path()?.to_string_lossy().into_owned();
            let mut buf = Vec::new();
            std::io::Read::read_to_end(&mut entry, &mut buf)?;
            out.insert(name, hex::encode(sha2::Sha256::digest(&buf)));
        }
        Ok(out)
    };
    let (ia, ib) = (index(a)?, index(b)?);
    let mut diffs = Vec::new();
    for name in ia.keys().chain(ib.keys()).collect::<std::collections::BTreeSet<_>>() {
        let ha = ia.get(name).map(|s| s.as_str()).unwrap_or("<absent>");
        let hb = ib.get(name).map(|s| s.as_str()).unwrap_or("<absent>");
        if ha != hb {
            diffs.push(format!("{name}: {ha} != {hb}"));
        }
    }
    Ok(diffs)
}

/// The soname → owner index for `sandbox`-level lint: the installed db.
fn installed_sonames(root: &Path) -> Result<BTreeMap<String, String>> {
    use std::collections::HashMap;
    let db = crate::db::Db::new(root);
    let mut out = BTreeMap::new();
    let owners: HashMap<String, String> = db
        .file_owners()
        .unwrap_or_default()
        .into_iter()
        .map(|(k, v)| (k.trim_start_matches('/').to_string(), v))
        .collect();
    for name in owners.keys() {
        for f in db.files(name)? {
            let f = f.trim_start_matches('/');
            if let Some(base) = f.rsplit('/').next() {
                let is_libdir = f.starts_with("usr/lib") || f.starts_with("lib");
                if is_libdir
                    && base.starts_with("lib")
                    && base.contains(".so")
                    && !base.ends_with(".a")
                    && !base.ends_with(".la")
                {
                    out.entry(base.to_string()).or_insert_with(|| name.clone());
                }
            }
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Section execution
// ---------------------------------------------------------------------------

/// Everything the sandbox needs for one build, prepared once.
struct SandboxCfg {
    spec_path: PathBuf,
    base: crate::sandbox::SandboxSpec,
}

#[allow(clippy::too_many_arguments)]
fn prepare_sandbox(
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

/// Run one build section under `bash -e` — directly at `none`/`env`, inside
/// the sandbox at `sandbox`/`closure` — streaming its combined stdout+stderr
/// to the console as it is produced and to `<work>/build-<label>.log`. On
/// failure the last lines are quoted.
#[allow(clippy::too_many_arguments)]
fn run_section(
    rec: &Recipe,
    label: &str,
    script: &str,
    ctx: &ExpandCtx,
    srcdir: &Path,
    work: &Path,
    envs: &[(String, String)],
    env_prefix: &str,
    sandbox_cfg: Option<&SandboxCfg>,
    opts: &BuildOpts,
) -> Result<()> {
    let expanded = rec.expand(script, ctx);
    println!("nawa: [{}] running {} (isolation: {})", rec.id(), label, opts.isolation.as_str());

    let log_path = work.join(format!("build-{label}.log"));
    let mut log = std::fs::File::create(&log_path)
        .with_context(|| format!("create {}", log_path.display()))?;

    // `exec 2>&1` merges stderr into the single stdout pipe, so the output
    // keeps its real ordering and one reader is enough.
    let wrapped = format!("exec 2>&1\n{env_prefix}{expanded}");

    let mut child = if let Some(cfg) = sandbox_cfg {
        let mut spec = cfg.base.clone();
        spec.script = wrapped;
        std::fs::write(&cfg.spec_path, toml::to_string_pretty(&spec)?)
            .with_context(|| format!("write {}", cfg.spec_path.display()))?;
        let exe = std::env::current_exe().context("locate the nawa binary")?;
        let (child, sock) = crate::sandbox::spawn_init(&exe, &cfg.spec_path)
            .context("spawn nawa sandbox-init")?;
        // install the ranged uid map when we are root, then release the child
        crate::sandbox::parent_sync(&child, sock);
        child
    } else {
        Command::new("bash")
            .args(["-e", "-c", &wrapped])
            .current_dir(srcdir)
            .envs(envs.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .env("NAWA_JOBS", ctx.jobs.to_string())
            .env("NAWA_PKGDIR", &ctx.pkgdir)
            .env("NAWA_SRCDIR", &ctx.srcdir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .spawn()
            .context("failed to spawn bash")?
    };

    let mut reader = BufReader::new(child.stdout.take().context("no stdout pipe")?);
    let mut tail: std::collections::VecDeque<String> =
        std::collections::VecDeque::with_capacity(FAIL_TAIL_LINES + 1);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        // read_until: tolerant of non-UTF-8 bytes and of very long lines
        if reader.read_until(b'\n', &mut buf)? == 0 {
            break;
        }
        let _ = log.write_all(&buf);
        {
            let mut out = std::io::stdout().lock();
            let _ = out.write_all(&buf);
            let _ = out.flush();
        }
        if tail.len() == FAIL_TAIL_LINES {
            tail.pop_front();
        }
        tail.push_back(String::from_utf8_lossy(&buf).trim_end().to_string());
    }
    let status = child.wait().context("waiting for the build section")?;

    if !status.success() {
        bail!(
            "{} failed with status {} — log: {}\n--- last output ---\n{}",
            label,
            status,
            log_path.display(),
            tail.into_iter().collect::<Vec<_>>().join("\n")
        );
    }
    Ok(())
}

