//! install / remove / update / autoremove / outdated handlers and the plan runner.

use crate::adapters::config::InstallMode;
use crate::adapters::db;
use crate::adapters::lock;
use crate::adapters::pathutil::is_archive_arg;
use crate::application::context::Ctx;
use crate::application::features::package::apply_transaction::{
    apply_transaction, archive_name_of, Applied, ApplyRequest,
};
use crate::application::features::package::plan;
use crate::application::features::package::plan_install;
use crate::application::features::package::revdep;
use crate::application::features::system::profile as sys_profile;
use crate::application::ports::user_interaction::{PlanView, UserInteraction};
use crate::application::recovery;
use crate::cli::PlanFlags;
use crate::commands::build_one;
use crate::commands::warn_revdep;
use crate::domain::package::version;
use crate::groups;
use crate::install;
use crate::presentation::color::Palette;
use crate::presentation::color::Stream;
use crate::presentation::confirm;
use crate::presentation::humanize::format_date;
use crate::presentation::humanize::format_duration;
use crate::presentation::humanize::format_size;
use crate::suggest;
use crate::transaction;
use anyhow::{bail, Result};
use std::path::Path;
use std::path::PathBuf;

pub(crate) fn cmd_install(
    ctx: &Ctx,
    targets: &[String],
    flags: &PlanFlags,
    no_deps: bool,
    key: Option<PathBuf>,
    mode: Option<InstallMode>,
    repo: Option<String>,
) -> Result<()> {
    cmd_install_with_why(
        ctx,
        targets,
        flags,
        no_deps,
        key,
        mode,
        repo,
        plan::Why::Explicit,
        None,
    )
}

