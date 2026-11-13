//! Build engine: fetch -> verify -> extract -> [assemble build root] ->
//! run build sections (optionally inside the sandbox) -> stage into pkgdir
//! -> lint -> create archive.

use anyhow::{bail, Context, Result};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use crate::buildenv::{self, Isolation};
use crate::fetch;
use crate::recipe::{ExpandCtx, Recipe};
use crate::slim;


pub(crate) use crate::buildenv::{BuildOpts, CheckMode};


/// How many trailing output lines are quoted when a section fails.
const FAIL_TAIL_LINES: usize = 25;


pub(crate) struct BuildResult {
    pub archive: PathBuf,
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

    // The closure build root is needed for the cache identity and again when
    // the build root is assembled. Keep the graph parsed for the cache lookup
    // so a cache miss does not read every `recipe.toml` twice; without the
    // cache the assembly step loads it as before.
    let mut closure_known: Option<std::collections::BTreeMap<String, Recipe>> = None;

    // In live/env modes the toolchain comes from the host root, not a pinned
    // closure archive. Capture the exact fetch PATH versions before cache lookup.
    let toolchain_identity = crate::fetchrun::build_toolchain_identity(rec, opts.isolation)?;
    // Cache-only identity (never part of the legacy `.PKGINFO build_id`):
    // the effective isolation level and, for host-root builds, the C/C++
    // toolchain the build sections will actually invoke.
    let host_toolchain_identity = cache::host_toolchain_identity(opts.isolation);

    // ---- build_id cache hit (opt-in, --use-cache) ------------------------
    // Everything the build id covers is known before any heavy work: the
    // recipe bytes, the pinned source sha256, the closure archive *names*
    // (versions) and the profile flags. A matching sidecar means the
    // archive in `out/` was produced from exactly these inputs.
    if opts.use_cache && opts.isolation >= Isolation::Env {
        let closure_paths: Vec<PathBuf> = if opts.isolation == Isolation::Closure {
            let known = crate::recipe::load_all(&opts.recipes_root)?;
            let paths = crate::closure::buildroot(rec, &known, opts.check != CheckMode::Off)
                .context("compute build root")?
                .iter()
                .filter_map(|n| known.get(n).map(|r| crate::closure::archive_for(r, &opts.outdir)))
                .collect();
            closure_known = Some(known);
            paths
        } else {
            Vec::new()
        };
        if let Some(hit) = try_cache_hit(
            rec,
            opts,
            &closure_paths,
            &toolchain_identity,
            &host_toolchain_identity,
        )? {
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
    let StagedSource { srcdir, sources: staged, assets: staged_assets } =
        stage::stage_source(rec, opts, &builddir, &work, &src, &pkg)?;

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
    let sandbox_base = stage::sandbox_base_for(opts, &work)?;
    let view = stage::build_view(
        sandboxed,
        &sandbox_base,
        staged_assets.as_ref().map(|a| a.dir.as_path()),
        &src,
        &srcdir,
        &pkg,
    );

    // ---- assemble the build root (closure level, §5) ----------------------
    let assembled =
        buildroot::assemble_build_root(rec, opts, &work, &outdir, closure_known.as_ref())?;

    // ---- ecosystem fetch (fetch-phase.md §7, §11, §12) --------------------
    let fetch_env = buildroot::run_fetch_phase(
        rec, opts, &builddir, &work, &src, sandboxed, &sandbox_base, assembled.as_ref(),
    )?;

    // ---- environment ------------------------------------------------------
    let (env_vars, env_prefix) = buildroot::build_env(
        rec, opts, sandboxed, &sandbox_base, &view, staged_assets.is_some(), sde, &fetch_env,
    );

    // ---- run build sections ---------------------------------------------
    let ctx = ExpandCtx {
        jobs: opts.jobs,
        pkgdir: view.pkg.clone(),
        srcdir: view.src.clone(),
        arch: rec.arch(),
        // §4.3: %{srcroot} exists only for recipes with [[package.sources]]
        // — an assets-only recipe has a staged workspace but no workspace
        // root (the sandbox binds it all the same; the NAME is just off)
        srcroot: (!rec.package.sources.is_empty()).then(|| view.srcroot.clone()),
        // §5.3: same rule for %{assetsdir}
        assetsdir: staged_assets.as_ref().map(|_| view.assets.clone()),
    };
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
            rec, opts, &work, &src, &pkg, &env_vars, &sandbox_base, &view.src, layers,
        )?)
    } else {
        None
    };
    run_sections(
        rec, opts, &ctx, &srcdir, &work, &env_vars, env_prefix, sandbox_cfg.as_ref(),
    )?;

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

    // ---- collect staged files, lint, archive -------------------------------
    let (files, size) = package::collect_staged_files(&pkg)?;
    if opts.isolation >= Isolation::Env {
        package::lint_stage(
            rec, opts, &pkg, &work, assembled.as_ref(), sandboxed, &sandbox_base,
            staged_assets.is_some(),
        )?;
    }
    let out_path = package::package_archives(
        rec,
        opts,
        &pkg,
        &outdir,
        size,
        sde,
        assembled.as_ref(),
        staged_assets.as_ref().map(|a| a.digest.as_str()),
        &toolchain_identity,
        &host_toolchain_identity,
        &slim_report,
    )?;

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

    Ok(BuildResult { archive: out_path })
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
mod buildroot;
mod package;
pub mod sandbox;
pub mod sections;
mod stage;

pub(crate) use cache::{build_id_for, buildid_sidecar, cache_id_for, try_cache_hit};
pub(crate) use sandbox::prepare_sandbox;
pub(crate) use sections::{diff_archives, run_sections};
use stage::StagedSource;

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
    use super::sections::installed_sonames;
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
