//! `apply-transaction`: execute an approved install/update plan (design
//! §4.3/§4.4). `plan_install` decided *what* will happen; this use case owns
//! the per-package sequence of fetch/build and install steps and drives them
//! through the transaction journal. All user-visible text goes through the
//! [`UserInteraction`] port — this module owns the wording, the presentation
//! adapter owns the rendering (colours, tables, prompt mechanics).
//!
//! The summary is not printed here: success is returned as [`Applied::Done`]
//! with the changed package names and the raw elapsed seconds, and the caller
//! formats the duration and owns the post-install reverse-dependency warning.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

use crate::application::features::package::plan_install::{Source, Sourcing};
use crate::application::ports::user_interaction::{PlanRow, PlanView, UserInteraction};
use crate::config::InstallMode;
use crate::ctx::Ctx;
use crate::db;
use crate::install;
use crate::lock;
use crate::plan;
use crate::repo;
use crate::transaction;
use crate::upgrade;

/// How [`apply_transaction`] ended.
#[derive(Debug)]
pub(crate) enum Applied {
    /// the plan was empty or this was a dry run — nothing was built or
    /// installed (the caller still finishes the recorded operation)
    NothingToDo,
    /// every package was installed: `changed` holds the installed package
    /// names in install order, `seconds` the elapsed wall time
    Done { changed: Vec<String>, seconds: u64 },
}

/// Build one plan item: the injected build effect. The real implementation
/// (`commands::build_one`) prints its own progress from the presentation
/// layer, so it cannot be referenced from here.
pub(crate) type BuildStep<'a> = &'a dyn Fn(&Ctx, &plan::Item, usize, usize) -> Result<PathBuf>;

/// Per-run knobs the caller owns. No CLI flag struct crosses the boundary —
/// plain values only, and the build effect is injected.
pub(crate) struct ApplyRequest<'a> {
    /// build one plan item; called only for `Source::Build` items
    pub build: BuildStep<'a>,
    pub force: bool,
    pub allow_unsigned: bool,
    /// show the plan and stop: nothing is built or installed
    pub dry_run: bool,
    /// the install reason recorded for repository-only `extras`
    pub extras_why: plan::Why,
}

/// Details for one staged archive installation.
struct InstallRequest<'a> {
    reason: crate::db::InstallReason,
    what: &'a str,
    number: usize,
    total: usize,
    force: bool,
    allow_unsigned: bool,
    /// SHA-256 the repository index promised for this archive (repository
    /// sources only). Always pinned, so the installed bytes are the bytes
    /// that were validated against the index entry.
    index_sha256: Option<&'a str>,
    /// The index was authenticated by a trusted key and replaces the
    /// per-archive detached signature (schema v2).
    signed_index: bool,
    /// Where this archive comes from, for the history record.
    source: &'a str,
}