#[allow(clippy::too_many_arguments)] // mirrors cmd_install and adds an explicit install reason
pub(crate) fn cmd_install_with_why(
    ctx: &Ctx,
    targets: &[String],
    flags: &PlanFlags,
    no_deps: bool,
    key: Option<PathBuf>,
    mode: Option<InstallMode>,
    repo: Option<String>,
    target_why: plan::Why,
    group_id: Option<&str>,
) -> Result<()> {
    let archives = targets.iter().filter(|t| is_archive_arg(t)).count();
    if archives == targets.len() {
        for t in targets {
            install_archive_file(
                ctx,
                Path::new(t),
                flags.force,
                flags.allow_unsigned,
                key.clone(),
            )?;
        }
        let changed: Vec<String> = targets
            .iter()
            .filter_map(|t| revdep::archive_name(Path::new(t)))
            .collect();
        warn_revdep(ctx, &changed);
        return Ok(());
    }
    if archives > 0 {
        bail!("do not mix package names and archive files in one `nawa install`");
    }
    if key.is_some() {
        bail!("--key only applies to archive files (package names come from the repository or are built locally)");
    }

    let known = ctx.load_all()?;
    let installed = installed_map(ctx)?;

    // The effective target profile is an explicit plan input (§8.1). An
    // invalid profile fails closed: refuse rather than plan against a
    // broken desired state (§7).
    let effective = sys_profile::load_effective(&ctx.root)?;

    // Complete interrupted (or legacy, untracked) hooks before consulting the
    // repository. A retry must work for binary-only packages even if the
    // repository is temporarily unavailable, and must not reinstall payloads.
    let mut remaining_targets = Vec::new();
    for t in targets {
        if installed.contains_key(t) && install::hook_needs_retry(&ctx.root, t)? {
            crate::nawa_info!("retrying incomplete post-install hook for {t}");
            let _lock = lock::acquire(&ctx.root, lock::Mode::Exclusive)?;
            // repair a killed transaction before touching package state; this
            // command takes a short lock per transaction instead of one for
            // the whole command, so `main` cannot do it for us
            recovery::recover_and_report(&ctx.root);
            install::retry_pending_hook(t, &ctx.root)?;
            // Recipe-backed targets still go through normal source/version
            // selection after the hook is repaired.
            if known.contains_key(t) {
                remaining_targets.push(t.clone());
            }
        } else {
            remaining_targets.push(t.clone());
        }
    }
    if remaining_targets.is_empty() {
        return Ok(());
    }

    // where packages come from: flag > install_mode (env / nawa.toml / default)
    let mode = mode.unwrap_or(ctx.install_mode);
    let repo_url = repo.or_else(|| ctx.repo.clone());
    let (sourcing, sourcing_notes) = plan_install::open_sourcing(
        ctx,
        mode,
        repo_url.as_deref(),
        flags.allow_unsigned,
        flags.force,
    )?;
    for note in &sourcing_notes {
        note.emit();
    }

    // plan the recipe-backed part and split the repository-only targets.
    // The notices print even when planning fails, exactly as the inline
    // version printed them before the planner bailed.
    let mut notices = Vec::new();
    let planned = plan_install::plan(
        &remaining_targets,
        &known,
        &installed,
        &sourcing,
        &effective,
        target_why,
        !no_deps,
        &mut notices,
    );
    for notice in &notices {
        notice.emit();
    }
    let outcome = planned?;
    let plan = outcome.plan;
    let extras = outcome.extras;
    if plan.items.is_empty() && extras.is_empty() {
        // A group install still changes durable membership even when every
        // selected package is already present, so keep it visible in history.
        if let Some(id) = group_id.filter(|_| !flags.dry_run) {
            let _lock = lock::acquire(&ctx.root, lock::Mode::Exclusive)?;
            // recover before `begin_operation`: its own TxWAL must never be
            // mistaken for an abandoned transaction
            recovery::recover_and_report(&ctx.root);
            let mut op = transaction::begin_operation(
                &ctx.root,
                transaction::OperationCommand::GroupInstall,
                &remaining_targets,
                &[],
            )?;
            op.record_group(id);
            op.finish(None)?;
        }
        return Ok(());
    }
    // one transaction for the whole command (roadmap §5.1): the id is
    // allocated under the exclusive db lock inside begin_operation
    let repo_ids: Vec<String> = sourcing
        .open
        .as_ref()
        .and_then(|o| o.base_url.clone())
        .into_iter()
        .collect();
    {
        let _lock = lock::acquire(&ctx.root, lock::Mode::Exclusive)?;
        // this command locks per install step, so `main` has not run recovery;
        // do it before the transaction's state and intents exist
        recovery::recover_and_report(&ctx.root);
        let command = if group_id.is_some() {
            transaction::OperationCommand::GroupInstall
        } else {
            transaction::OperationCommand::Install
        };
        let mut op =
            transaction::begin_operation(&ctx.root, command, &remaining_targets, &repo_ids)?;
        if let Some(id) = group_id {
            op.record_group(id);
        }
        drop(_lock);
        let build = |ctx: &Ctx, it: &plan::Item, n: usize, total: usize| {
            build_one(ctx, it, flags, n, total)
        };
        let request = ApplyRequest {
            build: &build,
            force: flags.force,
            allow_unsigned: flags.allow_unsigned,
            dry_run: flags.dry_run,
            extras_why: target_why,
        };
        let ui = interaction(flags.yes);
        let result = apply_transaction(ctx, &plan, &extras, &sourcing, &mut op, &request, &ui);
        let outcome = match result {
            Ok(applied) => applied,
            Err(err) => {
                let _lock = lock::acquire(&ctx.root, lock::Mode::Exclusive)?;
                op.finish(Some(&err))?;
                return Err(err);
            }
        };
        {
            let _lock = lock::acquire(&ctx.root, lock::Mode::Exclusive)?;
            op.finish(None)?;
        }
        match outcome {
            Applied::Done { changed, seconds } => {
                crate::nawa_success!(
                    "done — {} package(s) installed in {}",
                    changed.len(),
                    format_duration(seconds)
                );
                warn_revdep(ctx, &changed);
            }
            Applied::NothingToDo => {}
        }
        Ok(())
    }
}

/// Render a use-case notice through the standard status macros. The method
/// lives in the presentation layer (`commands`) on purpose: the application
/// layer *returns* notices, it never prints them.
impl crate::application::features::package::plan_install::Notice {
    pub(crate) fn emit(&self) {
        match self {
            Self::Note(text) => crate::nawa_note!("{}", text),
            Self::Info(text) => crate::nawa_info!("{}", text),
        }
    }
}

/// The presentation adapter for [`apply_transaction`]: own boxed closures
/// wired to the real prompts, status macros and the plan-table renderer, so
/// the port object is self-contained. `yes` pre-answers every confirmation.
fn interaction(yes: bool) -> UserInteraction {
    UserInteraction {
        stderr_line: Box::new(|line| eprintln!("{line}")),
        info: Box::new(|line| crate::nawa_info!("{}", line)),
        warn: Box::new(|line| crate::nawa_warn!("{}", line)),
        show_plan: Box::new(render_plan),
        choose: Box::new(confirm::choose),
        confirm: Box::new(move |prompt| confirm::confirm(prompt, yes)),
    }
}

