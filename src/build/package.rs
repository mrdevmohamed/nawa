//! Build phase 3: collect the staged files, lint them and write the archives.

use anyhow::{bail, Result};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::sections::installed_sonames;
use super::{buildid_sidecar, build_id_for, BuildOpts};
use crate::archive;
use crate::buildenv::Isolation;
use crate::closure::AssembledRoot;
use crate::recipe::Recipe;
use crate::slim;

/// Every staged path (sorted, rooted at `/`) and the total size of the
/// regular files. An empty tree is a failed install section.
pub(super) fn collect_staged_files(pkg: &Path) -> Result<(Vec<String>, u64)> {
    let mut files: Vec<String> = Vec::new();
    let mut size: u64 = 0;
    for e in walkdir::WalkDir::new(pkg).sort_by_file_name() {
        let e = e?;
        let rel = e.path().strip_prefix(pkg)?;
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
    Ok((files, size))
}

/// Post-build checks ("lint", §7); an error aborts before any archive exists.
#[allow(clippy::too_many_arguments)]
pub(super) fn lint_stage(
    rec: &Recipe,
    opts: &BuildOpts,
    pkg: &Path,
    work: &Path,
    assembled: Option<&AssembledRoot>,
    sandboxed: bool,
    sandbox_base: &str,
    has_assets: bool,
) -> Result<()> {
    if opts.isolation >= Isolation::Env {
        let sonames: BTreeMap<String, String> = match assembled {
            Some(root) => root.sonames.clone(),
            None => installed_sonames(&opts.root)?,
        };
        // inside the sandbox the build only ever sees /build/... (or the
        // fallback base); outside it the host build dir must not leak either
        let host_builddir: Option<String> = if sandboxed {
            if sandbox_base == "/build" {
                None // the hardcoded /build/src, /build/pkg checks cover it
            } else {
                Some(sandbox_base.to_string())
            }
        } else {
            // Match this build's own work dir (e.g. /build/perl-5.44.0-1),
            // not the bare builddir: with builddir=/build the bare path is a
            // substring of innocent doc text ("/build", "/builddir"...) and
            // flagged 24 false positives in perl's man pages and modules.
            Some(work.to_string_lossy().into_owned())
        };
        let ctx = crate::lint::LintCtx {
            pkgdir: pkg,
            rundeps: &rec.depends.run,
            sonames: &sonames,
            host_builddir: host_builddir.as_deref(),
            // §5.3/§6: the staged snapshot and the per-build fetch views
            // are <build_inside>/assets and <build_inside>/fetch/… inside
            // the sandbox; recipes that enable them must not leak those
            // paths into the archive
            extra_leak_paths: {
                let mut leaks: Vec<&'static str> = Vec::new();
                if has_assets {
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
    Ok(())
}

/// Write `.PKGINFO` and the main archive, then one archive per split-off
/// sub-package (`-doc`, `-lang`). Returns the main archive path and its
/// `.PKGINFO`.
#[allow(clippy::too_many_arguments)]
pub(super) fn package_archives(
    rec: &Recipe,
    opts: &BuildOpts,
    pkg: &Path,
    outdir: &Path,
    size: u64,
    sde: u64,
    assembled: Option<&AssembledRoot>,
    assets_digest: Option<&str>,
    toolchain_identity: &[(&'static str, String)],
    slim_report: &slim::Report,
) -> Result<(PathBuf, String)> {
    let name = &rec.package.name;
    let arch = rec.arch();
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
            .map(|a| a.archives.clone())
            .unwrap_or_default();
        let bid = build_id_for(rec, opts, &closure_shas, assets_digest, toolchain_identity);
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
            pkg,
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
        archive::create_with_hooks(pkg, &pkginfo, Some(&hooks), &out_path)?;
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
    Ok((out_path, pkginfo))
}
