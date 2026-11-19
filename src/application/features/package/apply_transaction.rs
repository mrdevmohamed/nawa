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

use crate::adapters::config::InstallMode;
use crate::adapters::db;
use crate::adapters::lock;
use crate::application::context::Ctx;
use crate::application::features::package::plan;
use crate::application::features::package::plan_install::{Source, Sourcing};
use crate::application::features::system::migrate_coordinator::MigrationPermit;
use crate::application::ports::user_interaction::{PlanRow, PlanView, UserInteraction};
use crate::domain::system::migration::LegacyInitHandoff;
use crate::install;
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
    replacements: &'a [crate::domain::planning::install::Replacement],
    reason: crate::adapters::db::InstallReason,
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
    /// Present only on an approved system migration package step.
    legacy_init_handoff: Option<&'a LegacyInitHandoff>,
}

/// Lock ownership for one install step (§10.3). The ordinary path takes
/// and releases the exclusive lock around the install only — never
/// across a fetch or a build (the hold would starve readers for the
/// whole download). The coordinator path carries its own guard instead:
/// the migration coordinator holds the exclusive package-database lock
/// for the whole preparation phase and passes both the guard and the
/// typed permit through this internal call chain (§10.3: reacquiring
/// would deadlock against its own flock, and an ordinary acquire would
/// be refused by the now-active gate).
pub(crate) enum LockOwnership<'a> {
    /// acquire + release around this one install (every ordinary caller)
    PerStep,
    /// the caller holds the exclusive lock and is the migration
    /// coordinator: the borrowed guard proves lock ownership (a `DbLock`
    /// cannot be constructed without a real acquisition) and the permit
    /// proves §10.3 authorization; together they are the only legitimate
    /// way past the active mutation gate
    Coordinator {
        guard: &'a lock::DbLock,
        permit: &'a MigrationPermit,
    },
}

/// One staged-install request from the migration coordinator:
/// everything [`install_for_migration`] needs beside the shared
/// handles.
pub(crate) struct MigrationInstall<'a> {
    pub(crate) repository: Option<&'a upgrade::OpenRepo>,
    pub(crate) archive: &'a Path,
    /// the planned package name, for display
    pub(crate) what: &'a str,
    pub(crate) number: usize,
    pub(crate) total: usize,
    /// the sha256 the approved plan pinned (installer-verified)
    pub(crate) expected_sha256: Option<&'a str>,
    pub(crate) reason: crate::adapters::db::InstallReason,
    pub(crate) force: bool,
    pub(crate) allow_unsigned: bool,
    /// provenance for the history record
    pub(crate) source: &'a str,
    /// Opaque authority minted from the validated built-in migration.
    pub(crate) legacy_init_handoff: &'a LegacyInitHandoff,
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

    // Resolve the global ask choice before displaying or confirming the plan.
    // The user must approve the source, version and digest the executor uses.
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
            sources.fill(Source::Build);
        }
    }

    let from_repo = count_repo(&sources, extras);
    (ui.info)(&format!(
        "{total} package(s) to install into {} ({from_repo} from the repository, {} to build)",
        ctx.root.display(),
        total - from_repo
    ));
    (ui.show_plan)(&plan_view(plan, extras, &sources, sourcing));
    if request.dry_run {
        (ui.info)("dry run — nothing was built or installed");
        return Ok(Applied::NothingToDo);
    }
    if sourcing.ask && from_repo == 0 {
        (ui.info)(&format!("building all {total} package(s) from source"));
    }
    (ui.confirm)(&format!("nawa: install {total} package(s)?"))?;

    let started = std::time::Instant::now();
    let order = ordered_steps(plan, extras.len());
    for (position, action) in order.iter().enumerate() {
        crate::adapters::interrupt::check()?;
        let n = position + 1;
        let step_result = match action {
            plan::PlanStep::Recipe(index) => (|| -> Result<()> {
                let it = plan
                    .items
                    .get(*index)
                    .context("internal: recipe step outside plan")?;
                let source_choice = sources
                    .get(*index)
                    .context("internal: recipe source missing from plan")?;
                let id = it.rec.id();
                let (archive, index_sha256, signed_index) =
                    acquire(ctx, it, request, sourcing, source_choice, ui, n, total)?;
                let source = match source_choice {
                    Source::Repo(_) => "repository",
                    Source::Build => "local-build",
                };
                install_one(
                    ctx,
                    &archive,
                    InstallRequest {
                        replacements: &plan.removals,
                        reason: it.why.db_reason(),
                        what: &id,
                        number: n,
                        total,
                        force: request.force,
                        allow_unsigned: request.allow_unsigned,
                        index_sha256: index_sha256.as_deref(),
                        signed_index,
                        source,
                        legacy_init_handoff: None,
                    },
                    sourcing.open.as_ref(),
                    op,
                    ui,
                    LockOwnership::PerStep,
                )
            })(),
            plan::PlanStep::Binary(index) => (|| -> Result<()> {
                let re = extras
                    .get(*index)
                    .context("internal: binary step outside repository actions")?;
                let what = format!("{}-{}-{}", re.name, re.version, re.rel);
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
                let reason = plan
                    .binary_whys
                    .get(*index)
                    .copied()
                    .unwrap_or(request.extras_why)
                    .db_reason();
                install_one(
                    ctx,
                    &archive,
                    InstallRequest {
                        replacements: &plan.removals,
                        reason,
                        what: &what,
                        number: n,
                        total,
                        force: request.force,
                        allow_unsigned: request.allow_unsigned,
                        index_sha256: Some(re.sha256.as_str()),
                        signed_index,
                        source: "repository",
                        legacy_init_handoff: None,
                    },
                    sourcing.open.as_ref(),
                    op,
                    ui,
                    LockOwnership::PerStep,
                )
            })(),
        };
        if let Err(error) = step_result {
            if position > 0 {
                (ui.stderr_line)(&format!(
                    "nawa: {position} of {total} package(s) were installed before this failure — \
                     re-run the same command to continue (installed packages are skipped)"
                ));
            }
            return Err(error);
        }
    }

    let changed = order
        .iter()
        .map(|step| match step {
            plan::PlanStep::Recipe(index) => plan.items[*index].rec.package.name.clone(),
            plan::PlanStep::Binary(index) => extras[*index].name.clone(),
        })
        .collect();
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

