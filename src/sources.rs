//! New-schema source staging (fetch-phase.md §4.2, §6, §11).
//!
//! A recipe with `[[package.sources]]` builds its source workspace from
//! several verified artifacts instead of one legacy tarball:
//!
//! ```text
//! <srccache>/sha256/<digest>        content-addressed cache (§11)
//!         │ download_content_addressed — every source BEFORE any extraction
//!         ▼
//! <work>/stage/<name>               per-source extraction, strip applied
//!         │ merge_staged — rename in array order, mtimes survive
//!         ▼
//! <work>/src/<into>/…               the source workspace (= srcroot)
//! ```
//!
//! Ordering pinned by the design:
//! * all sources are downloaded and verified first; a bad hash fails the
//!   build before any extraction, before the closure and before any build
//!   section (§6),
//! * archives merge into the workspace in recipe order with `rename` (same
//!   filesystem), so file mtimes survive byte-identically (§4.2),
//! * `SOURCE_DATE_EPOCH` is the pinned value, else the newest mtime among
//!   the regular files extracted from archives — computed *after* the merge
//!   and *before* plain (`unpack = false`) files are copied in, so their
//!   staging time can never reach it (§6); such files are stamped with the
//!   final epoch,
//! * collisions are errors naming both sources; nothing is silently
//!   overwritten — within one source either (a duplicate archive entry is
//!   the same failure).

use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::fetch;
use crate::recipe::Recipe;

/// The staged source workspace of a new-schema recipe.
pub(crate) struct SourcesStaged {
    /// The source workspace root (`<work>/src`): `%{srcroot}` /
    /// `NAWA_SRCROOT`, the parent of the working directory (§4.3).
    pub srcroot: PathBuf,
    /// The working directory of every build section: the workspace root
    /// plus `[package] workdir` (empty = the root itself).
    pub srcdir: PathBuf,
    /// The chosen `SOURCE_DATE_EPOCH` (§6): the recipe-pinned value, else
    /// derived from the newest regular-file mtime in the workspace.
    pub source_date_epoch: u64,
}

/// What a workspace path has been claimed for during the merge.
struct Claim {
    is_dir: bool,
    owner: String,
}

type Claims = BTreeMap<PathBuf, Claim>;