/// Render the plan table from the port's display data (the dnf-style table
/// with sizes and the totals line). Moved verbatim from the former
/// `print_plan`; the use case now supplies the rows, the header info line
/// and the exact wording.
fn render_plan(view: &PlanView) {
    let pal = Palette::auto(Stream::Stdout);
    let rows = &view.rows;
    let size_cell = |n: u64| {
        if n == 0 {
            "-".to_string()
        } else {
            format_size(n)
        }
    };
    let wn = rows
        .iter()
        .map(|r| r.name.len())
        .max()
        .unwrap_or(0)
        .max("Package".len());
    let wv = rows
        .iter()
        .map(|r| r.ver.len())
        .max()
        .unwrap_or(0)
        .max("Version".len());
    let wd = rows
        .iter()
        .map(|r| size_cell(r.dl).len())
        .max()
        .unwrap_or(0)
        .max("Download".len());
    let wi = rows
        .iter()
        .map(|r| size_cell(r.inst).len())
        .max()
        .unwrap_or(0)
        .max("Installed".len());
    let wh = rows
        .iter()
        .map(|r| r.sha256.as_ref().map_or(1, String::len))
        .max()
        .unwrap_or(1)
        .max("SHA-256".len());
    if let Some(repository) = &view.repository {
        println!("Repository: {repository}");
    }
    println!();
    println!(
        "{}",
        pal.header(&format!(
            "  {:>3}  {:<wn$}  {:<wv$}  {:<6}  {:>wd$}  {:>wi$}  {:<wh$}  Reason",
            "#", "Package", "Version", "Source", "Download", "Installed", "SHA-256"
        ))
    );
    for (i, r) in rows.iter().enumerate() {
        let (dl, inst) = (size_cell(r.dl), size_cell(r.inst));
        println!(
            "  {:>3}  {}  {}  {}  {dl:>wd$}  {inst:>wi$}  {:<wh$}  {}",
            i + 1,
            pal.name(&format!("{:<wn$}", r.name)),
            pal.ver(&format!("{:<wv$}", r.ver)),
            pal.dim(&format!("{:<6}", r.tag)),
            r.sha256.as_deref().unwrap_or("-"),
            pal.dim(&r.note),
        );
    }
    let dl_sum: u64 = rows.iter().map(|r| r.dl).sum();
    let inst_sum: u64 = rows.iter().map(|r| r.inst).sum();
    let unknown = rows.iter().filter(|r| r.inst == 0).count();
    println!();
    let mut line = format!(
        "nawa: download size {}, installed size {}",
        format_size(dl_sum),
        format_size(inst_sum)
    );
    if unknown > 0 {
        line.push_str(&format!(
            " (not counting {unknown} package(s) of unknown size)"
        ));
    }
    println!("{line}");
}

/// The installed packages by name, read under a brief shared lock.
pub(crate) fn installed_map(ctx: &Ctx) -> Result<std::collections::HashMap<String, db::Entry>> {
    let _lock = lock::acquire(&ctx.root, lock::Mode::Shared)?;
    Ok(db::Db::new(&ctx.root)
        .installed()?
        .into_iter()
        .map(|e| (e.name.clone(), e))
        .collect())
}

pub(crate) fn cmd_outdated(ctx: &Ctx) -> Result<()> {
    let known = ctx.load_all()?;
    let installed = db::Db::new(&ctx.root).installed()?;
    let list = plan::outdated(&installed, &known);
    if list.is_empty() {
        crate::nawa_info!("everything is up to date ({} installed)", installed.len());
        return Ok(());
    }
    let pal = Palette::auto(Stream::Stdout);
    for (e, r) in &list {
        println!(
            "{} {} -> {}",
            pal.name(&format!("{:<24}", e.name)),
            pal.dim(&format!("{:<14}", format!("{}-{}", e.version, e.rel))),
            pal.ver(&format!("{}-{}", r.package.version, r.package.rel)),
        );
    }
    crate::nawa_info!(
        "{} package(s) can be updated — run `nawa update`",
        list.len()
    );
    Ok(())
}