/// Show the plan, ask, then install every item in order — dependencies
/// first, one `[n/total]` line per step. Each item comes from the repository
/// or is built from its recipe (see [`Sourcing`]); `extras` are repository
/// packages without a recipe, installed last.
///
/// The caller owns the transaction (`op`), the confirmation policy (through
/// `ui`) and the summary rendering; this function reports what happened
/// instead of printing it.
pub(crate) fn apply_transaction(
    ctx: &Ctx,
    plan: &plan::Plan,
    extras: &[repo::RepoEntry],
    sourcing: &Sourcing,
    op: &mut transaction::OperationHandle,
    request: &ApplyRequest<'_>,
    ui: &UserInteraction,
) -> Result<Applied> {
    let total = plan.items.len() + extras.len();
    if total == 0 {
        (ui.info)("nothing to do");
        return Ok(Applied::NothingToDo);
    }

    // decide every source up front: a strict `repo` run must fail BEFORE the
    // first package is touched, not halfway through
    let mut sources: Vec<Source> = Vec::with_capacity(plan.items.len());
    for it in &plan.items {
        let (source, note) = sourcing.pick(&it.rec)?;
        if let Some(note) = note {
            (ui.stderr_line)(&note);
        }
        sources.push(source);
    }

    let from_repo = count_repo(&sources, extras);
    (ui.info)(&format!(
        "{total} package(s) to install into {} ({from_repo} from the repository, {} to build)",
        ctx.root.display(),
        total - from_repo
    ));
    (ui.show_plan)(&plan_view(plan, extras, &sources));
    if request.dry_run {
        (ui.info)("dry run — nothing was built or installed");
        return Ok(Applied::NothingToDo);
    }

    // `install_mode = ask`: on a terminal, let the user choose the source
    // for everything the repository could supply
    if sourcing.ask && count_repo(&sources, extras) > 0 {
        let c = (ui.choose)(
            "nawa: install the repository-available packages from the repository (r) or build them from source (b)?",
            'r',
            &['r', 'b'],
        )?;
        if c == 'b' {
            if let Some(re) = extras.first() {
                bail!(
                    "`{}` has no recipe, so it cannot be built — it exists only in the repository",
                    re.name
                );
            }
            for s in sources.iter_mut() {
                *s = Source::Build;
            }
            (ui.info)(&format!("building all {total} package(s) from source"));
        }
    }
    (ui.confirm)(&format!("nawa: install {total} package(s)?"))?;

    let started = std::time::Instant::now();
    for (i, it) in plan.items.iter().enumerate() {
        // Ctrl+C during the previous install was deferred until that package
        // was complete; now that the system is consistent, stop here
        crate::interrupt::check()?;
        let n = i + 1;
        let id = it.rec.id();
        let mut step = || -> Result<()> {
            let (archive, index_sha256, signed_index) =
                acquire(ctx, it, request, sourcing, &sources[i], ui, n, total)?;
            let source = match &sources[i] {
                Source::Repo(_) => "repository",
                Source::Build => "local-build",
            };
            install_one(
                ctx,
                &archive,
                InstallRequest {
                    reason: it.why.db_reason(),
                    what: &id,
                    number: n,
                    total,
                    force: request.force,
                    allow_unsigned: request.allow_unsigned,
                    index_sha256: index_sha256.as_deref(),
                    signed_index,
                    source,
                },
                sourcing.open.as_ref(),
                op,
                ui,
            )
        };
        if let Err(e) = step() {
            if i > 0 {
                (ui.stderr_line)(&format!(
                    "nawa: {i} of {total} package(s) were installed before this failure — \
                     re-run the same command to continue (installed packages are skipped)"
                ));
            }
            return Err(e);
        }
    }
    for (j, re) in extras.iter().enumerate() {
        crate::interrupt::check()?;
        let n = plan.items.len() + j + 1;
        let what = format!("{}-{}-{}", re.name, re.version, re.rel);
        let mut step = || -> Result<()> {
            (ui.info)(&format!(
                "[{n}/{total}] fetching {what} from the repository"
            ));
            let open = sourcing
                .open
                .as_ref()
                .context("internal: repository package without an open repository")?;
            let archive = upgrade::fetch_entry(&ctx.root, open, re, request.allow_unsigned)
                .with_context(|| format!("[{n}/{total}] repository fetch of {what} failed"))?;
            let signed_index = open.archive_auth == upgrade::ArchiveAuth::SignedIndexSha256;
            install_one(
                ctx,
                &archive,
                InstallRequest {
                    reason: request.extras_why.db_reason(),
                    what: &what,
                    number: n,
                    total,
                    force: request.force,
                    allow_unsigned: request.allow_unsigned,
                    index_sha256: Some(re.sha256.as_str()),
                    signed_index,
                    source: "repository",
                },
                sourcing.open.as_ref(),
                op,
                ui,
            )
        };
        if let Err(e) = step() {
            if n > 1 {
                (ui.stderr_line)(&format!(
                    "nawa: {} of {total} package(s) were installed before this failure — \
                     re-run the same command to continue (installed packages are skipped)",
                    n - 1
                ));
            }
            return Err(e);
        }
    }
    let mut changed: Vec<String> = plan
        .items
        .iter()
        .map(|it| it.rec.package.name.clone())
        .collect();
    changed.extend(extras.iter().map(|re| re.name.clone()));
    Ok(Applied::Done {
        changed,
        seconds: started.elapsed().as_secs(),
    })
}