/// Stage every `[[package.sources]]` of `rec` into `<work>/src` and return
/// the workspace layout. `cache` is the source cache root (`$NAWA_SRCCACHE`,
/// default `<builddir>/distfiles`) — the same value the legacy branch uses.
pub(crate) fn stage_sources(rec: &Recipe, work: &Path, src: &Path, cache: &Path, jobs: u32) -> Result<SourcesStaged> {
    // the per-source extraction directory: same filesystem as `src/`, so
    // the rename merge never copies (§4.2)
    let stage = work.join("stage");
    std::fs::create_dir_all(&stage)
        .with_context(|| format!("create source staging dir {}", stage.display()))?;
    let ctx = crate::recipe::ExpandCtx {
        jobs,
        pkgdir: work.join("pkg"),
        srcdir: src.to_path_buf(),
        arch: rec.arch(),
        // URLs may reference the workspace root; the sandbox view replaces
        // this ctx at section time
        srcroot: Some(src.to_path_buf()),
        assetsdir: None,
    };

    // ---- all sources, downloaded and verified, before any extraction (§6)

    struct Fetched<'a> {
        spec: &'a crate::recipe::SourceSpec,
        cache_path: PathBuf,
        url: String,
        archived: bool,
    }

    // Download all sources in parallel (network-bound); extraction and
    // merge stay in recipe order. `jobs` is the build parallelism knob;
    // one thread per source is fine because recipes rarely have > few
    // `[[package.sources]]`.
    let download_results: Vec<Result<Fetched>> = std::thread::scope(|scope| {
        let mut handles = Vec::new();
        for spec in &rec.package.sources {
            let ctx = &ctx;
            let h = scope.spawn(move || {
                let url = rec.expand(&spec.url, ctx);
                let mirrors: Vec<String> = spec
                    .mirrors
                    .iter()
                    .map(|m| rec.expand(m, ctx))
                    .collect();
                let dest = fetch::digest_cache_dir(cache).join(&spec.sha256);
                fetch::download_content_addressed(&url, &mirrors, &spec.sha256, &dest)
                    .with_context(|| {
                        format!("fetch source `{}` of recipe {}", spec.name, rec.package.name)
                    })?;
                crate::nawa_success!("source `{}` verified: {}", spec.name, dest.display());
                let archived = match spec.unpack {
                    Some(true) => true,
                    Some(false) => false,
                    None => fetch::detect_source_format(&dest).is_some(),
                };
                Ok::<_, anyhow::Error>(Fetched {
                    spec,
                    cache_path: dest,
                    url,
                    archived,
                })
            });
            handles.push(h);
        }
        handles
            .into_iter()
            .map(|h| h.join().expect("download thread panicked"))
            .collect::<Vec<Result<Fetched>>>()
    });
    let mut fetched: Vec<Fetched> = Vec::with_capacity(download_results.len());
    for r in download_results {
        fetched.push(r?);
    }

    // ---- extract archives into stage/<name>

    for f in &fetched {
        if !f.archived {
            if f.spec.unpack == Some(true) {
                bail!(
                    "source `{}` ({}) is not a tar archive — set `unpack = false` \
                     to stage it as a plain file (fetch-phase.md §4.2)",
                    f.spec.name,
                    f.url
                );
            }
            continue;
        }
        let dir = stage.join(&f.spec.name);
        fetch::extract_source_archive_stripped(&f.cache_path, &dir, f.spec.strip_components)
            .with_context(|| {
                format!(
                    "extract source `{}` into {}",
                    f.spec.name,
                    dir.display()
                )
            })?;
    }

    // ---- merge archives into the workspace, in recipe order (§4.2)

    let mut claims: Claims = BTreeMap::new();
    for f in &fetched {
        if !f.archived {
            continue;
        }
        let into = if f.spec.into.is_empty() {
            PathBuf::new()
        } else {
            PathBuf::from(&f.spec.into)
        };
        merge_staged(&stage.join(&f.spec.name), &into, &f.spec.name, src, &mut claims)
            .with_context(|| format!("merge source `{}`", f.spec.name))?;
    }

    // ---- SOURCE_DATE_EPOCH: pinned, else the newest regular-file mtime
    // from the archives (§6: computed before plain files are copied in)

    let sde = match rec.package.source_date_epoch {
        Some(p) => p,
        None => crate::buildenv::source_date_epoch_from_files(src).with_context(|| {
            format!("derive SOURCE_DATE_EPOCH for recipe {}", rec.package.name)
        })?,
    };

    // ---- plain sources: copied into place and stamped with the epoch

    for f in &fetched {
        if f.archived {
            continue;
        }
        let fname = f.url.rsplit('/').find(|s| !s.is_empty()).unwrap_or("source.bin");
        let into = if f.spec.into.is_empty() {
            PathBuf::from(fname)
        } else {
            PathBuf::from(&f.spec.into)
        };
        stage_plain_source(&f.cache_path, &into, &f.spec.name, src, sde, &mut claims)
            .with_context(|| format!("stage source `{}`", f.spec.name))?;
    }

    // ---- workdir: must exist after assembly (§4.3)

    let srcdir = if rec.package.workdir.is_empty() {
        src.to_path_buf()
    } else {
        let d = src.join(&rec.package.workdir);
        if !d.is_dir() {
            bail!(
                "recipe {}: [package] workdir `{}` does not exist in the staged \
                 source workspace ({}) — it must exist after assembly \
                 (fetch-phase.md §4.3)",
                rec.package.name,
                rec.package.workdir,
                d.display()
            );
        }
        d
    };

    // ---- the staging tree is transient (§6 layout keeps only src/assets/…)

    std::fs::remove_dir_all(&stage)
        .with_context(|| format!("remove source staging dir {}", stage.display()))?;

    Ok(SourcesStaged {
        srcroot: src.to_path_buf(),
        srcdir,
        source_date_epoch: sde,
    })
}