pub(crate) fn cmd_update(
    ctx: &Ctx,
    names: &[String],
    flags: &PlanFlags,
    mode: Option<InstallMode>,
    repo: Option<String>,
) -> Result<()> {
    let mode = mode.unwrap_or_else(|| {
        if repo.is_some() {
            InstallMode::Repo
        } else {
            InstallMode::Build
        }
    });

    let known = ctx.load_all()?;
    let installed = installed_map(ctx)?;
    // The effective target profile is an explicit plan input (§8.1); an
    // invalid profile fails closed (§7).
    let effective = sys_profile::load_effective(&ctx.root)?;
    for n in names {
        if !installed.contains_key(n) {
            bail!("`{n}` is not installed (use `nawa install {n}`)");
        }
    }
    let list: Vec<db::Entry> = installed.values().cloned().collect();
    let mut targets: Vec<String> = plan::outdated(&list, &known)
        .into_iter()
        .map(|(e, _)| e.name.clone())
        .filter(|n| names.is_empty() || names.contains(n))
        .collect();
    targets.sort(); // deterministic update plan even though installed_map is a HashMap
    if targets.is_empty() {
        crate::nawa_info!("everything is up to date");
        return Ok(());
    }
    let repo_url = repo.or_else(|| ctx.repo.clone());
    let (sourcing, sourcing_notes) = plan_install::open_sourcing(
        ctx,
        mode,
        repo_url.as_deref(),
        flags.allow_unsigned,
        flags.force,
    )?;
    for note in &sourcing_notes {
        note.emit();
    }

    // Strict repository updates must validate every requested package before
    // resolving or opening the transaction. A stale archive is never a
    // successful update merely because it can be installed.
    if mode == InstallMode::Repo {
        for name in &targets {
            let recipe = known
                .get(name)
                .ok_or_else(|| anyhow::anyhow!("no recipe for {name}"))?;
            let re = sourcing.entry(name).ok_or_else(|| {
                anyhow::anyhow!("{name} has no package for this architecture in the repository")
            })?;
            let available = format!("{}-{}", re.version, re.rel);
            let wanted = format!("{}-{}", recipe.package.version, recipe.package.rel);
            if version::vercmp(&available, &wanted) == std::cmp::Ordering::Less {
                bail!(
                    "repository has {name} {available}, but the recipe requires {wanted}; publish the newer package first, or use --from auto / --build"
                );
            }
        }
    }

    let mut notices = Vec::new();
    let planned = plan_install::plan(
        &targets,
        &known,
        &installed,
        &sourcing,
        &effective,
        plan::Why::Update,
        true,
        &mut notices,
    );
    for notice in &notices {
        notice.emit();
    }
    let outcome = planned?;
    let plan = outcome.plan;
    let extras = outcome.extras;
    if let Some(note) = plan_install::provider_note(&plan) {
        note.emit();
    }
    if plan.items.is_empty() && extras.is_empty() {
        crate::nawa_info!("everything is up to date");
        return Ok(());
    }

    let repo_ids: Vec<String> = sourcing
        .open
        .as_ref()
        .and_then(|o| o.base_url.clone())
        .into_iter()
        .collect();
    let mut op = {
        let _lock = lock::acquire(&ctx.root, lock::Mode::Exclusive)?;
        // `update` locks per install step; recover before opening its own
        // transaction so a prior crash cannot be reconciled as this one
        recovery::recover_and_report(&ctx.root);
        transaction::begin_operation(
            &ctx.root,
            transaction::OperationCommand::Update,
            &targets,
            &repo_ids,
        )?
    };
    let build =
        |ctx: &Ctx, it: &plan::Item, n: usize, total: usize| build_one(ctx, it, flags, n, total);
    let request = ApplyRequest {
        build: &build,
        force: flags.force,
        allow_unsigned: flags.allow_unsigned,
        dry_run: flags.dry_run,
        extras_why: plan::Why::Update,
    };
    let ui = interaction(flags.yes);
    let result = apply_transaction(ctx, &plan, &extras, &sourcing, &mut op, &request, &ui);
    let outcome = match result {
        Ok(applied) => applied,
        Err(err) => {
            let _lock = lock::acquire(&ctx.root, lock::Mode::Exclusive)?;
            op.finish(Some(&err))?;
            // also after a partial run: whatever was installed is worth
            // checking
            let changed: Vec<String> = plan
                .items
                .iter()
                .map(|it| it.rec.package.name.clone())
                .chain(extras.iter().map(|entry| entry.name.clone()))
                .collect();
            warn_revdep(ctx, &changed);
            return Err(err);
        }
    };
    {
        let _lock = lock::acquire(&ctx.root, lock::Mode::Exclusive)?;
        op.finish(None)?;
    }
    if let Applied::Done { seconds, .. } = outcome {
        crate::nawa_success!(
            "done — {} package(s) installed in {}",
            plan.items.len() + extras.len(),
            format_duration(seconds)
        );
    }
    // also after a partial run: whatever was installed is worth checking
    let changed: Vec<String> = plan
        .items
        .iter()
        .map(|it| it.rec.package.name.clone())
        .chain(extras.iter().map(|entry| entry.name.clone()))
        .collect();
    warn_revdep(ctx, &changed);
    Ok(())
}