/// Get the archive for one plan item: fetch it from the repository or build
/// it. Returns the archive path, the repository index sha256 (repository
/// copies only) and whether a signed index authenticated it. In `auto` mode a
/// failed repository fetch falls back to a build (the recipe pins its
/// sources); in strict `repo` mode it is an error.
#[allow(clippy::too_many_arguments)] // preserves the original acquire_archive call shape; item + request + sourcing + source + progress belong together
fn acquire(
    ctx: &Ctx,
    it: &plan::Item,
    request: &ApplyRequest<'_>,
    sourcing: &Sourcing,
    source: &Source,
    ui: &UserInteraction,
    n: usize,
    total: usize,
) -> Result<(PathBuf, Option<String>, bool)> {
    let id = it.rec.id();
    let acquired = match source {
        Source::Build => ((request.build)(ctx, it, n, total)?, None, false),
        Source::Repo(re) => {
            (ui.info)(&format!("[{n}/{total}] fetching {id} from the repository"));
            let open = sourcing
                .open
                .as_ref()
                .context("internal: repository source without an open repository")?;
            match upgrade::fetch_entry(&ctx.root, open, re, request.allow_unsigned) {
                Ok(a) => (
                    a,
                    Some(re.sha256.clone()),
                    open.archive_auth == upgrade::ArchiveAuth::SignedIndexSha256,
                ),
                // strict repo mode never builds behind the user's back
                Err(e) if sourcing.mode == InstallMode::Repo => {
                    return Err(e)
                        .with_context(|| format!("[{n}/{total}] repository fetch of {id} failed"));
                }
                // auto: the recipe pins its sources, so a build is a safe fallback
                Err(e) => {
                    (ui.warn)(&format!(
                        "cannot use the repository copy of {id} ({e:#}) — building from source"
                    ));
                    ((request.build)(ctx, it, n, total)?, None, false)
                }
            }
        }
    };
    Ok(acquired)
}

/// Install one ready archive. The library functions never lock; hold the db
/// lock for this one install only, never through a build or a download.
fn install_one(
    ctx: &Ctx,
    archive: &Path,
    details: InstallRequest<'_>,
    open_repo: Option<&upgrade::OpenRepo>,
    op: &mut transaction::OperationHandle,
    ui: &UserInteraction,
) -> Result<()> {
    (ui.info)(&format!(
        "[{}/{}] installing {}",
        details.number, details.total, details.what
    ));
    let _lock = lock::acquire(&ctx.root, lock::Mode::Exclusive)?;
    // Resolve the real package identity from .PKGINFO; archive filenames
    // are not authoritative package metadata.
    let package = archive_name_of(archive)
        .ok_or_else(|| anyhow::anyhow!("cannot read package name from {}", archive.display()))?;
    let db = db::Db::new(&ctx.root);
    let before = db.entry(&package)?;
    // A package that was explicitly requested must stay explicit even when
    // a group later upgrades it. "keep" preserves the existing DB reason.
    let effective_reason = if details.reason == crate::db::InstallReason::Group
        && before
            .as_ref()
            .is_some_and(|entry| entry.reason == crate::db::InstallReason::Explicit.as_str())
    {
        crate::db::InstallReason::Keep
    } else {
        details.reason
    };
    let target = install::inspect_archive_state(archive, effective_reason, before.as_ref())?;
    if let Some(expected) = details.index_sha256 {
        if expected != target.artifact_sha256 {
            bail!(
                "archive {} changed after repository verification: index expected {}, inspected {}",
                archive.display(),
                expected,
                target.artifact_sha256
            );
        }
    }
    let action = match &before {
        None => transaction::history::StepAction::Install,
        Some(old)
            if old.version != target.version.clone().unwrap_or_default()
                || Some(old.rel) != target.rel =>
        {
            transaction::history::StepAction::Upgrade
        }
        Some(_) => transaction::history::StepAction::Reinstall,
    };
    let mut recorder = op.recorder();
    let step_no = recorder.step_intent_state(
        &package,
        action,
        before.as_ref(),
        Some(&target),
        &target.artifact_sha256,
    )?;
    let file_journal = install::FileJournalId::for_step(recorder.tx(), step_no);
    let installed = match details.index_sha256 {
        Some(expected) if details.signed_index => {
            let proof = open_repo.map(|open| install::IndexProof {
                index_bytes: Some(&open.index_bytes),
                index_signature: open.index_signature.as_deref(),
                signer_public: open.signer_public.as_deref(),
                repo_id: open.base_url.as_deref(),
            });
            install::install_archive_from_index_proof(
                archive,
                &ctx.root,
                details.force,
                expected,
                effective_reason,
                proof,
                file_journal,
            )
        }
        pinned => {
            let expected = pinned.unwrap_or(target.artifact_sha256.as_str());
            install::install_archive_pinned(
                archive,
                &ctx.root,
                install::InstallOpts {
                    force: details.force,
                    allow_unsigned: details.allow_unsigned,
                    key: None,
                    reason: effective_reason,
                    pinned_sha: Some(expected),
                    file_journal,
                },
            )
        }
    };
    install::record_install_outcome(
        &mut recorder,
        &ctx.root,
        step_no,
        &package,
        action,
        before.as_ref(),
        &target,
        details.source,
        &installed,
    )?;
    installed.with_context(|| {
        format!(
            "[{}/{}] install of {} failed",
            details.number, details.total, details.what
        )
    })?;
    Ok(())
}