/// Move the contents of the per-source extraction dir `from` into the
/// workspace at `src/<into_rel>` with `rename`, registering every moved
/// path in `claims` under `owner`.
///
/// Directories merge — the same path may become a directory through any
/// number of sources. Any other claim conflict (a file staged twice, or a
/// file where another source has a directory — and vice versa) is an error
/// naming both sources and the path. Entries are visited parents-first, so
/// directory claims always exist before their children are moved.
///
/// `rename` within the same filesystem preserves file, symlink and
/// hard-link mtimes exactly (§4.2: the merge must not perturb the archive's
/// mtimes — `SOURCE_DATE_EPOCH` derives from them).
fn merge_staged(
    from: &Path,
    into_rel: &Path,
    owner: &str,
    src: &Path,
    claims: &mut Claims,
) -> Result<()> {
    for entry in walkdir::WalkDir::new(from)
        .min_depth(1)
        .sort_by_file_name()
    {
        let entry = entry.with_context(|| format!("walk {}", from.display()))?;
        let rel = entry
            .path()
            .strip_prefix(from)
            .context("stage path is under the stage root")?
            .to_path_buf();
        let dest_rel: PathBuf = into_rel.join(&rel);
        match_vs_claims(src, &dest_rel, !entry.file_type().is_dir(), owner, claims)?;
        let dest = src.join(&dest_rel);
        if entry.file_type().is_dir() {
            std::fs::create_dir_all(&dest)
                .with_context(|| format!("create workspace dir {}", dest.display()))?;
        } else {
            std::fs::rename(entry.path(), &dest).with_context(|| {
                format!(
                    "move staged {} into the workspace ({})",
                    entry.path().display(),
                    dest.display()
                )
            })?;
        }
    }
    Ok(())
}

/// Copy the plain (`unpack = false`) cache entry `file` into the workspace
/// at `src/<into_rel>` and stamp its mtime/atime with `sde` (§6: the file's
/// staging time must never reach `SOURCE_DATE_EPOCH` — which is why the
/// epoch is computed before this runs).
fn stage_plain_source(
    file: &Path,
    into_rel: &Path,
    owner: &str,
    src: &Path,
    sde: u64,
    claims: &mut Claims,
) -> Result<()> {
    match_vs_claims(src, into_rel, true, owner, claims)?;
    let dest = src.join(into_rel);
    std::fs::copy(file, &dest)
        .with_context(|| format!("copy plain source into {}", dest.display()))?;
    let t = filetime::FileTime::from_unix_time(sde as i64, 0);
    filetime::set_file_times(&dest, t, t)
        .with_context(|| format!("stamp plain source {}", dest.display()))?;
    Ok(())
}