pub(crate) fn cmd_autoremove(ctx: &Ctx, dry_run: bool, yes: bool, build_deps: bool) -> Result<()> {
    let known = ctx.load_all()?;
    if known.is_empty() && !build_deps {
        bail!(
            "no recipes under {} — cannot tell which dependencies are build tools to keep \
             (point --recipes at the tree, or pass --build-deps to ignore build tools)",
            ctx.recipes.display()
        );
    }
    let installed = db::Db::new(&ctx.root).installed()?;
    // Membership files are the authoritative roots for package groups. Do
    // not let autoremove discard a direct group member merely because it was
    // originally pulled in as a dependency before the group was installed.
    let group_owned = groups::membership::group_owned_packages(&ctx.root, None)?;
    let mut orphan_view = installed.clone();
    for entry in &mut orphan_view {
        if group_owned.contains(&entry.name) && entry.reason == "dependency" {
            entry.reason = "group".to_string();
        }
    }
    let orphans = plan::orphans(&orphan_view, &known, build_deps);
    if orphans.is_empty() {
        crate::nawa_info!("nothing to remove");
        return Ok(());
    }
    let order = plan::removal_order(&orphans, &installed);
    let pal = Palette::auto(Stream::Stdout);
    crate::nawa_info!("{} unneeded dependency package(s):", order.len());
    for n in &order {
        println!("  {}", pal.name(n));
    }
    if dry_run {
        crate::nawa_info!("dry run — nothing was removed");
        return Ok(());
    }
    confirm::confirm(&format!("nawa: remove {} package(s)?", order.len()), yes)?;
    // the command holds the exclusive db lock for its whole duration
    // (see cli::lock_mode), so no per-step locking happens here
    let mut op = transaction::begin_operation(
        &ctx.root,
        transaction::OperationCommand::Autoremove,
        &[],
        &[],
    )?;
    for (i, n) in order.iter().enumerate() {
        crate::adapters::interrupt::check()?;
        crate::nawa_info!("[{}/{}] removing {n}", i + 1, order.len());
        let db = db::Db::new(&ctx.root);
        let before = db.entry(n)?;
        let mut recorder = op.recorder();
        let step_no = recorder.step_intent(
            n,
            transaction::history::StepAction::Remove,
            before.as_ref(),
            None,
            "",
        )?;
        let file_journal = install::FileJournalId::for_step(recorder.tx(), step_no);
        let result = install::remove_with_linked(n, &ctx.root, false, file_journal);
        install::record_remove_outcome(
            &mut recorder,
            &ctx.root,
            step_no,
            n,
            transaction::history::StepAction::Remove,
            before.as_ref(),
            "autoremove",
            &result,
        )?;
        if let Err(err) = result {
            op.finish(Some(&err))?;
            return Err(err);
        }
    }
    op.finish(None)?;
    crate::nawa_success!("done — {} package(s) removed", order.len());
    Ok(())
}