/// Install one ready archive. The library functions never lock; with
/// [`LockOwnership::PerStep`] the db lock is held for this one install
/// only, never through a build or a download. With
/// [`LockOwnership::Coordinator`] the caller (the migration coordinator)
/// already holds it, and no acquire happens at all.
fn install_one(
    ctx: &Ctx,
    archive: &Path,
    details: InstallRequest<'_>,
    open_repo: Option<&upgrade::OpenRepo>,
    op: &mut transaction::OperationHandle,
    ui: &UserInteraction,
    ownership: LockOwnership<'_>,
) -> Result<()> {
    (ui.info)(&format!(
        "[{}/{}] installing {}",
        details.number, details.total, details.what
    ));
    let per_step_lock = match ownership {
        LockOwnership::PerStep => Some(lock::acquire(&ctx.root, lock::Mode::Exclusive)?),
        // the coordinator's own guard covers this install: no second
        // acquire (§10.3 — it would deadlock, and the gate is active)
        LockOwnership::Coordinator { guard, permit } => {
            let _proof_of_ownership = (guard, permit);
            None
        }
    };
    // keep the guard alive for the whole install
    let _held = per_step_lock;
    // Resolve the real package identity from .PKGINFO; archive filenames
    // are not authoritative package metadata.
    let package = archive_name_of(archive)
        .ok_or_else(|| anyhow::anyhow!("cannot read package name from {}", archive.display()))?;
    let db = db::Db::new(&ctx.root);
    let before = db.entry(&package)?;
    // A package that was explicitly requested must stay explicit even when
    // a group later upgrades it. "keep" preserves the existing DB reason.
    let effective_reason = if (details.reason == crate::adapters::db::InstallReason::Dependency
        && before.is_some())
        || (details.reason == crate::adapters::db::InstallReason::Group
            && before.as_ref().is_some_and(|entry| {
                entry.reason == crate::adapters::db::InstallReason::Explicit.as_str()
            })) {
        crate::adapters::db::InstallReason::Keep
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
    let replacements: Vec<_> = details
        .replacements
        .iter()
        .filter(|r| r.by == package)
        .collect();
    if !replacements.is_empty() {
        if details.force {
            bail!(
                "generic --force cannot be combined with an authenticated package replacement plan"
            );
        }
        // Authenticate the replacement archive before removing anything. The
        // installer still snapshots/rechecks the same pinned bytes afterwards.
        if !details.signed_index {
            crate::adapters::sign::verify_for_install(
                archive,
                &ctx.root,
                None,
                details.allow_unsigned,
            )?;
        }
        let info = crate::adapters::archive::parse_pkginfo(
            &crate::adapters::archive::read_pkginfo(archive)?,
        )?;
        let metadata = crate::domain::package::metadata::PkgMetadata::from_pkginfo_map(&info)
            .map_err(anyhow::Error::msg)?;
        let relations = metadata
            .relations
            .as_ref()
            .context("replacement archive has unknown package relations")?;
        if details.signed_index {
            let open = open_repo
                .context("signed replacement archive has no authenticated repository proof")?;
            let signer = open
                .signer_public
                .as_deref()
                .context("signed replacement archive has no trusted signer")?;
            let signature = open
                .index_signature
                .as_deref()
                .context("signed replacement archive has no index signature")?;
            crate::adapters::sign::verify_trusted_index_proof(
                &open.index_bytes,
                signature,
                &crate::adapters::checksum::sha256_bytes(signer),
                &ctx.root,
            )?;
            let indexed = open
                .index
                .find(&package)
                .context("signed index has no replacement package entry")?;
            if Some(indexed.sha256.as_str()) != details.index_sha256
                || std::fs::metadata(archive)?.len() != indexed.size
            {
                bail!("replacement archive identity differs from its authenticated index entry");
            }
            let indexed_metadata = open
                .metadata_for(&package)
                .context("signed index has no complete metadata for the replacement package")?;
            crate::domain::package::metadata::check_chain(&indexed_metadata, &metadata)
                .map_err(anyhow::Error::msg)
                .context("replacement archive metadata does not match its authenticated index")?;
        }
        let projected = db
            .installed()?
            .into_iter()
            .filter(|e| !replacements.iter().any(|r| r.name == e.name))
            .map(|e| (e.name, format!("{}-{}", e.version, e.rel)))
            .collect();
        metadata
            .verify_installed(&package, &projected)
            .map_err(anyhow::Error::msg)?;
        // Validate the complete removal set before its first side effect.
        for replacement in &replacements {
            let current = db
                .entry(&replacement.name)?
                .context("replacement package is no longer installed")?;
            if crate::domain::planning::install::version_rel(&current.version, current.rel)
                != replacement.current
            {
                bail!(
                    "installed replacement target changed after planning: {}",
                    replacement.name
                );
            }
            if !relations.replaces.iter().any(|raw| {
                crate::domain::package::relations::Requirement::parse(raw)
                    .is_ok_and(|r| r.name == replacement.name && r.matches(&replacement.current))
            }) {
                bail!(
                    "archive does not authorize the displayed replacement of {}",
                    replacement.name
                );
            }
            let files = db.files(&replacement.name)?;
            if matches!(
                replacement.name.as_str(),
                "runit" | "runit-services" | "systemd"
            ) || files.iter().any(|p| {
                matches!(
                    p.trim_start_matches('/'),
                    "usr/bin/init"
                        | "sbin/init"
                        | "usr/sbin/init"
                        | "usr/bin/runit"
                        | "usr/lib/systemd/systemd"
                )
            }) {
                bail!("ordinary package replacement cannot retire an init provider; a separately approved migration cleanup is required");
            }
            if db.installed()?.iter().any(|e| {
                e.name != replacement.name
                    && e.run.iter().any(|raw| {
                        crate::domain::package::relations::Requirement::parse(raw)
                            .is_ok_and(|r| r.name == replacement.name)
                    })
            }) {
                bail!(
                    "replacement target {} still has installed dependents",
                    replacement.name
                );
            }
        }
        for replacement in replacements {
            let old = db.entry(&replacement.name)?;
            let mut recorder = op.recorder();
            let number = recorder.step_intent(
                &replacement.name,
                transaction::history::StepAction::Remove,
                old.as_ref(),
                None,
                old.as_ref()
                    .map(|e| e.artifact_sha256.as_str())
                    .unwrap_or(""),
            )?;
            let journal = install::FileJournalId::for_step(recorder.tx(), number);
            let result = install::remove_with_linked(&replacement.name, &ctx.root, false, journal)
                .and_then(|()| {
                    if db.entry(&replacement.name)?.is_some() {
                        bail!("replacement removal failed its database postcondition");
                    }
                    Ok(())
                });
            install::record_remove_outcome(
                &mut recorder,
                &ctx.root,
                number,
                &replacement.name,
                transaction::history::StepAction::Remove,
                old.as_ref(),
                "approved-replacement",
                &result,
            )?;
            result?;
        }
    }
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
            let metadata = open_repo.and_then(|open| open.metadata_for(&package));
            let proof = open_repo.map(|open| install::IndexProof {
                index_bytes: Some(&open.index_bytes),
                index_signature: open.index_signature.as_deref(),
                signer_public: open.signer_public.as_deref(),
                // Repository URLs can contain credentials; durable migration
                // provenance lives in the plan's source fingerprint instead.
                repo_id: if details.legacy_init_handoff.is_some() {
                    Some(details.source)
                } else {
                    open.base_url.as_deref()
                },
                metadata: metadata.as_ref(),
            });
            match (details.legacy_init_handoff, proof) {
                (Some(authority), Some(proof)) => {
                    install::install_archive_from_index_proof_for_migration(
                        archive,
                        &ctx.root,
                        expected,
                        effective_reason,
                        proof,
                        file_journal,
                        authority,
                    )
                }
                (_, proof) => install::install_archive_from_index_proof(
                    archive,
                    &ctx.root,
                    details.force,
                    expected,
                    effective_reason,
                    proof,
                    file_journal,
                ),
            }
        }
        pinned => {
            let expected = pinned.unwrap_or(target.artifact_sha256.as_str());
            let opts = install::InstallOpts {
                force: details.force,
                allow_unsigned: details.allow_unsigned,
                key: None,
                reason: effective_reason,
                pinned_sha: Some(expected),
                file_journal,
            };
            match details.legacy_init_handoff {
                Some(authority) => install::install_archive_pinned_for_migration(
                    archive, &ctx.root, opts, authority,
                ),
                None => install::install_archive_pinned(archive, &ctx.root, opts),
            }
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

/// Install one staged archive as part of an approved migration plan
/// (§10.3): the same verified install path as every ordinary install
/// ([`install_one`]), but under the coordinator's lock ownership — the
/// caller holds the exclusive lock (the borrowed guard proves it) and
/// presents the unforgeable permit (§10.3). The coordinator's staged
/// artifacts are verified before the mutation phase (§10.4 stage 4), so
/// nothing is fetched or built here.
pub(crate) fn install_for_migration(
    ctx: &Ctx,
    request: MigrationInstall<'_>,
    op: &mut transaction::OperationHandle,
    ui: &UserInteraction,
    guard: &lock::DbLock,
    permit: &MigrationPermit,
) -> Result<()> {
    if request.force {
        bail!("migration package steps cannot use generic --force; the typed legacy-init handoff is the only permitted conflict exception");
    }
    #[cfg(not(test))]
    if request.repository.is_none() || request.expected_sha256.is_none() || request.allow_unsigned {
        bail!("migration install requires the approved trusted index proof and exact archive pin");
    }
    install_one(
        ctx,
        request.archive,
        InstallRequest {
            replacements: &[],
            reason: request.reason,
            what: request.what,
            number: request.number,
            total: request.total,
            force: request.force,
            allow_unsigned: request.allow_unsigned,
            index_sha256: request.expected_sha256,
            // Retain the exact authenticated index proof from staging.
            signed_index: request.repository.is_some(),
            source: request.source,
            legacy_init_handoff: Some(request.legacy_init_handoff),
        },
        request.repository,
        op,
        ui,
        LockOwnership::Coordinator { guard, permit },
    )
}

/// The package name inside an archive (from .PKGINFO), when readable.
pub(crate) fn archive_name_of(archive: &Path) -> Option<String> {
    let raw = crate::adapters::archive::read_pkginfo(archive).ok()?;
    let info = crate::adapters::archive::parse_pkginfo(&raw).ok()?;
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

fn ordered_steps(plan: &plan::Plan, binary_count: usize) -> Vec<plan::PlanStep> {
    if !plan.order.is_empty() {
        return plan.order.clone();
    }
    let mut steps = Vec::with_capacity(plan.items.len() + binary_count);
    steps.extend((0..plan.items.len()).map(plan::PlanStep::Recipe));
    steps.extend((0..binary_count).map(plan::PlanStep::Binary));
    steps
}

fn plan_view(
    plan: &plan::Plan,
    extras: &[repo::RepoEntry],
    sources: &[Source],
    sourcing: &Sourcing,
) -> PlanView {
    let mut rows: Vec<PlanRow> = Vec::with_capacity(plan.items.len() + extras.len());
    for removal in &plan.removals {
        rows.push(PlanRow {
            name: removal.name.clone(),
            ver: removal.current.clone(),
            tag: "remove",
            dl: 0,
            inst: 0,
            note: format!(
                "explicitly replaced by {} before its installation",
                removal.by
            ),
            sha256: None,
        });
    }
    for action in ordered_steps(plan, extras.len()) {
        match action {
            plan::PlanStep::Recipe(index) => {
                let it = &plan.items[index];
                let (tag, version, dl, inst, sha256) = match &sources[index] {
                    Source::Repo(re) => (
                        "repo",
                        format!("{}-{}", re.version, re.rel),
                        re.size,
                        re.installed_size,
                        Some(re.sha256.clone()),
                    ),
                    Source::Build => (
                        "build",
                        format!("{}-{}", it.rec.package.version, it.rec.package.rel),
                        0,
                        0,
                        None,
                    ),
                };
                let mut note = it.why.label().to_string();
                if matches!(&sources[index], Source::Repo(_)) && sourcing.mode == InstallMode::Auto
                {
                    note.push_str(" (build fallback if fetch fails)");
                }
                if let Some(old) = &it.replaces {
                    note.push_str(&format!(" (replaces {old})"));
                }
                rows.push(PlanRow {
                    name: it.rec.package.name.clone(),
                    ver: version,
                    tag,
                    dl,
                    inst,
                    note,
                    sha256,
                });
            }
            plan::PlanStep::Binary(index) => {
                let re = &extras[index];
                let why = plan
                    .binary_whys
                    .get(index)
                    .copied()
                    .unwrap_or(plan::Why::Explicit);
                let mut note = why.label().to_string();
                if let Some(old) = plan.binary_replaces.get(index).and_then(Option::as_ref) {
                    note.push_str(&format!(" (replaces {old})"));
                }
                rows.push(PlanRow {
                    name: re.name.clone(),
                    ver: format!("{}-{}", re.version, re.rel),
                    tag: "repo",
                    dl: re.size,
                    inst: re.installed_size,
                    note,
                    sha256: Some(re.sha256.clone()),
                });
            }
        }
    }
    for row in &mut rows {
        if plan.unknown_relations.contains(&row.name) {
            row.note
                .push_str(" (legacy conflicts/replacements unknown)");
        }
    }
    PlanView {
        repository: sourcing
            .open
            .as_ref()
            .and_then(|open| open.base_url.clone()),
        rows,
    }
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
            rundeps: None,
            provides: None,
            relations: None,
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
        Ctx::from_config(&crate::adapters::config::Config {
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
                            "{}|{}|{}|{}|{}|{}|{}",
                            r.name,
                            r.ver,
                            r.tag,
                            r.dl,
                            r.inst,
                            r.note,
                            r.sha256.as_deref().unwrap_or("-")
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
    fn plan_view_preserves_mixed_dependency_order() {
        let planned = plan::Plan {
            items: vec![item("app", "1.0")],
            order: vec![plan::PlanStep::Binary(0), plan::PlanStep::Recipe(0)],
            binary_whys: vec![plan::Why::Dependency],
            binary_replaces: vec![None],
            ..Default::default()
        };
        let binary = repo_entry("runtime-lib", "1.0", 1);
        let view = plan_view(
            &planned,
            &[binary],
            &[Source::Build],
            &Sourcing::build_only(),
        );
        assert_eq!(
            view.rows
                .iter()
                .map(|row| row.name.as_str())
                .collect::<Vec<_>>(),
            ["runtime-lib", "app"]
        );
        assert_eq!(view.rows[0].note, "dependency");
        assert_eq!(view.rows[0].sha256.as_deref(), Some("00"));
        assert_eq!(view.rows[1].note, "explicit");
        assert_eq!(view.rows[1].sha256, None);
        assert_eq!(view.repository, None);
    }

    fn replacement_archive(root: &Path, name: &str, relations: &str) -> PathBuf {
        let stage = root.join(format!("stage-{name}"));
        std::fs::create_dir_all(stage.join("usr/share/replacement")).unwrap();
        std::fs::write(stage.join("usr/share/replacement/file"), name).unwrap();
        let archive = root.join(format!("{name}-1-1.nawa.tar.zst"));
        crate::adapters::archive::create(
            &stage,
            &format!(
                "pkgname = {name}\npkgver = 1\npkgrel = 1\narch = x86_64\nrundeps = \n{relations}"
            ),
            &archive,
        )
        .unwrap();
        archive
    }

    #[test]
    fn explicit_replacement_reuses_the_package_remove_and_install_wal() {
        let root = crate::testutil::tmpdir("package-relations-replacement");
        let old = replacement_archive(&root, "old", "");
        {
            let _guard = lock::acquire(&root, lock::Mode::Exclusive)
                .unwrap()
                .unwrap();
            install::install_archive(&old, &root, false, false, None, db::InstallReason::Explicit)
                .unwrap();
        }
        let new = replacement_archive(
            &root,
            "new",
            "provides = \nrelations_schema = 1\nconflicts = old\nreplaces = old<2-1\n",
        );
        let removal = crate::domain::planning::install::Replacement {
            name: "old".into(),
            current: "1-1".into(),
            by: "new".into(),
        };
        let mut operation = transaction::begin_operation(
            &root,
            transaction::OperationCommand::Install,
            &["new".into()],
            &[],
        )
        .unwrap();
        let recorder = Recorder::default();
        install_one(
            &ctx_with(&root),
            &new,
            InstallRequest {
                replacements: &[removal],
                reason: db::InstallReason::Explicit,
                what: "new",
                number: 1,
                total: 1,
                force: false,
                allow_unsigned: false,
                index_sha256: None,
                signed_index: false,
                source: "local-build",
                legacy_init_handoff: None,
            },
            None,
            &mut operation,
            &recorder.ui(true, 'b'),
            LockOwnership::PerStep,
        )
        .unwrap();
        operation.finish(None).unwrap();
        let db = db::Db::new(&root);
        assert!(db.entry("old").unwrap().is_none());
        assert!(db.entry("new").unwrap().is_some());
        assert_eq!(
            std::fs::read_to_string(root.join("usr/share/replacement/file")).unwrap(),
            "new"
        );
        let records = transaction::history::list_records(&root).unwrap();
        let record = transaction::history::load_record(&root, records[0].id).unwrap();
        assert_eq!(
            record
                .package_changes
                .iter()
                .map(|c| (c.name.as_str(), c.action.as_str(), c.status.as_str()))
                .collect::<Vec<_>>(),
            [
                ("old", "remove", "committed"),
                ("new", "install", "committed")
            ]
        );
    }

    #[test]
    fn an_invalid_replacement_archive_cannot_remove_the_old_package() {
        let root = crate::testutil::tmpdir("package-relations-invalid-replacement");
        let old = replacement_archive(&root, "old", "");
        install::install_archive(&old, &root, false, false, None, db::InstallReason::Explicit)
            .unwrap();
        let new = replacement_archive(&root, "new", "");
        let mut operation = no_history();
        let recorder = Recorder::default();
        assert!(install_one(
            &ctx_with(&root),
            &new,
            InstallRequest {
                replacements: &[crate::domain::planning::install::Replacement {
                    name: "old".into(),
                    current: "1-1".into(),
                    by: "new".into()
                }],
                reason: db::InstallReason::Explicit,
                what: "new",
                number: 1,
                total: 1,
                force: true,
                allow_unsigned: false,
                index_sha256: None,
                signed_index: false,
                source: "local-build",
                legacy_init_handoff: None,
            },
            None,
            &mut operation,
            &recorder.ui(true, 'b'),
            LockOwnership::PerStep
        )
        .is_err());
        assert!(db::Db::new(&root).entry("old").unwrap().is_some());
        assert_eq!(
            std::fs::read_to_string(root.join("usr/share/replacement/file")).unwrap(),
            "old"
        );
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
            ["SHOW curl|1.0-1|build|0|0|explicit|-".to_string()]
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
        assert!(recorder.find("SHOW").is_none());
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
            ["SHOW curl|1.0-1|repo|0|0|explicit|00".to_string()]
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