/// Check `path` (workspace-relative) against the existing claims and
/// register it: a directory is mergeable with directories of any owner;
/// anything overlapping a file claim — or a second file claim — is an
/// error naming both sources.
///
/// `wants_file` describes the new claim; parent directories are registered
/// (and created) first, so a nested entry fails with the colliding *path*
/// named, not with an ENOENT from the copy/rename.
fn match_vs_claims(
    src: &Path,
    path: &Path,
    wants_file: bool,
    owner: &str,
    claims: &mut Claims,
) -> Result<()> {
    // every parent directory must be a directory: register the missing
    // chain (this source owns the dirs it creates)
    let mut ancestors: Vec<PathBuf> = path
        .ancestors()
        .skip(1)
        .map(Path::to_path_buf)
        .collect();
    ancestors.reverse();
    for anc in ancestors {
        if anc.as_os_str().is_empty() {
            continue;
        }
        match claims.get(&anc) {
            Some(c) if c.is_dir => {}
            Some(c) => bail!(
                "source workspace collision: `{}` is a file staged by source `{}` \
                 but source `{}` needs a directory there",
                anc.display(),
                c.owner,
                owner
            ),
            None => {
                claims.insert(anc.clone(), Claim { is_dir: true, owner: owner.to_string() });
                std::fs::create_dir_all(src.join(&anc))
                    .with_context(|| format!("create workspace dir {}", src.join(&anc).display()))?;
            }
        }
    }
    match claims.get(path) {
        Some(c) if c.is_dir && !wants_file => {} // directories merge
        Some(c) if c.is_dir => bail!(
            "source workspace collision: `{}` is a directory (staged by source `{}`) \
             but source `{}` stages a file there",
            path.display(),
            c.owner,
            owner
        ),
        Some(c) => bail!(
            "source workspace collision: `{}` is claimed twice — \
             sources `{}` and `{}` both stage it (fetch-phase.md §4.2)",
            path.display(),
            c.owner,
            owner
        ),
        None => {
            // belt beyond the claim map: a manifest can list the same path
            // twice — nothing is silently overwritten (§4.2)
            if src.join(path).symlink_metadata().is_ok() {
                bail!(
                    "source workspace collision: `{}` is claimed twice by \
                     source `{}` (duplicate archive entries?)",
                    path.display(),
                    owner
                );
            }
            claims.insert(
                path.to_path_buf(),
                Claim { is_dir: !wants_file, owner: owner.to_string() },
            );
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// unit tests: the merge/stamp/SDE primitives (the flow itself is covered by
// tests/sources.rs with a local HTTP server)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("nawa-sources-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// A fake "extraction result": dirs/files/symlinks under `from` with a
    /// pinned mtime on every file.
    fn make_tree(dir: &Path, items: &[(&str, &str, &[u8])], mtime: i64) {
        for (name, link, data) in items {
            let p = dir.join(name);
            let parent = p.parent().unwrap();
            std::fs::create_dir_all(parent).unwrap();
            // a trailing marker distinguishes a directory entry
            if name.ends_with('/') {
                continue;
            }
            match *link {
                "" => {
                    std::fs::write(&p, data).unwrap();
                    let t = filetime::FileTime::from_unix_time(mtime, 0);
                    filetime::set_file_times(&p, t, t).unwrap();
                }
                target => std::os::unix::fs::symlink(target, &p).unwrap(),
            }
        }
    }

    #[test]
    fn merge_moves_entries_and_preserves_mtimes() {
        let d = scratch("merge");
        let src = d.join("src");
        let stage = d.join("stage/main");
        std::fs::create_dir_all(&src).unwrap();
        make_tree(&stage, &[("hello", "", b"hello\n")], 1_000_000);

        let mut claims = Claims::new();
        merge_staged(&stage, &PathBuf::new(), "main", &src, &mut claims).unwrap();
        let md = std::fs::symlink_metadata(src.join("hello")).unwrap();
        use std::os::unix::fs::MetadataExt;
        assert_eq!(md.mtime(), 1_000_000, "rename keeps the archive mtime (§4.2)");
        assert!(claims.get(Path::new("hello")).unwrap().owner == "main");
        // the stage source entry was moved away
        assert!(!stage.join("hello").exists());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn merge_honors_into_and_keeps_subpaths() {
        let d = scratch("merge-into");
        let src = d.join("src");
        std::fs::create_dir_all(&src).unwrap();
        // the archive contents land under <into>/: stage/main/Makefile
        // becomes src/kernel/Makefile when into = "kernel"
        make_tree(&d.join("stage/main"), &[("Makefile", "", b"make\n")], 7);
        make_tree(&d.join("stage/patches"), &[("0001-fix", "", b"diff\n")], 7);

        let mut claims = Claims::new();
        merge_staged(&d.join("stage/main"), &PathBuf::from("kernel"), "main", &src, &mut claims).unwrap();
        merge_staged(
            &d.join("stage/patches"),
            &PathBuf::from("patches"),
            "patch-bundle",
            &src,
            &mut claims,
        )
        .unwrap();
        assert_eq!(std::fs::read(src.join("kernel/Makefile")).unwrap(), b"make\n");
        assert_eq!(std::fs::read(src.join("patches/0001-fix")).unwrap(), b"diff\n");
        assert_eq!(
            claims.get(Path::new("kernel")).map(|c| (c.owner.as_str(), c.is_dir)),
            Some(("main", true))
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn collision_between_sources_names_both() {
        let d = scratch("collide");
        let src = d.join("src");
        std::fs::create_dir_all(&src).unwrap();
        let a = d.join("stage/a");
        let b = d.join("stage/b");
        make_tree(&a, &[("C", "", b"from a\n")], 7);
        make_tree(&b, &[("C", "", b"from b\n")], 7);

        let mut claims = Claims::new();
        merge_staged(&a, &PathBuf::new(), "a", &src, &mut claims).unwrap();
        let err = merge_staged(&b, &PathBuf::new(), "b", &src, &mut claims).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("claimed twice") && msg.contains("`a`") && msg.contains("`b`"), "got: {msg}");
        // the first source's bytes win; nothing was overwritten
        assert_eq!(std::fs::read(src.join("C")).unwrap(), b"from a\n");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn file_vs_directory_collisions_are_errors() {
        let d = scratch("kindclash");
        let src = d.join("src");
        std::fs::create_dir_all(&src).unwrap();
        // source a stages the file `data`; source b stages the dir `data/`
        make_tree(&d.join("stage/a"), &[("data", "", b"file\n")], 7);
        make_tree(&d.join("stage/b"), &[("data/payload", "", b"busy\n")], 7);

        let mut claims = Claims::new();
        merge_staged(&d.join("stage/a"), &PathBuf::new(), "a", &src, &mut claims).unwrap();
        let err = merge_staged(&d.join("stage/b"), &PathBuf::new(), "b", &src, &mut claims).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("collision") && msg.contains("`a`") && msg.contains("`b`"), "got: {msg}");

        // and the other direction: dir first, file second — a fresh
        // workspace, with both stage trees rebuilt (the first merges
        // moved their entries away)
        let d2 = scratch("kindclash2");
        let src2 = d2.join("src");
        std::fs::create_dir_all(&src2).unwrap();
        make_tree(&d2.join("stage/b"), &[("data/payload", "", b"busy\n")], 7);
        make_tree(&d2.join("stage/a"), &[("data", "", b"file\n")], 7);
        let mut claims = Claims::new();
        merge_staged(&d2.join("stage/b"), &PathBuf::new(), "b", &src2, &mut claims).unwrap();
        let err = merge_staged(&d2.join("stage/a"), &PathBuf::new(), "a", &src2, &mut claims).unwrap_err();
        assert!(err.to_string().contains("collision"), "got: {err}");
        let _ = std::fs::remove_dir_all(&d2);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn directory_merge_across_sources_is_allowed() {
        let d = scratch("dirmrg");
        let src = d.join("src");
        std::fs::create_dir_all(&src).unwrap();
        let a = d.join("stage/a");
        let b = d.join("stage/b");
        make_tree(&a, &[("common/x", "", b"ax\n")], 7);
        make_tree(&b, &[("common/y", "", b"by\n")], 7);

        let mut claims = Claims::new();
        merge_staged(&a, &PathBuf::new(), "a", &src, &mut claims).unwrap();
        merge_staged(&b, &PathBuf::new(), "b", &src, &mut claims).unwrap();
        assert_eq!(std::fs::read(src.join("common/x")).unwrap(), b"ax\n");
        assert_eq!(std::fs::read(src.join("common/y")).unwrap(), b"by\n");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn duplicate_entry_within_one_source_is_rejected() {
        let d = scratch("dup");
        let src = d.join("src");
        std::fs::create_dir_all(&src).unwrap();
        // simulate the state after a rename already landed a path: fresh
        // claim map, but the file exists — the FS belt must fire, because
        // nothing is silently overwritten (§4.2)
        std::fs::write(src.join("twice"), b"one\n").unwrap();
        let err = match_vs_claims(&src, Path::new("twice"), true, "a", &mut Claims::new())
            .unwrap_err()
            .to_string();
        assert!(err.contains("duplicate archive entries"), "got: {err}");
        assert_eq!(std::fs::read(src.join("twice")).unwrap(), b"one\n");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn plain_source_is_copied_and_stamped_with_the_epoch() {
        let d = scratch("plain");
        let src = d.join("src");
        std::fs::create_dir_all(&src).unwrap();
        let blob = d.join("blob");
        std::fs::write(&blob, b"bootstrap data\n").unwrap();

        let mut claims = Claims::new();
        stage_plain_source(
            &blob,
            &PathBuf::from("data/bootstrap.dat"),
            "data",
            &src,
            1_700_000_123,
            &mut claims,
        )
        .unwrap();
        let dest = src.join("data/bootstrap.dat");
        // metadata FIRST: reading the file afterwards bumps the atime
        // (relatime updates it when atime <= mtime)
        let md = std::fs::symlink_metadata(&dest).unwrap();
        use std::os::unix::fs::MetadataExt;
        assert_eq!(md.mtime(), 1_700_000_123, "stamped mtime (§6)");
        assert_eq!(md.atime(), 1_700_000_123, "stamped atime (§6)");
        assert_eq!(std::fs::read(&dest).unwrap(), b"bootstrap data\n");
        assert!(src.join("data").is_dir(), "parent dirs created");
        assert!(claims.get(Path::new("data")).unwrap().is_dir);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn plain_source_refuses_an_existing_destination() {
        let d = scratch("plainclash");
        let src = d.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("shared.dat"), b"already here\n").unwrap();
        let blob = d.join("blob");
        std::fs::write(&blob, b"content\n").unwrap();

        let mut claims = Claims::new();
        let err = stage_plain_source(
            &blob,
            &PathBuf::from("shared.dat"),
            "data",
            &src,
            42,
            &mut claims,
        )
        .unwrap_err();
        assert!(err.to_string().contains("collision"), "got: {err}");
        assert_eq!(std::fs::read(src.join("shared.dat")).unwrap(), b"already here\n");
        let _ = std::fs::remove_dir_all(&d);
    }
}