pub(crate) fn install_archive_file(
    ctx: &Ctx,
    file: &Path,
    force: bool,
    allow_unsigned: bool,
    key: Option<PathBuf>,
) -> Result<()> {
    if !file.exists() {
        // a near-miss hint saves a trip to `ls build/out`
        let dir = match file.parent() {
            Some(d) if !d.as_os_str().is_empty() => d,
            _ => Path::new("."),
        };
        let wanted = file
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| file.display().to_string());
        let names: Vec<String> = std::fs::read_dir(dir)
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .map(|e| e.file_name().to_string_lossy().to_string())
                    .collect()
            })
            .unwrap_or_default();
        bail!(
            "archive not found: {}{}",
            file.display(),
            suggest::hint(&wanted, &names)
        );
    }
    // the caller (cmd_install) holds the exclusive lock for archive installs
    let db = db::Db::new(&ctx.root);
    let pkg_name = archive_name_of(file)
        .ok_or_else(|| anyhow::anyhow!("cannot read package name from {}", file.display()))?;
    let before = db.entry(&pkg_name)?;
    let target = install::inspect_archive_state(
        file,
        crate::adapters::db::InstallReason::Explicit,
        before.as_ref(),
    )?;
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
    let digest = target.artifact_sha256.clone();
    let mut op = transaction::begin_operation(
        &ctx.root,
        transaction::OperationCommand::ArchiveInstall,
        std::slice::from_ref(&pkg_name),
        &[],
    )?;
    let mut recorder = op.recorder();
    let step_no =
        recorder.step_intent_state(&pkg_name, action, before.as_ref(), Some(&target), &digest)?;
    let file_journal = install::FileJournalId::for_step(recorder.tx(), step_no);
    let result = install::install_archive_pinned(
        file,
        &ctx.root,
        install::InstallOpts {
            force,
            allow_unsigned,
            key: key.as_deref(),
            reason: crate::adapters::db::InstallReason::Explicit,
            pinned_sha: Some(&digest),
            file_journal,
        },
    );
    install::record_install_outcome(
        &mut recorder,
        &ctx.root,
        step_no,
        &pkg_name,
        action,
        before.as_ref(),
        &target,
        "archive",
        &result,
    )?;
    op.finish(result.as_ref().err())?;
    result?;
    Ok(())
}

pub(crate) fn cmd_remove(ctx: &Ctx, name: &str, force: bool, yes: bool) -> Result<()> {
    let db = db::Db::new(&ctx.root);
    let entry = match db.entry(name)? {
        Some(e) => e,
        None => bail!("`{name}` is not installed"),
    };
    if crate::domain::planning::removal::is_protected_package(name) {
        bail!("cannot remove protected core package `{name}`; it is required for a usable Faseela system");
    }
    let dependents: Vec<String> = db
        .installed()?
        .into_iter()
        .filter(|e| {
            e.name != name
                && e.run
                    .iter()
                    .any(|r| crate::domain::package::relations::depends_on(r, name))
        })
        .map(|e| e.name)
        .collect();
    let nfiles = db.files(name)?.len();

    // the confirmation shows what will actually happen; the removal itself
    // re-checks dependents (refusing without --force) after the prompt
    let pal = Palette::auto(Stream::Stdout);
    crate::nawa_info!(
        "{} {} ({} file(s))",
        pal.warn("removing"),
        pal.name(name),
        nfiles
    );
    println!(
        "     {} {} — {}",
        pal.dim("version"),
        pal.ver(&entry.version_full()),
        pal.dim(&format_date(entry.install_date))
    );
    if !dependents.is_empty() {
        println!(
            "     {} {}",
            pal.warn("required by:"),
            pal.name(&dependents.join(", "))
        );
        if !force {
            println!(
                "     {}",
                pal.warn("removal will fail unless --force is passed")
            );
        }
    }
    confirm::confirm(
        &format!("nawa: remove {name} from {}?", ctx.root.display()),
        yes,
    )?;
    let db = db::Db::new(&ctx.root);
    let before = db.entry(name)?;
    let mut op = transaction::begin_operation(
        &ctx.root,
        transaction::OperationCommand::Remove,
        &[name.to_string()],
        &[],
    )?;
    let mut recorder = op.recorder();
    let step_no = recorder.step_intent(
        name,
        transaction::history::StepAction::Remove,
        before.as_ref(),
        None,
        before
            .as_ref()
            .map(|b| b.artifact_sha256.as_str())
            .unwrap_or(""),
    )?;
    let file_journal = install::FileJournalId::for_step(recorder.tx(), step_no);
    let result = install::remove_with_linked(name, &ctx.root, force, file_journal);
    install::record_remove_outcome(
        &mut recorder,
        &ctx.root,
        step_no,
        name,
        transaction::history::StepAction::Remove,
        before.as_ref(),
        "remove",
        &result,
    )?;
    op.finish(result.as_ref().err())?;
    result?;
    // the removed package is gone: this checks what depended on it
    warn_revdep(ctx, &[name.to_string()]);
    Ok(())
}