/// The package name inside an archive (from .PKGINFO), when readable.
pub(crate) fn archive_name_of(archive: &Path) -> Option<String> {
    let raw = crate::archive::read_pkginfo(archive).ok()?;
    let info = crate::archive::parse_pkginfo(&raw).ok()?;
    info.get("pkgname").cloned()
}

/// How many items of the plan come from the repository (`extras` always do).
fn count_repo(sources: &[Source], extras: &[repo::RepoEntry]) -> usize {
    sources
        .iter()
        .filter(|s| matches!(s, Source::Repo(_)))
        .count()
        + extras.len()
}

/// The plan table as display data: one [`PlanRow`] per package in install
/// order. The use case owns the why-notes; the presentation adapter owns
/// column widths, colours and the totals line.
fn plan_view(plan: &plan::Plan, extras: &[repo::RepoEntry], sources: &[Source]) -> PlanView {
    let mut rows: Vec<PlanRow> = Vec::with_capacity(plan.items.len() + extras.len());
    for (i, it) in plan.items.iter().enumerate() {
        let (tag, dl, inst) = match &sources[i] {
            Source::Repo(re) => ("repo", re.size, re.installed_size),
            Source::Build => ("build", 0, 0),
        };
        let mut note = it.why.label().to_string();
        if let Some(old) = &it.replaces {
            note.push_str(&format!(" (replaces {old})"));
        }
        rows.push(PlanRow {
            name: it.rec.package.name.clone(),
            ver: format!("{}-{}", it.rec.package.version, it.rec.package.rel),
            tag,
            dl,
            inst,
            note,
        });
    }
    for re in extras {
        rows.push(PlanRow {
            name: re.name.clone(),
            ver: format!("{}-{}", re.version, re.rel),
            tag: "repo",
            dl: re.size,
            inst: re.installed_size,
            note: "explicit (no recipe: dependencies not resolved)".to_string(),
        });
    }
    PlanView { rows }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recipe::Recipe;
    use crate::repo::{Index, RepoEntry};
    use crate::transaction::context::no_history;
    use crate::upgrade::{ArchiveAuth, OpenRepo};
    use std::cell::RefCell;
    use std::collections::BTreeMap;
    use std::rc::Rc;

    fn rec(name: &str, ver: &str) -> Recipe {
        toml::from_str(&format!(
            "[package]\nname = \"{name}\"\nversion = \"{ver}\"\n\
             [depends]\nbuild = []\nrun = []\n[build]\ninstall = \"true\"\n"
        ))
        .unwrap()
    }

    fn item(name: &str, ver: &str) -> plan::Item {
        plan::Item {
            rec: rec(name, ver),
            why: plan::Why::Explicit,
            replaces: None,
        }
    }

    fn repo_entry(name: &str, ver: &str, rel: u32) -> RepoEntry {
        RepoEntry {
            name: name.into(),
            version: ver.into(),
            rel,
            arch: String::new(),
            file: format!("{name}.nawa.tar.zst"),
            sha256: "00".into(),
            download_url: None,
            size: 0,
            installed_size: 0,
        }
    }

    fn open_repo(entries: Vec<RepoEntry>) -> OpenRepo {
        OpenRepo {
            dir: PathBuf::new(),
            base_url: None,
            index: Index {
                generated: None,
                expires: None,
                repository: None,
                recipes: None,
                groups: None,
                packages: entries,
            },
            archive_auth: ArchiveAuth::DetachedSignature,
            index_bytes: Vec::new(),
            index_signature: None,
            signer_public: None,
            groups: None,
        }
    }

    fn ctx_with(root: &Path) -> Ctx {
        Ctx::from_config(&crate::config::Config {
            root: root.to_path_buf(),
            recipes: root.join("recipes"),
            builddir: root.join("build"),
            profiles: root.join("profiles"),
            jobs: None,
            repo: None,
            install_mode: InstallMode::Auto,
            downloads: 4,
            file: None,
            system_file: None,
            sources: BTreeMap::new(),
        })
    }

    /// A record-everything adapter: every port call appends one prefixed
    /// line, builds additionally to `builds`. The `yes`/`choice` parameters
    /// stand in for the user's answers.
    #[derive(Default)]
    struct Recorder {
        lines: Rc<RefCell<Vec<String>>>,
        builds: Rc<RefCell<Vec<(String, usize, usize)>>>,
    }

    impl Recorder {
        fn of(&self, prefix: &str) -> Vec<String> {
            self.lines
                .borrow()
                .iter()
                .filter(|l| l.starts_with(prefix))
                .cloned()
                .collect()
        }
        fn infos(&self) -> Vec<String> {
            self.of("INFO ")
        }
        fn stderr(&self) -> Vec<String> {
            self.of("STDERR ")
        }
        fn find(&self, needle: &str) -> Option<usize> {
            self.lines.borrow().iter().position(|l| l.contains(needle))
        }
        fn ui(&self, yes: bool, choice: char) -> UserInteraction {
            let lines = Rc::clone(&self.lines);
            let stderr_line = Box::new(move |l: &str| {
                lines.borrow_mut().push(format!("STDERR {l}"));
            });
            let lines = Rc::clone(&self.lines);
            let info = Box::new(move |l: &str| {
                lines.borrow_mut().push(format!("INFO {l}"));
            });
            let lines = Rc::clone(&self.lines);
            let warn = Box::new(move |l: &str| {
                lines.borrow_mut().push(format!("WARN {l}"));
            });
            let lines = Rc::clone(&self.lines);
            let show_plan = Box::new(move |view: &PlanView| {
                let rows = view
                    .rows
                    .iter()
                    .map(|r| {
                        format!(
                            "{}|{}|{}|{}|{}|{}",
                            r.name, r.ver, r.tag, r.dl, r.inst, r.note
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("; ");
                lines.borrow_mut().push(format!("SHOW {rows}"));
            });
            let lines = Rc::clone(&self.lines);
            let choice = Rc::new(choice);
            let choose = Box::new(move |prompt: &str, _default: char, _allowed: &[char]| {
                lines.borrow_mut().push(format!("CHOOSE {prompt}"));
                Ok(*choice)
            });
            let lines = Rc::clone(&self.lines);
            let confirm = Box::new(move |prompt: &str| {
                lines.borrow_mut().push(format!("CONFIRM {prompt}"));
                if yes {
                    Ok(())
                } else {
                    bail!("declined")
                }
            });
            UserInteraction {
                stderr_line,
                info,
                warn,
                show_plan,
                choose,
                confirm,
            }
        }
        /// A build step that records `(package, n, total)` and either
        /// succeeds with a placeholder archive path or fails on the first
        /// call with `fail_first`. Borrowed by the test through
        /// [`plain_request`].
        fn build_fn(
            &self,
            fail_first: bool,
        ) -> impl Fn(&Ctx, &plan::Item, usize, usize) -> Result<PathBuf> + '_ {
            let builds = Rc::clone(&self.builds);
            move |_ctx: &Ctx, it: &plan::Item, n: usize, total: usize| {
                builds
                    .borrow_mut()
                    .push((it.rec.package.name.clone(), n, total));
                if fail_first {
                    bail!("boom building {}", it.rec.package.name)
                }
                Ok(PathBuf::from("/nonexistent/nawa-test.nawa.tar.zst"))
            }
        }
    }

    /// The constant per-run knobs of the unit-test [`ApplyRequest`]; `build`
    /// comes from the test's [`Recorder::build_fn`].
    fn plain_request<'a>(build: BuildStep<'a>, dry_run: bool) -> ApplyRequest<'a> {
        ApplyRequest {
            build,
            force: false,
            allow_unsigned: false,
            dry_run,
            extras_why: plan::Why::Explicit,
        }
    }

    fn apply(
        ctx: &Ctx,
        plan: &plan::Plan,
        extras: &[RepoEntry],
        sourcing: &Sourcing,
        request: &ApplyRequest<'_>,
        ui: &UserInteraction,
    ) -> Result<Applied> {
        let mut op = no_history();
        apply_transaction(ctx, plan, extras, sourcing, &mut op, request, ui)
    }

    #[test]
    fn empty_plan_is_nothing_to_do() {
        let recorder = Recorder::default();
        let ctx = ctx_with(&crate::testutil::tmpdir("apply-empty"));
        let sourcing = Sourcing::build_only();
        let ui = recorder.ui(true, 'r');
        let build = recorder.build_fn(false);
        let request = plain_request(&build, false);
        let applied = apply(&ctx, &plan::Plan::default(), &[], &sourcing, &request, &ui).unwrap();
        assert!(matches!(applied, Applied::NothingToDo));
        assert_eq!(recorder.infos(), ["INFO nothing to do".to_string()]);
        assert!(recorder.find("SHOW").is_none());
        assert!(recorder.find("CONFIRM").is_none());
        assert!(recorder.builds.borrow().is_empty());
    }

    #[test]
    fn dry_run_shows_the_plan_and_installs_nothing() {
        let recorder = Recorder::default();
        let ctx = ctx_with(&crate::testutil::tmpdir("apply-dryrun"));
        let sourcing = Sourcing::build_only();
        let p = plan::Plan {
            items: vec![item("curl", "1.0")],
            ..Default::default()
        };
        let build = recorder.build_fn(false);
        let request = plain_request(&build, true);
        let ui = recorder.ui(true, 'r');
        let applied = apply(&ctx, &p, &[], &sourcing, &request, &ui).unwrap();
        assert!(matches!(applied, Applied::NothingToDo));
        assert_eq!(
            recorder.infos()[0],
            format!(
                "INFO 1 package(s) to install into {} (0 from the repository, 1 to build)",
                ctx.root.display()
            )
        );
        assert_eq!(
            recorder.infos()[1],
            "INFO dry run — nothing was built or installed".to_string()
        );
        assert_eq!(
            recorder.of("SHOW "),
            ["SHOW curl|1.0-1|build|0|0|explicit".to_string()]
        );
        assert!(recorder.find("CONFIRM").is_none());
        assert!(recorder.builds.borrow().is_empty());
    }

    #[test]
    fn declined_confirm_installs_nothing() {
        let recorder = Recorder::default();
        let ctx = ctx_with(&crate::testutil::tmpdir("apply-declined"));
        let sourcing = Sourcing::build_only();
        let p = plan::Plan {
            items: vec![item("curl", "1.0")],
            ..Default::default()
        };
        let ui = recorder.ui(false, 'r');
        let build = recorder.build_fn(false);
        let request = plain_request(&build, false);
        let error = apply(&ctx, &p, &[], &sourcing, &request, &ui).unwrap_err();
        assert_eq!(error.to_string(), "declined");
        assert!(recorder.find("SHOW").is_some());
        assert_eq!(
            recorder.of("CONFIRM "),
            ["CONFIRM nawa: install 1 package(s)?".to_string()]
        );
        assert!(recorder.find("installing").is_none());
        assert!(recorder.builds.borrow().is_empty());
    }

    #[test]
    fn ask_mode_cannot_build_repository_only_extras() {
        let recorder = Recorder::default();
        let ctx = ctx_with(&crate::testutil::tmpdir("apply-askextra"));
        // the plan item has no repository entry (built), the extra is
        // repository-only: choosing "build" cannot apply to it
        let sourcing = Sourcing {
            mode: InstallMode::Auto,
            ask: true,
            open: Some(open_repo(vec![repo_entry("tool", "1.0", 1)])),
        };
        let p = plan::Plan {
            items: vec![item("curl", "1.0")],
            ..Default::default()
        };
        let extras = vec![repo_entry("tool", "1.0", 1)];
        let ui = recorder.ui(true, 'b');
        let build = recorder.build_fn(false);
        let request = plain_request(&build, false);
        let error = apply(&ctx, &p, &extras, &sourcing, &request, &ui).unwrap_err();
        assert!(error
            .to_string()
            .contains("`tool` has no recipe, so it cannot be built"));
        assert!(recorder.find("SHOW").is_some());
        assert!(recorder.find("CHOOSE").is_some());
        assert!(recorder.find("CONFIRM").is_none());
        assert!(recorder.builds.borrow().is_empty());
    }

    #[test]
    fn ask_mode_build_choice_builds_everything() {
        let recorder = Recorder::default();
        let ctx = ctx_with(&crate::testutil::tmpdir("apply-askbuild"));
        // the repository has the recipe's version, so the item starts as a
        // repository source; choosing "b" must switch every source to build
        let sourcing = Sourcing {
            mode: InstallMode::Auto,
            ask: true,
            open: Some(open_repo(vec![repo_entry("curl", "1.0", 1)])),
        };
        let p = plan::Plan {
            items: vec![item("curl", "1.0")],
            ..Default::default()
        };
        let ui = recorder.ui(true, 'b');
        let build = recorder.build_fn(false);
        let request = plain_request(&build, false);
        let result = apply(&ctx, &p, &[], &sourcing, &request, &ui);
        assert!(result.is_err()); // the fake archive cannot be installed
        assert_eq!(*recorder.builds.borrow(), vec![("curl".to_string(), 1, 1)]);
        assert_eq!(
            recorder.infos()[1],
            "INFO building all 1 package(s) from source".to_string()
        );
        assert!(!recorder.infos().iter().any(|l| l.contains("fetching")));
    }

    #[test]
    fn strict_pick_failure_fails_before_the_plan_is_shown() {
        let recorder = Recorder::default();
        let ctx = ctx_with(&crate::testutil::tmpdir("apply-strictpick"));
        let sourcing = Sourcing {
            mode: InstallMode::Repo,
            ask: false,
            open: Some(open_repo(vec![])),
        };
        let p = plan::Plan {
            items: vec![item("curl", "1.0")],
            ..Default::default()
        };
        let ui = recorder.ui(true, 'r');
        let build = recorder.build_fn(false);
        let request = plain_request(&build, false);
        let error = apply(&ctx, &p, &[], &sourcing, &request, &ui).unwrap_err();
        assert!(error
            .to_string()
            .contains("not in the repository for this architecture"));
        assert!(recorder.find("SHOW").is_none());
        assert!(recorder.find("CONFIRM").is_none());
        assert!(recorder.lines.borrow().is_empty());
    }

    #[test]
    fn pick_note_prints_before_the_plan() {
        let recorder = Recorder::default();
        let ctx = ctx_with(&crate::testutil::tmpdir("apply-picknote"));
        // strict repo mode installs the repository copy even when the recipe
        // tree is newer — with the "repository version" note on stderr
        let sourcing = Sourcing {
            mode: InstallMode::Repo,
            ask: false,
            open: Some(open_repo(vec![repo_entry("curl", "1.0", 1)])),
        };
        let p = plan::Plan {
            items: vec![item("curl", "2.0")],
            ..Default::default()
        };
        let ui = recorder.ui(true, 'r');
        let build = recorder.build_fn(false);
        let request = plain_request(&build, false);
        let result = apply(&ctx, &p, &[], &sourcing, &request, &ui);
        assert!(result.is_err()); // the repository archive does not exist
        let note = recorder
            .find("repository has curl 1.0-1, the recipe is 2.0-1")
            .unwrap();
        let show = recorder.find("SHOW").unwrap();
        let confirm = recorder.find("CONFIRM").unwrap();
        let fetch = recorder.find("fetching").unwrap();
        assert!(note < show);
        assert!(show < confirm);
        assert!(confirm < fetch);
        assert_eq!(
            recorder.of("SHOW "),
            ["SHOW curl|2.0-1|repo|0|0|explicit".to_string()]
        );
    }

    #[test]
    fn first_item_failure_leaves_no_continuation_note() {
        let recorder = Recorder::default();
        let ctx = ctx_with(&crate::testutil::tmpdir("apply-firstfail"));
        let sourcing = Sourcing::build_only();
        let p = plan::Plan {
            items: vec![item("curl", "1.0"), item("wget", "1.0")],
            ..Default::default()
        };
        let ui = recorder.ui(true, 'r');
        let build = recorder.build_fn(true);
        let request = plain_request(&build, false);
        let error = apply(&ctx, &p, &[], &sourcing, &request, &ui).unwrap_err();
        assert!(error.to_string().contains("boom building curl"));
        assert_eq!(*recorder.builds.borrow(), vec![("curl".to_string(), 1, 2)]);
        assert!(recorder
            .stderr()
            .iter()
            .all(|l| !l.contains("were installed before this failure")));
    }
}
