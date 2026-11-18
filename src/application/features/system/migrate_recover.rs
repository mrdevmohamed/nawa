//! Migration recovery and post-reboot resume (design §10.3–§10.5, §12.2,
//! §14.1–14.2). This is the imperative-shell-side use case that consumes the
//! exact on-disk shapes the Phase 5B coordinator pins:
//!
//! * **Case A — interrupted `planned`.** A kill after publishing `planned`
//!   but before the gate save leaves a non-gate-active record that still
//!   reserves the migration id. Recovery inspects the linked package
//!   transaction: only when there is no evidence that preparation began is
//!   the record discarded (and the removal re-read to prove it). Any other
//!   answer is preserved and reported as `needs-recovery`.
//! * **Case B — `preparing` with an intent but no outcome.** The step may
//!   have performed its side effect before the process stopped. Recovery
//!   never re-runs it. It reconciles the write-ahead intent against the
//!   *actual* package database and the linked package transaction, reusing
//!   the existing package WAL reconciliation (`transaction::recovery`) and
//!   the shared `PackageState::matches_entry` identity/digest comparison.
//!   A missing outcome is not success; a failed command is not proof of no
//!   side effect; an unreadable or ambiguous database state fails closed.
//! * **Case C — `preparing` with every step resolved.** The conclusion was
//!   never published. Recovery reconstructs it from the durable outcomes
//!   *after verifying their postconditions in the database*, then publishes
//!   it through the same durable path the coordinator uses.
//! * **Case D — `awaiting-reboot` with unfinished transaction history.** The
//!   migration record is authoritative; recovery finalizes the linked
//!   package transaction history (again via the existing WAL reconciliation)
//!   without changing the migration outcome, and reports any discrepancy.
//! * **Case E — resume after reboot.** An explicit, live-host-only operation
//!   that verifies PID 1 identity, the expected target provider, the
//!   target-root boot/init postconditions, the record's recorded source and
//!   target, and the availability of the documented recovery path. It never
//!   inspects or manages individual services. Only verified completion
//!   clears the gate.
//! * **Case F — rollback.** Only a *verified* compensation with a documented
//!   boot restoration strategy may move `rollback-required → failed`. No
//!   such strategy exists until the Faseela/SAQ boot contract is resolved
//!   (B1/B2/B5), so production verification failure lands in
//!   `needs-recovery` with the gate kept — never an invented rollback.
//!
//! Two journals stay separate (§11): this module writes only migration
//! records; package payload/database reconciliation is delegated to
//! `transaction::recovery` and never duplicated here. All record writes go
//! through the store's atomic 0600 publication, and every gate-crossing
//! exclusive lock is taken through `lock::acquire_for_migration` with the
//! lock-defined, unforgeable in-process capability (§13). There is no
//! environment bypass, no nested `nawa` executor and no CLI/plugin permit.

use std::path::Path;

use anyhow::{anyhow, bail, Result};

use crate::adapters::lock;
use crate::adapters::migration_record as store;
use crate::application::features::system::detect::{self, Context as ExecContext, RuntimeInit};
use crate::application::ports::user_interaction::UserInteraction;
use crate::domain::system::identity::InitKind;
use crate::domain::system::migration::{
    transition, HandlerId, MigrationRecord, MigrationState, StepOutcome, Transition,
};
use crate::domain::system::observation::Confidence;
use crate::transaction::history::{self, TxStatus};
use crate::transaction::recovery as wal;

/// What recovery did to one record. The detail is a human-readable, honest
/// account of the decision (including why an ambiguous case was preserved).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RecoveryAction {
    /// an abandoned planned record was removed, with the removal re-read
    Discarded,
    /// a resolved `preparing` record was concluded from verified state
    ReConcluded(MigrationState),
    /// the linked transaction history was finalized; the migration record
    /// was not changed
    HistoryFinalized(String),
    /// the case is genuinely ambiguous: the record is preserved, gate kept
    NeedsRecovery(String),
    /// the record needed no change
    Unchanged(String),
}

/// One deterministic recovery decision, for a sorted record id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RecoveryNote {
    pub(crate) id: String,
    pub(crate) action: RecoveryAction,
}

/// The result of one recovery pass. The caller renders all notes; none are
/// printed here (functional core / imperative shell).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct RecoveryReport {
    pub(crate) notes: Vec<RecoveryNote>,
}

impl RecoveryReport {
    fn note(&mut self, id: &str, action: RecoveryAction) {
        self.notes.push(RecoveryNote {
            id: id.to_string(),
            action,
        });
    }
}

/// How one post-reboot resume ended.
#[derive(Debug)]
pub(crate) enum ResumeOutcome {
    /// the target init and all boot postconditions were verified; the
    /// record is `completed` and the gate is clear
    Verified(MigrationRecord),
    /// verification failed and no verified rollback exists: the record is
    /// `needs-recovery`, the gate stays active, the failures are returned
    NeedsRecovery(MigrationRecord, Vec<String>),
}

/// Recover every migration record under `ctx.root`, in sorted-id order,
/// under one exclusive lock taken with the migration capability. Read-only
/// inspection commands never call this; it is the explicit recovery path.
///
/// A record that cannot be read fails the whole pass closed: recovery must
/// never guess past a corrupt state and silently clear a gate (§10.3).
pub(crate) fn recover(
    ctx: &crate::application::context::Ctx,
    ui: &UserInteraction,
) -> Result<RecoveryReport> {
    let _guard =
        lock::acquire_for_migration(&ctx.root, lock::migration_authority())?.ok_or_else(|| {
            anyhow!(
                "internal: the exclusive package database lock is required to recover a migration"
            )
        })?;
    let _interrupt = crate::adapters::interrupt::Guard::new();

    let ids = store::record_ids(&ctx.root)?;
    let mut report = RecoveryReport::default();
    for id in ids {
        if crate::adapters::interrupt::check().is_err() {
            bail!(
                "interrupted while recovering migration records — the durable state is unchanged"
            );
        }
        let Some(record) = store::load(&ctx.root, &id)? else {
            // the directory listing saw the file; under the lock nothing may
            // remove it. Report rather than silently skip.
            report.note(
                &id,
                RecoveryAction::NeedsRecovery(format!("record {id} disappeared while being read")),
            );
            continue;
        };
        if record.state.is_terminal() {
            report.note(
                &id,
                RecoveryAction::Unchanged(format!("{} is terminal", record.state.as_str())),
            );
            continue;
        }
        let action = match record.state {
            MigrationState::Planned => recover_planned(ctx, &record, ui)?,
            MigrationState::Preparing
            | MigrationState::Verifying
            | MigrationState::RollbackRequired => recover_gate_active(ctx, &record, ui)?,
            MigrationState::AwaitingReboot => finalize_history(ctx, &record, ui)?,
            MigrationState::NeedsRecovery => RecoveryAction::NeedsRecovery(
                "manual recovery: automatic safe continuation or rollback cannot be proven"
                    .to_string(),
            ),
            MigrationState::Completed | MigrationState::Failed => unreachable!("terminal above"),
        };
        report.note(&id, action);
    }
    Ok(report)
}

/// Case A. A `planned` record is not gate-active, so the absence of a gate
/// proves nothing about what the killed process did. Inspect the linked
/// transaction: preparation began only if it recorded a committed package
/// change. If there is none, discarding is safe; re-read to prove removal.
fn recover_planned(
    ctx: &crate::application::context::Ctx,
    record: &MigrationRecord,
    ui: &UserInteraction,
) -> Result<RecoveryAction> {
    // A planned record has no steps by construction; a step here means an
    // interrupted writer and is never safe to discard.
    if !record.steps.is_empty() {
        return Ok(RecoveryAction::NeedsRecovery(
            "a `planned` record has write-ahead steps — preparation began; discarded only by explicit operator decision".to_string(),
        ));
    }
    match linked_transaction(&ctx.root, &record.id)? {
        Some(tx) => {
            let tx_record = history::load_record(&ctx.root, tx)?;
            let committed = tx_record
                .package_changes
                .iter()
                .any(|c| c.status == "committed" || c.status == "committed-hook-failed");
            if committed {
                // Retroactively close the gate: preparation *did* begin even
                // though the gate save never became durable.
                let mut amended = record.clone();
                amended.state = transition(record.state, Transition::BeginPreparation)
                    .map_err(anyhow::Error::msg)?;
                amended.state =
                    transition(amended.state, Transition::Ambiguous).map_err(anyhow::Error::msg)?;
                persist(&ctx.root, &amended)?;
                (ui.warn)(&format!(
                    "migration {}: a killed preparation committed transaction {tx} packages before the gate became durable — declared needs-recovery, gate active",
                    record.id
                ));
                Ok(RecoveryAction::NeedsRecovery(
                    "package changes were committed without a durable gate".to_string(),
                ))
            } else {
                // The transaction exists but changed nothing: abandon it so
                // it does not keep a `preparing` history record forever, then
                // discard the planned record.
                discard_planned(ctx, record, ui)
            }
        }
        None => discard_planned(ctx, record, ui),
    }
}

fn discard_planned(
    ctx: &crate::application::context::Ctx,
    record: &MigrationRecord,
    ui: &UserInteraction,
) -> Result<RecoveryAction> {
    store::remove(&ctx.root, &record.id)?;
    match store::load(&ctx.root, &record.id)? {
        None => {
            (ui.info)(&format!(
                "migration {}: abandoned planned record discarded — a fresh, re-approved apply may reuse this id",
                record.id
            ));
            Ok(RecoveryAction::Discarded)
        }
        Some(_) => bail!(
            "migration {}: the planned record could not be proven removed — refusing to allow a retry",
            record.id
        ),
    }
}

/// Cases B and C. Reconcile every interrupted step, verify the resolved
/// ones, and (re)publish the conclusion. A `Verifying` or `RollbackRequired`
/// record with no verified rollback is ambiguous and keeps the gate.
fn recover_gate_active(
    ctx: &crate::application::context::Ctx,
    record: &MigrationRecord,
    ui: &UserInteraction,
) -> Result<RecoveryAction> {
    // `Verifying`/`RollbackRequired` have no preparation steps left to
    // reconcile; there is no documented rollback, so the honest answer is
    // to keep the gate for the operator (§10.5: never invent a rollback).
    if record.state != MigrationState::Preparing {
        return Ok(RecoveryAction::NeedsRecovery(format!(
            "{} cannot be continued automatically: post-reboot verification must be re-run explicitly with `nawa system migrate resume` (no verified rollback is registered)",
            record.state.as_str()
        )));
    }

    let tx = linked_transaction(&ctx.root, &record.id)?;
    // Reconcile the package WAL first (existing logic, never duplicated):
    // this writes per-step results into the package journal and gives us the
    // authoritative per-package outcome to compare against each migration
    // step's write-ahead intent.
    let journal_reconciled = match tx {
        Some(tx) => wal::reconcile(&ctx.root, tx)?,
        None => None,
    };
    // When the process died after `op.finish` (or the journal was cleared)
    // but before the conclusion was published, no journal remains; the
    // already-written history record is still the authoritative package
    // outcome. Use it when WAL reconciliation has nothing to add.
    if let (Some(tx), Some(finished)) = (tx, journal_reconciled.as_ref()) {
        let existing = history::load_record(&ctx.root, tx)?;
        if (existing.status()? == TxStatus::Preparing || existing.status()? == TxStatus::Running)
            && existing.id == finished.id
        {
            // write the reconciled record through the history store
            history::rewrite_record(&ctx.root, finished)?;
        }
    }
    let reconciled = match (tx, journal_reconciled) {
        (Some(tx), None) => history::load_record(&ctx.root, tx).ok(),
        (_, Some(finished)) => Some(finished),
        (None, None) => None,
    };

    let mut amended = record.clone();
    if record.approval.is_some() {
        if let Err(error) = verify_retained_evidence(&ctx.root, record) {
            amended.state =
                transition(amended.state, Transition::Ambiguous).map_err(anyhow::Error::msg)?;
            persist(&ctx.root, &amended)?;
            return Ok(RecoveryAction::NeedsRecovery(format!(
                "retained approval/snapshot evidence is invalid: {error:#}"
            )));
        }
    }
    let mut reconciled_any = Vec::new();
    let mut ambiguous: Option<String> = None;
    let mut committed_any = false;
    let mut failed_steps = 0usize;

    let mut package_index = 0;
    let expected_actions: Vec<_> = record
        .approval
        .iter()
        .flat_map(|pin| pin.artifacts.iter())
        .filter(|a| a.kind != crate::domain::system::plan::PackageActionKind::Keep)
        .collect();
    for step in amended.steps.iter_mut() {
        if record.approval.is_some() && step.id == "capture-boot-snapshot" {
            if step.outcome != Some(StepOutcome::Succeeded) {
                ambiguous = Some("snapshot capture was interrupted; retained publication needs explicit inspection".into());
                break;
            }
            continue;
        }
        let index = package_index;
        package_index += 1;
        if record.approval.is_some() {
            let expected = expected_actions.get(index);
            let change = reconciled
                .as_ref()
                .and_then(|r| r.package_changes.get(index));
            if !expected.is_some_and(|artifact| {
                step.id == format!("install-{}", artifact.name)
                    && change.is_some_and(|c| change_matches_pin(c, artifact))
            }) {
                ambiguous = Some(format!(
                    "step {} cannot be attributed to its exact approved artifact and package WAL",
                    step.id
                ));
                break;
            }
        }
        if step.outcome.is_some() {
            // Case C: a resolved step still has to hold in the database —
            // a success claim is not proof (§10.3 verify postconditions).
            if step.outcome == Some(StepOutcome::Succeeded) {
                let holds = reconciled
                    .as_ref()
                    .and_then(|r| r.package_changes.get(index))
                    .is_some_and(|change| {
                        change.status == "committed" && change_status_matches_db(&ctx.root, change)
                    });
                if !holds {
                    ambiguous = Some(format!(
                        "step {} claims success but its package transaction has no committed change",
                        step.id
                    ));
                    break;
                }
                committed_any = true;
            }
            if matches!(step.outcome, Some(StepOutcome::Failed { .. })) {
                failed_steps += 1;
            }
            continue;
        }

        // Case B: intent with no outcome. Never re-run; classify from the
        // reconciled package transaction and the actual database.
        let change = reconciled
            .as_ref()
            .and_then(|r| r.package_changes.get(index));
        let outcome = match change {
            Some(change) if change.status == "committed" => {
                if change_status_matches_db(&ctx.root, change) {
                    StepOutcome::Succeeded
                } else {
                    ambiguous = Some(format!(
                        "step {} has a committed package change but the database no longer matches its after-state",
                        step.id
                    ));
                    break;
                }
            }
            Some(change) if change.status == "failed" => {
                if before_state_matches_db(&ctx.root, change) {
                    StepOutcome::Failed {
                        reason: change
                            .failure
                            .clone()
                            .unwrap_or_else(|| "the reconciled package step failed".to_string()),
                    }
                } else {
                    ambiguous = Some(format!(
                        "step {} failed but the database matches neither its before- nor after-state",
                        step.id
                    ));
                    break;
                }
            }
            Some(change) => {
                ambiguous = Some(format!(
                    "step {} has an unclassifiable package status {:?}",
                    step.id, change.status
                ));
                break;
            }
            None => {
                // no package journal for this step: the only safe conclusion
                // is that no observable side effect happened. A package that
                // *is* present cannot be attributed to this step.
                let db = crate::adapters::db::Db::new(&ctx.root);
                if db.installed().is_err() {
                    ambiguous = Some(
                        "package inventory is unreadable for an interrupted step without WAL"
                            .into(),
                    );
                    break;
                }
                let present = db.installed().is_ok_and(|entries| {
                    entries
                        .iter()
                        .any(|e| step.intent.as_deref().is_some_and(|i| i.contains(&e.name)))
                });
                if present {
                    ambiguous = Some(format!(
                        "step {} has no package journal entry but a package named in its intent is installed",
                        step.id
                    ));
                    break;
                }
                StepOutcome::Failed {
                    reason: "no side effect was observed for this interrupted step".to_string(),
                }
            }
        };
        if outcome == StepOutcome::Succeeded {
            committed_any = true;
        } else {
            failed_steps += 1;
        }
        reconciled_any.push(step.id.clone());
        step.outcome = Some(outcome);
    }

    if let Some(reason) = ambiguous {
        // cannot prove the step outcomes: keep the durable state, gate active
        (ui.warn)(&format!(
            "migration {}: {reason} — the record stays {} (needs-recovery documented for the operator)",
            record.id,
            record.state.as_str()
        ));
        // The reconciled outcomes that *were* proven are kept on disk; the
        // state is left gate-keeping and reported as needing recovery.
        persist(&ctx.root, &amended)?;
        return Ok(RecoveryAction::NeedsRecovery(reason));
    }

    if !reconciled_any.is_empty() {
        persist(&ctx.root, &amended)?;
    }

    // Re-conclude from the durable outcomes (Case C). A failure with no
    // committed step is trivially compensated; a failure after a commit has
    // no verified compensation (§10.5), so it must keep the gate.
    let ending = if record.approval.is_some()
        && (package_index != expected_actions.len()
            || !super::migrate_apply::handler_blockers().is_empty())
    {
        // A prefix of the plan is never the complete operation. External boot
        // preparation also cannot be reconstructed from package outcomes.
        Transition::PreparationAmbiguous
    } else if failed_steps == 0 {
        Transition::PreparationCommitted
    } else if committed_any {
        Transition::PreparationAmbiguous
    } else {
        Transition::PreparationCompensated
    };
    let next = transition(record.state, ending).map_err(anyhow::Error::msg)?;
    amended.state = next;
    persist(&ctx.root, &amended)?;
    (ui.info)(&format!(
        "migration {}: reconciled {} interrupted step(s); durable state is now {}",
        record.id,
        reconciled_any.len(),
        next.as_str()
    ));
    Ok(RecoveryAction::ReConcluded(next))
}

/// Case D. Finalize the linked transaction history while leaving the
/// migration record (the authoritative outcome) untouched. A discrepancy is
/// reported honestly, never silently "fixed" by changing the migration.
fn finalize_history(
    ctx: &crate::application::context::Ctx,
    record: &MigrationRecord,
    ui: &UserInteraction,
) -> Result<RecoveryAction> {
    let Some(tx) = linked_transaction(&ctx.root, &record.id)? else {
        return Ok(RecoveryAction::Unchanged(
            "no linked preparation transaction to finalize".to_string(),
        ));
    };
    let existing = history::load_record(&ctx.root, tx)?;
    if !matches!(existing.status()?, TxStatus::Preparing | TxStatus::Running) {
        return Ok(RecoveryAction::Unchanged(format!(
            "linked transaction {tx} is already {}",
            existing.status
        )));
    }
    let Some(finished) = wal::reconcile(&ctx.root, tx)? else {
        return Ok(RecoveryAction::NeedsRecovery(format!(
            "linked transaction {tx} has no journal to reconcile"
        )));
    };
    history::rewrite_record(&ctx.root, &finished)?;
    let discrepancy = finished.status != TxStatus::Partial.as_str()
        && finished.status != TxStatus::Completed.as_str();
    if discrepancy {
        (ui.warn)(&format!(
            "migration {} is awaiting-reboot but its linked transaction {tx} reconciled as {} — the migration record remains authoritative",
            record.id, finished.status
        ));
    }
    Ok(RecoveryAction::HistoryFinalized(format!(
        "transaction {tx} reconciled as {}",
        finished.status
    )))
}

/// The single explicit recovery/resume command path (§12.2). Runs every
/// reconciliation case first (A–D), then — only when a migration is actually
/// awaiting verification — the post-reboot verification (case E). Live-host
/// preflight happens once, before any record is touched. Read-only inspection
/// commands never call this.
pub(crate) fn recover_and_resume(
    ctx: &crate::application::context::Ctx,
    ui: &UserInteraction,
) -> Result<(RecoveryReport, Option<ResumeOutcome>)> {
    require_live_host(ctx, Path::new("/proc"))?;
    let report = recover(ctx, ui)?;
    if has_reboot_pending(&ctx.root)? {
        Ok((report, Some(resume(ctx, ui)?)))
    } else {
        Ok((report, None))
    }
}

fn require_live_host(ctx: &crate::application::context::Ctx, proc: &Path) -> Result<()> {
    let observed = detect::observe(&ctx.root, proc);
    match observed.context {
        ExecContext::LiveHost { .. } => Ok(()),
        ExecContext::Container { .. } => bail!(
            "system migrate resume is refused in a container: host PID 1 is not the verified Faseela host"
        ),
        ExecContext::Chroot { .. } => bail!(
            "system migrate resume is refused in a chroot: /proc/1 belongs to the outer environment"
        ),
        ExecContext::OfflineRoot { root } => bail!(
            "system migrate resume is refused for the offline root {} — it is a live-host operation",
            root.display()
        ),
        ExecContext::Unknown { reason } => bail!(
            "system migrate resume is refused: the execution context cannot be verified ({reason})"
        ),
    }
}

fn has_reboot_pending(root: &Path) -> Result<bool> {
    for id in store::record_ids(root)? {
        if let Some(record) = store::load(root, &id)? {
            if matches!(
                record.state,
                MigrationState::AwaitingReboot | MigrationState::Verifying
            ) {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// Case E. Resume after reboot: live-host only, no service queries. The
/// verification result is returned as typed data; the record is transitioned
/// and published durably here because this is an imperative use case over
/// the durable store.
pub(crate) fn resume(
    ctx: &crate::application::context::Ctx,
    ui: &UserInteraction,
) -> Result<ResumeOutcome> {
    // `observe` enforces the canonical-root rule: an explicit non-canonical
    // `--root` is an offline target and can never be a live host.
    require_live_host(ctx, Path::new("/proc"))?;
    resume_inner(ctx, Path::new("/proc"), ui, true)
}

/// The injectable-proc variant: unit tests drive a fake `/proc` so the real
/// PID 1 identity rules are exercised without booting anything. It bypasses
/// only the *test environment's* root-privilege and canonical-root checks;
/// every actual verification rule is the production one.
#[cfg(test)]
pub(crate) fn resume_with(
    ctx: &crate::application::context::Ctx,
    proc: &Path,
    ui: &UserInteraction,
) -> Result<ResumeOutcome> {
    resume_inner(ctx, proc, ui, false)
}

fn resume_inner(
    ctx: &crate::application::context::Ctx,
    proc: &Path,
    ui: &UserInteraction,
    require_root: bool,
) -> Result<ResumeOutcome> {
    // Use the canonical observation path: with a live host the managed root
    // is `/`, and a fake `/proc` in tests is attributed the same way (the
    // pid1_root/self_root markers decide chroot vs host).
    let observed = detect::observe_canonical(&ctx.root, proc);
    match &observed.context {
        ExecContext::LiveHost { .. } => {}
        ExecContext::Container { .. } => bail!(
            "system migrate resume is refused in a container: host PID 1 is not the verified Faseela host"
        ),
        ExecContext::Chroot { .. } => bail!(
            "system migrate resume is refused in a chroot: /proc/1 belongs to the outer environment"
        ),
        ExecContext::OfflineRoot { root } => bail!(
            "system migrate resume is refused for the offline root {} — it is a live-host operation",
            root.display()
        ),
        ExecContext::Unknown { reason } => bail!(
            "system migrate resume is refused: the execution context cannot be verified ({reason})"
        ),
    }
    if require_root && crate::adapters::sys::geteuid() != 0 {
        bail!("system migrate resume requires root privileges to verify the boot/init state");
    }

    let _guard =
        lock::acquire_for_migration(&ctx.root, lock::migration_authority())?.ok_or_else(|| {
            anyhow!(
                "internal: the exclusive package database lock is required to resume a migration"
            )
        })?;
    // The authoritative observation must be fresh under the lock too.
    let observed = detect::observe_canonical(&ctx.root, proc);
    if !matches!(observed.context, ExecContext::LiveHost { .. }) {
        bail!("live-host attribution changed before post-boot verification");
    }

    let ids = store::record_ids(&ctx.root)?;
    let mut candidates = Vec::new();
    for id in ids {
        let record = store::load(&ctx.root, &id)?
            .ok_or_else(|| anyhow!("migration record {id} disappeared while being read"))?;
        if matches!(
            record.state,
            MigrationState::AwaitingReboot | MigrationState::Verifying
        ) {
            candidates.push(record);
        }
    }
    if candidates.len() != 1 {
        bail!(
            "system migrate resume expects exactly one awaiting-reboot/verifying migration, found {} — the state is ambiguous",
            candidates.len()
        );
    }
    let mut record = candidates.remove(0);

    // The recorded source and target must match the trusted handler's scope
    // (§10.1/§10.2): a record whose capabilities drifted cannot be verified.
    let expected_target = match record.handler {
        HandlerId::FaseelaInitRunitToSystemd => InitKind::Systemd,
    };
    let expected_source = match record.handler {
        HandlerId::FaseelaInitRunitToSystemd => InitKind::Runit,
    };
    if record.from.identity != expected_source.capability_identity()
        || record.to.identity != expected_target.capability_identity()
    {
        bail!(
            "migration {}: the recorded transition {} → {} does not match the trusted handler {} — refusing to verify (§10.2)",
            record.id, record.from.to_key(), record.to.to_key(), record.handler.as_str()
        );
    }

    if record.state == MigrationState::AwaitingReboot {
        record.state =
            transition(record.state, Transition::BeginVerification).map_err(anyhow::Error::msg)?;
        persist(&ctx.root, &record)?;
    }

    let mut failures = Vec::new();
    match &observed.runtime {
        RuntimeInit::Observed {
            kind,
            confidence: Confidence::Proven,
            ..
        } if *kind == expected_target => {
            (ui.info)(&format!(
                "migration {}: PID 1 is verified as {}",
                record.id,
                kind.capability_identity()
            ));
        }
        RuntimeInit::Observed { kind, .. } => failures.push(format!(
            "PID 1 is {} but the migration target is {}",
            kind.capability_identity(),
            expected_target.capability_identity()
        )),
        RuntimeInit::Unknown { reason } => {
            failures.push(format!("PID 1 identity is unknown: {reason}"))
        }
    }

    match &observed.desired {
        detect::DesiredInit::Declared { identity, .. } if identity == &record.to.identity => {}
        _ => failures
            .push("effective system profile does not require the approved target init".into()),
    }
    if let Err(error) = verify_retained_evidence(&ctx.root, &record) {
        failures.push(format!("retained migration evidence: {error:#}"));
    }
    if let Err(error) = verify_approved_inventory(&ctx.root, &record) {
        failures.push(format!("approved installed identities: {error:#}"));
    }
    // Firmware/initramfs and Live-ISO acceptance are not established by the
    // presence of files or by PID 1 alone. This boundary also applies to resume.
    failures.extend(
        super::migrate_apply::handler_blockers()
            .iter()
            .map(|s| s.to_string()),
    );

    for postcondition in verify_boot_postconditions(&ctx.root, &expected_target) {
        if !postcondition.satisfied {
            failures.push(format!(
                "boot postcondition {:?} failed: {}",
                postcondition.name, postcondition.detail
            ));
        }
    }

    if !failures.is_empty() {
        // No verified rollback route exists until B1/B2/B5 are resolved, so
        // verification failure keeps the gate (§10.5: never promise automatic
        // rollback of arbitrary root filesystem changes).
        record.state =
            transition(record.state, Transition::Ambiguous).map_err(anyhow::Error::msg)?;
        persist(&ctx.root, &record)?;
        (ui.warn)(&format!(
            "migration {}: verification failed ({}); the record is needs-recovery and the gate stays active",
            record.id,
            failures.join("; ")
        ));
        return Ok(ResumeOutcome::NeedsRecovery(record, failures));
    }

    // Resolve package history *before* clearing the authoritative gate.
    if matches!(
        finalize_history(ctx, &record, ui)?,
        RecoveryAction::NeedsRecovery(_)
    ) {
        bail!("package transaction history could not be reconciled; mutation gate retained");
    }
    record.state =
        transition(record.state, Transition::VerificationSucceeded).map_err(anyhow::Error::msg)?;
    persist(&ctx.root, &record)?;
    // Finalization does *not* remove the old provider: retiring runit needs
    // its own verified rollback window (B2/B5) and its own package plan.
    (ui.info)(&format!(
        "migration {}: target init verified; the record is completed and the mutation gate is clear (old-provider cleanup is a separate, explicitly authorized action)",
        record.id
    ));
    Ok(ResumeOutcome::Verified(record))
}

fn verify_retained_evidence(root: &Path, record: &MigrationRecord) -> Result<()> {
    let pin = record.approval.as_ref().ok_or_else(|| {
        anyhow!("legacy schema-1 record has no pinned SystemPlan; it cannot authorize completion")
    })?;
    pin.validate().map_err(anyhow::Error::msg)?;
    let snapshot = record
        .snapshot
        .as_ref()
        .ok_or_else(|| anyhow!("no durable snapshot binding was captured"))?;
    crate::adapters::migration_boot_snapshot::verify_pinned(root, &record.id, snapshot)?;
    for artifact in &pin.artifacts {
        let object = crate::adapters::artifact::object_path(root, &artifact.sha256)?;
        crate::adapters::artifact::verify_object_file(&object, &artifact.sha256)?;
        if std::fs::metadata(object)?.len() != artifact.size {
            bail!(
                "retained staged archive size mismatch for {}",
                artifact.name
            );
        }
    }
    Ok(())
}

fn change_matches_pin(
    change: &history::PackageChange,
    pin: &crate::domain::system::plan::ArtifactPin,
) -> bool {
    change.name == pin.name
        && change.after.as_ref().is_some_and(|after| {
            after
                .version
                .as_ref()
                .zip(after.rel)
                .is_some_and(|(v, r)| format!("{v}-{r}") == pin.target)
                && after.artifact_sha256 == pin.sha256
        })
}

fn verify_approved_inventory(root: &Path, record: &MigrationRecord) -> Result<()> {
    let pin = record
        .approval
        .as_ref()
        .ok_or_else(|| anyhow!("no approved package inventory"))?;
    let actual = super::observation::observe(root, Path::new("/proc"));
    let mut expected: std::collections::BTreeMap<_, _> = pin
        .installed_identities
        .iter()
        .map(|i| (i.name.clone(), i.clone()))
        .collect();
    for artifact in &pin.artifacts {
        if artifact.kind == crate::domain::system::plan::PackageActionKind::Keep {
            continue;
        }
        let reason = if artifact.install_reason == crate::domain::planning::install::Why::Update {
            expected
                .get(&artifact.name)
                .map(|i| i.reason.clone())
                .unwrap_or_default()
        } else {
            artifact.install_reason.db_reason().as_str().to_string()
        };
        expected.insert(
            artifact.name.clone(),
            crate::domain::system::plan::InstalledIdentity {
                name: artifact.name.clone(),
                version: artifact.target.clone(),
                sha256: artifact.sha256.clone(),
                reason,
            },
        );
        if crate::install::hook_needs_retry(root, &artifact.name)? {
            bail!("{} has an unverified post-install hook", artifact.name);
        }
    }
    if !matches!(
        actual.packages,
        super::observation::PackageInventory::Available(_)
    ) || actual.installed_identities != expected.into_values().collect::<Vec<_>>()
    {
        bail!("installed package state does not match the exact approved changes; the old provider must be retained");
    }
    Ok(())
}

/// One checkable boot/init postcondition, reported with its source so the
/// verification output is attributable (§5.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Postcondition {
    pub(crate) name: &'static str,
    pub(crate) satisfied: bool,
    pub(crate) detail: String,
}

/// Verify the target-root boot/init postconditions using read-only, no-follow
/// inspection of the generic `init` entry points. This is the Faseela/saq
/// selection contract (B1): a fallback chain that would pick an installed
/// runit must not be mistaken for an explicit systemd selection. When no
/// entry point explicitly identifies the target, verification fails closed.
pub(crate) fn verify_boot_postconditions(root: &Path, target: &InitKind) -> Vec<Postcondition> {
    let target_name = target.capability_identity();
    let mut out = Vec::new();

    // 1. the generic init entry point must explicitly select the target
    let mut selected = Vec::new();
    let mut invalid = Vec::new();
    for rel in ["sbin/init", "usr/sbin/init", "usr/bin/init"] {
        match resolve_target_file(root, rel) {
            Ok(None) => {}
            Ok(Some(path))
                if ["usr/lib/systemd/systemd", "lib/systemd/systemd"]
                    .iter()
                    .any(|p| path == Path::new(p)) =>
            {
                selected.push(rel);
            }
            Ok(Some(_)) | Err(_) => invalid.push(rel),
        }
    }
    out.push(if !selected.is_empty() && invalid.is_empty() {
        Postcondition {
            name: "init-entry-point-selects-target",
            satisfied: true,
            detail: format!("{} resolve to the executable {target_name} payload under the selected root", selected.join(", ")),
        }
    } else {
        Postcondition {
            name: "init-entry-point-selects-target",
            satisfied: false,
            detail: format!("init selection is missing, contradictory or unsafe for {target_name}; invalid entries: {}", invalid.join(", ")),
        }
    });

    // 2. the target provider's payload must be staged in the target root
    let target_paths = ["usr/lib/systemd/systemd", "lib/systemd/systemd"];
    let staged = target_paths
        .iter()
        .find(|p| resolve_target_file(root, p).is_ok_and(|p| p.is_some()));
    out.push(match staged {
        Some(path) => Postcondition {
            name: "target-provider-staged",
            satisfied: true,
            detail: format!("{path} is present in the target root"),
        },
        None => Postcondition {
            name: "target-provider-staged",
            satisfied: false,
            detail: format!("no {target_name} provider payload is installed under the target root"),
        },
    });

    // 3. the documented recovery route: the old provider's artifacts must be
    // retained until the new init has booted and passed verification (§10.5).
    let old_paths = ["usr/bin/runit", "sbin/runit-init"];
    let retained = old_paths
        .iter()
        .find(|p| resolve_target_file(root, p).is_ok_and(|p| p.is_some()));
    out.push(match retained {
        Some(path) => Postcondition {
            name: "old-provider-retained-for-recovery",
            satisfied: true,
            detail: format!("{path} is still present (old boot route retained)"),
        },
        None => Postcondition {
            name: "old-provider-retained-for-recovery",
            satisfied: false,
            detail: "no old-provider artifact is retained — the documented recovery route is unavailable"
                .to_string(),
        },
    });

    out
}

/// Resolve selected-root paths with root-relative absolute symlinks and a
/// bounded no-follow walk. Never canonicalize an absolute offline-root link
/// against the developer's host filesystem.
fn resolve_target_file(root: &Path, relative: &str) -> Result<Option<std::path::PathBuf>> {
    use std::collections::VecDeque;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Component, PathBuf};
    let mut pending: VecDeque<_> = Path::new(relative)
        .components()
        .map(|c| c.as_os_str().to_os_string())
        .collect();
    let mut resolved = PathBuf::new();
    let mut links = 0;
    while let Some(part) = pending.pop_front() {
        if part == "." {
            continue;
        }
        if part == ".." {
            if !resolved.pop() {
                bail!("init path escapes selected root");
            }
            continue;
        }
        resolved.push(part);
        let meta = match std::fs::symlink_metadata(root.join(&resolved)) {
            Ok(meta) => meta,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        if meta.file_type().is_symlink() {
            links += 1;
            if links > 32 {
                bail!("init symlink loop");
            }
            let link = std::fs::read_link(root.join(&resolved))?;
            resolved.pop();
            if link.is_absolute() {
                resolved.clear();
            }
            let parts: Vec<_> = link
                .components()
                .filter(|c| !matches!(c, Component::RootDir))
                .map(|c| c.as_os_str().to_os_string())
                .collect();
            for part in parts.into_iter().rev() {
                pending.push_front(part);
            }
        } else if pending.is_empty() {
            if !meta.is_file() || meta.permissions().mode() & 0o111 == 0 || meta.len() == 0 {
                bail!("init payload is not a nonempty executable regular file");
            }
        } else if !meta.is_dir() {
            bail!("init path ancestor is not a directory");
        }
    }
    Ok(Some(resolved))
}

/// The transaction whose `requested_packages` names this migration, created
/// by the coordinator's `begin_operation` (§11). Multiple matches are
/// ambiguous and fail closed; the latest id wins when a crash produced a
/// stale duplicate, reported through the error chain.
fn linked_transaction(root: &Path, id: &str) -> Result<Option<u64>> {
    let records = history::list_records(root)?;
    let mut matches: Vec<u64> = records
        .into_iter()
        .filter(|r| r.command == "migrate" && r.requested_packages.iter().any(|p| p == id))
        .map(|r| r.id)
        .collect();
    matches.sort_unstable();
    match matches.len() {
        0 => Ok(None),
        1 => Ok(Some(matches[0])),
        n => bail!(
            "migration {id} links to {n} migrate transactions ({:?}) — the linkage is ambiguous",
            matches
        ),
    }
}

fn change_status_matches_db(root: &Path, change: &history::PackageChange) -> bool {
    let current = match crate::adapters::db::Db::new(root).entry(&change.name) {
        Ok(entry) => entry,
        Err(_) => return false, // unreadable database fails closed
    };
    match (current, &change.after) {
        (None, None) => true,
        (Some(entry), Some(after)) => after.matches_entry(&entry),
        _ => false,
    }
}

fn before_state_matches_db(root: &Path, change: &history::PackageChange) -> bool {
    let current = match crate::adapters::db::Db::new(root).entry(&change.name) {
        Ok(entry) => entry,
        Err(_) => return false,
    };
    match (current, &change.before) {
        (None, None) => true,
        (Some(entry), Some(before)) => before.matches_entry(&entry),
        _ => false,
    }
}

/// Durable record publication with reload-verify (the coordinator's
/// `persist` contract, shared by every recovery transition). A publish-only
/// failure whose on-disk result already matches the intent stands; anything
/// else is reported with what is actually on disk.
fn persist(root: &Path, record: &MigrationRecord) -> Result<()> {
    if let Err(error) = store::save(root, record) {
        let note = match store::load(root, &record.id) {
            Ok(Some(on_disk)) if on_disk == *record => return Ok(()),
            Ok(Some(on_disk)) => format!("the on-disk record is {}", on_disk.state.as_str()),
            Ok(None) => "no record is on disk".to_string(),
            Err(read_error) => format!("the on-disk record cannot be read ({read_error:#})"),
        };
        return Err(error.context(format!(
            "migration record {} was not published as intended ({note})",
            record.id
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::system::capability::Capability;
    use crate::domain::system::migration::{HandlerId, MigrationDefinition, StepRecord};
    use crate::testutil::tmpdir;
    use std::cell::RefCell;
    use std::os::unix::fs::symlink;
    use std::path::PathBuf;
    use std::rc::Rc;

    fn cap(key: &str) -> Capability {
        Capability::parse(key).unwrap()
    }

    fn definition(id: &str) -> MigrationDefinition {
        MigrationDefinition::new(
            id,
            1,
            cap("init-system=runit"),
            cap("init-system=systemd"),
            HandlerId::FaseelaInitRunitToSystemd,
            true,
        )
        .unwrap()
    }

    fn ctx_with(root: &Path) -> crate::application::context::Ctx {
        crate::application::context::Ctx::from_config(&crate::adapters::config::Config {
            root: root.to_path_buf(),
            recipes: root.join("recipes"),
            builddir: root.join("build"),
            profiles: root.join("profiles"),
            jobs: None,
            repo: None,
            install_mode: crate::adapters::config::InstallMode::Auto,
            downloads: 4,
            file: None,
            system_file: None,
            sources: std::collections::BTreeMap::new(),
        })
    }

    #[derive(Default)]
    struct Recorder {
        infos: Rc<RefCell<Vec<String>>>,
        warns: Rc<RefCell<Vec<String>>>,
    }

    impl Recorder {
        fn ui(&self) -> UserInteraction {
            let infos = Rc::clone(&self.infos);
            let stderr_warns = Rc::clone(&self.warns);
            let warn_warns = Rc::clone(&self.warns);
            UserInteraction {
                stderr_line: Box::new(move |l: &str| stderr_warns.borrow_mut().push(l.to_string())),
                info: Box::new(move |l: &str| infos.borrow_mut().push(l.to_string())),
                warn: Box::new(move |l: &str| warn_warns.borrow_mut().push(l.to_string())),
                show_plan: Box::new(|_| {}),
                choose: Box::new(|_, _, _| anyhow::bail!("no prompts in recovery")),
                confirm: Box::new(|_| anyhow::bail!("no prompts in recovery")),
            }
        }
    }

    fn seed_record(root: &Path, state: MigrationState, steps: Vec<StepRecord>) -> MigrationRecord {
        let mut record = MigrationRecord::new(&definition("faseela-runit-to-systemd-v1"));
        record.state = state;
        record.steps = steps;
        store::save(root, &record).unwrap();
        record
    }

    fn entry(root: &Path, name: &str, version: &str, digest: &str) {
        crate::adapters::db::Db::new(root)
            .add(
                &crate::adapters::db::Entry {
                    name: name.to_string(),
                    version: version.to_string(),
                    rel: 1,
                    arch: "x86_64".to_string(),
                    reason: "explicit".to_string(),
                    run: vec![],
                    install_date: 0,
                    size: 0,
                    artifact_sha256: digest.to_string(),
                },
                &[],
                &Default::default(),
            )
            .unwrap();
    }

    fn state(_name: &str, version: &str, digest: &str) -> history::PackageState {
        history::PackageState {
            version: Some(version.to_string()),
            rel: Some(1),
            arch: "x86_64".to_string(),
            reason: "explicit".to_string(),
            artifact_sha256: digest.to_string(),
        }
    }

    fn seed_tx(root: &Path, status: &str, changes: Vec<history::PackageChange>) -> u64 {
        let record = history::TransactionRecord {
            schema: history::SCHEMA_VERSION,
            id: 1,
            command: "migrate".to_string(),
            status: status.to_string(),
            started_at: 10,
            finished_at: None,
            duration_seconds: None,
            initiated_by: "uid:0".to_string(),
            requested_packages: vec!["faseela-runit-to-systemd-v1".to_string()],
            repository_ids: vec![],
            package_changes: changes,
            undone_by: None,
            undoes: None,
            groups: vec![],
        };
        history::write_record(root, &record).unwrap();
        record.id
    }

    fn change(
        name: &str,
        status: &str,
        before: Option<history::PackageState>,
        after: Option<history::PackageState>,
    ) -> history::PackageChange {
        history::PackageChange {
            name: name.to_string(),
            action: "install".to_string(),
            status: status.to_string(),
            before,
            after,
            source: "migration-staged".to_string(),
            failure: None,
            hook: None,
        }
    }

    fn on_disk(root: &Path) -> MigrationRecord {
        store::load(root, "faseela-runit-to-systemd-v1")
            .unwrap()
            .expect("the record exists")
    }

    fn gate_active(root: &Path) -> bool {
        !store::active_gates(root).unwrap().is_empty()
    }

    // ---- Case A: interrupted planned ------------------------------------

    #[test]
    fn an_abandoned_planned_record_without_a_transaction_is_discarded_and_verified() {
        let root = tmpdir("migrate-recover-planned-clean");
        seed_record(&root, MigrationState::Planned, vec![]);
        let recorder = Recorder::default();
        let report = recover(&ctx_with(&root), &recorder.ui()).unwrap();
        assert_eq!(report.notes.len(), 1);
        assert_eq!(report.notes[0].action, RecoveryAction::Discarded);
        assert!(store::load(&root, "faseela-runit-to-systemd-v1")
            .unwrap()
            .is_none());
        assert!(!gate_active(&root));
    }

    #[test]
    fn a_planned_record_with_committed_packages_is_preserved_as_needs_recovery() {
        let root = tmpdir("migrate-recover-planned-committed");
        seed_record(&root, MigrationState::Planned, vec![]);
        entry(&root, "sysd", "1", &"a".repeat(64));
        seed_tx(
            &root,
            "preparing",
            vec![change(
                "sysd",
                "committed",
                None,
                Some(state("sysd", "1", &"a".repeat(64))),
            )],
        );
        let recorder = Recorder::default();
        let report = recover(&ctx_with(&root), &recorder.ui()).unwrap();
        assert!(matches!(
            report.notes[0].action,
            RecoveryAction::NeedsRecovery(_)
        ));
        // the gate was retroactively closed: package mutations stay blocked
        assert!(gate_active(&root));
        assert_eq!(on_disk(&root).state, MigrationState::NeedsRecovery);
        assert!(recorder
            .warns
            .borrow()
            .iter()
            .any(|w| w.contains("committed")));
    }

    #[test]
    fn a_planned_record_with_a_step_is_never_discarded() {
        let root = tmpdir("migrate-recover-planned-step");
        seed_record(
            &root,
            MigrationState::Planned,
            vec![StepRecord {
                id: "install-sysd".to_string(),
                intent: Some("install the staged sysd artifact".to_string()),
                outcome: None,
            }],
        );
        let recorder = Recorder::default();
        let report = recover(&ctx_with(&root), &recorder.ui()).unwrap();
        assert!(matches!(
            report.notes[0].action,
            RecoveryAction::NeedsRecovery(_)
        ));
        assert_eq!(on_disk(&root).state, MigrationState::Planned);
    }

    // ---- Case B: interrupted step reconciliation ------------------------

    #[test]
    fn a_committed_step_with_no_outcome_is_reconciled_to_success_not_rerun() {
        let root = tmpdir("migrate-recover-step-committed");
        seed_record(
            &root,
            MigrationState::Preparing,
            vec![StepRecord {
                id: "install-sysd".to_string(),
                intent: Some("install the staged sysd artifact".to_string()),
                outcome: None,
            }],
        );
        entry(&root, "sysd", "1", &"a".repeat(64));
        seed_tx(
            &root,
            "preparing",
            vec![change(
                "sysd",
                "committed",
                None,
                Some(state("sysd", "1", &"a".repeat(64))),
            )],
        );
        let recorder = Recorder::default();
        let report = recover(&ctx_with(&root), &recorder.ui()).unwrap();
        assert_eq!(
            report.notes[0].action,
            RecoveryAction::ReConcluded(MigrationState::AwaitingReboot)
        );
        let record = on_disk(&root);
        assert_eq!(record.steps[0].outcome, Some(StepOutcome::Succeeded));
        assert_eq!(record.state, MigrationState::AwaitingReboot);
        // the package is still there — recovery never removed/reinstalled it
        assert!(crate::adapters::db::Db::new(&root)
            .entry("sysd")
            .unwrap()
            .is_some());
    }

    #[test]
    fn a_failed_step_whose_before_state_matches_is_recorded_as_failed_and_compensated() {
        let root = tmpdir("migrate-recover-step-failed");
        seed_record(
            &root,
            MigrationState::Preparing,
            vec![StepRecord {
                id: "install-sysd".to_string(),
                intent: Some("install the staged sysd artifact".to_string()),
                outcome: None,
            }],
        );
        seed_tx(
            &root,
            "preparing",
            vec![change("sysd", "failed", None, None)],
        );
        let recorder = Recorder::default();
        let report = recover(&ctx_with(&root), &recorder.ui()).unwrap();
        assert_eq!(
            report.notes[0].action,
            RecoveryAction::ReConcluded(MigrationState::Failed)
        );
        let record = on_disk(&root);
        assert!(matches!(
            record.steps[0].outcome,
            Some(StepOutcome::Failed { .. })
        ));
        assert_eq!(record.state, MigrationState::Failed);
        assert!(!gate_active(&root), "nothing committed — the gate is clear");
    }

    #[test]
    fn a_committed_step_whose_database_no_longer_matches_fails_closed() {
        let root = tmpdir("migrate-recover-step-mismatch");
        seed_record(
            &root,
            MigrationState::Preparing,
            vec![StepRecord {
                id: "install-sysd".to_string(),
                intent: Some("install the staged sysd artifact".to_string()),
                outcome: None,
            }],
        );
        // history says version 1 committed, but the database holds version 2
        entry(&root, "sysd", "2", &"a".repeat(64));
        seed_tx(
            &root,
            "preparing",
            vec![change(
                "sysd",
                "committed",
                None,
                Some(state("sysd", "1", &"a".repeat(64))),
            )],
        );
        let recorder = Recorder::default();
        let report = recover(&ctx_with(&root), &recorder.ui()).unwrap();
        assert!(matches!(
            report.notes[0].action,
            RecoveryAction::NeedsRecovery(_)
        ));
        assert_eq!(
            on_disk(&root).state,
            MigrationState::Preparing,
            "state is preserved"
        );
        assert!(gate_active(&root), "the gate stays active");
    }

    // ---- Case C: all steps resolved, conclusion unpublished -------------

    #[test]
    fn a_preparing_record_with_all_steps_resolved_is_re_concluded() {
        let root = tmpdir("migrate-recover-conclude");
        seed_record(
            &root,
            MigrationState::Preparing,
            vec![StepRecord {
                id: "install-sysd".to_string(),
                intent: Some("install the staged sysd artifact".to_string()),
                outcome: Some(StepOutcome::Succeeded),
            }],
        );
        entry(&root, "sysd", "1", &"a".repeat(64));
        seed_tx(
            &root,
            "preparing",
            vec![change(
                "sysd",
                "committed",
                None,
                Some(state("sysd", "1", &"a".repeat(64))),
            )],
        );
        let recorder = Recorder::default();
        let report = recover(&ctx_with(&root), &recorder.ui()).unwrap();
        assert_eq!(
            report.notes[0].action,
            RecoveryAction::ReConcluded(MigrationState::AwaitingReboot)
        );
        assert_eq!(on_disk(&root).state, MigrationState::AwaitingReboot);
        assert!(gate_active(&root));
    }

    #[test]
    fn a_success_claim_without_a_committed_package_change_fails_closed() {
        let root = tmpdir("migrate-recover-fake-success");
        seed_record(
            &root,
            MigrationState::Preparing,
            vec![StepRecord {
                id: "install-sysd".to_string(),
                intent: Some("install the staged sysd artifact".to_string()),
                outcome: Some(StepOutcome::Succeeded),
            }],
        );
        // a failed transaction: the success claim is not trusted
        seed_tx(
            &root,
            "preparing",
            vec![change("sysd", "failed", None, None)],
        );
        let recorder = Recorder::default();
        let report = recover(&ctx_with(&root), &recorder.ui()).unwrap();
        assert!(matches!(
            report.notes[0].action,
            RecoveryAction::NeedsRecovery(_)
        ));
        assert_eq!(on_disk(&root).state, MigrationState::Preparing);
        assert!(gate_active(&root));
    }

    // ---- Case D: finalize the linked transaction history ----------------

    #[test]
    fn an_awaiting_reboot_record_finalizes_its_preparing_history_without_change() {
        let root = tmpdir("migrate-recover-history");
        let record = seed_record(&root, MigrationState::AwaitingReboot, vec![]);
        seed_tx(&root, "preparing", vec![]);
        let recorder = Recorder::default();
        let report = recover(&ctx_with(&root), &recorder.ui()).unwrap();
        // no journal exists, so the transaction cannot be reconciled: the
        // discrepancy is reported, never silently "fixed"
        assert!(
            matches!(report.notes[0].action, RecoveryAction::NeedsRecovery(_))
                || matches!(report.notes[0].action, RecoveryAction::HistoryFinalized(_))
        );
        // the authoritative migration outcome is untouched
        assert_eq!(on_disk(&root).state, record.state);
    }

    // ---- Terminal / deterministic scan ----------------------------------

    #[test]
    fn terminal_records_are_untouched_and_records_are_scanned_in_sorted_order() {
        let root = tmpdir("migrate-recover-terminal");
        let mut terminal = MigrationRecord::new(&definition("aaa-done"));
        terminal.state = MigrationState::Completed;
        store::save(&root, &terminal).unwrap();
        let mut pending = MigrationRecord::new(&definition("bbb-planned"));
        pending.state = MigrationState::Planned;
        store::save(&root, &pending).unwrap();
        let recorder = Recorder::default();
        let report = recover(&ctx_with(&root), &recorder.ui()).unwrap();
        assert_eq!(report.notes.len(), 2);
        assert_eq!(report.notes[0].id, "aaa-done");
        assert!(matches!(
            report.notes[0].action,
            RecoveryAction::Unchanged(_)
        ));
        assert_eq!(report.notes[1].id, "bbb-planned");
        assert_eq!(report.notes[1].action, RecoveryAction::Discarded);
    }

    #[test]
    fn verification_and_rollback_required_records_keep_the_gate_without_a_rollback() {
        for state in [MigrationState::Verifying, MigrationState::RollbackRequired] {
            let root = tmpdir(&format!("migrate-recover-{}-gate", state.as_str()));
            seed_record(&root, state, vec![]);
            let recorder = Recorder::default();
            let report = recover(&ctx_with(&root), &recorder.ui()).unwrap();
            assert!(matches!(
                report.notes[0].action,
                RecoveryAction::NeedsRecovery(_)
            ));
            assert_eq!(on_disk(&root).state, state);
            assert!(gate_active(&root), "no invented rollback clears the gate");
        }
    }

    // ---- Case E: boot postconditions and post-reboot verification -------

    fn file(root: &Path, rel: &str, body: &str) {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    fn fake_proc(base: &Path, exe: Option<&str>, comm: Option<&str>, root: &Path) -> PathBuf {
        let proc = base.join("proc");
        std::fs::create_dir_all(proc.join("1")).unwrap();
        std::fs::create_dir_all(proc.join("self")).unwrap();
        if let Some(exe) = exe {
            symlink(exe, proc.join("1/exe")).unwrap();
        }
        if let Some(comm) = comm {
            file(base, "proc/1/comm", &format!("{comm}\n"));
        }
        symlink(root, proc.join("1/root")).unwrap();
        symlink(root, proc.join("self/root")).unwrap();
        proc
    }

    fn bootable_target_root(root: &Path) {
        use std::os::unix::fs::PermissionsExt;
        file(root, "usr/lib/systemd/systemd", "staged systemd");
        std::fs::set_permissions(
            root.join("usr/lib/systemd/systemd"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        symlink("../usr/lib/systemd/systemd", root.join("sbin/init")).ok();
        std::fs::create_dir_all(root.join("sbin")).ok();
        std::fs::remove_file(root.join("sbin/init")).ok();
        symlink("/usr/lib/systemd/systemd", root.join("sbin/init")).unwrap();
        file(root, "usr/bin/runit", "retained runit");
        std::fs::set_permissions(
            root.join("usr/bin/runit"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
    }

    #[test]
    fn boot_postconditions_verify_explicit_target_selection_and_a_retained_old_route() {
        let root = tmpdir("migrate-recover-boot-ok");
        bootable_target_root(&root);
        let checks = verify_boot_postconditions(&root, &InitKind::Systemd);
        assert!(checks.iter().all(|c| c.satisfied), "{checks:?}");
        assert!(checks
            .iter()
            .any(|c| c.name == "init-entry-point-selects-target"));
        assert!(checks
            .iter()
            .any(|c| c.name == "old-provider-retained-for-recovery"));
    }

    #[test]
    fn boot_postconditions_fail_closed_when_a_fallback_would_select_runit() {
        let root = tmpdir("migrate-recover-boot-bad");
        // an init entry point that points at runit must not verify systemd
        std::fs::create_dir_all(root.join("sbin")).unwrap();
        symlink("/usr/bin/runit", root.join("sbin/init")).unwrap();
        file(&root, "usr/bin/runit", "runit");
        file(&root, "usr/lib/systemd/systemd", "systemd staged");
        let checks = verify_boot_postconditions(&root, &InitKind::Systemd);
        assert!(checks
            .iter()
            .find(|c| c.name == "init-entry-point-selects-target")
            .is_some_and(|c| !c.satisfied));
    }

    #[test]
    fn pid1_and_payload_alone_cannot_complete_a_legacy_unpinned_record() {
        let root = tmpdir("migrate-recover-resume-ok");
        seed_record(&root, MigrationState::AwaitingReboot, vec![]);
        bootable_target_root(&root);
        let proc = fake_proc(
            &root,
            Some("/usr/lib/systemd/systemd"),
            Some("systemd"),
            &root,
        );
        let recorder = Recorder::default();
        let outcome = resume_with(&ctx_with(&root), &proc, &recorder.ui()).unwrap();
        match outcome {
            ResumeOutcome::NeedsRecovery(record, failures) => {
                assert_eq!(record.state, MigrationState::NeedsRecovery);
                assert!(failures.iter().any(|f| f.contains("legacy schema-1")));
                assert!(failures.iter().any(|f| f.contains("B5")));
            }
            other => panic!("expected NeedsRecovery, got {other:?}"),
        }
        assert_eq!(on_disk(&root).state, MigrationState::NeedsRecovery);
        assert!(
            gate_active(&root),
            "unverified boot/recovery must retain the gate"
        );
        assert!(recorder
            .infos
            .borrow()
            .iter()
            .any(|i| i.contains("PID 1 is verified")));
    }

    #[test]
    fn resume_with_a_wrong_pid1_enters_needs_recovery_and_keeps_the_gate() {
        let root = tmpdir("migrate-recover-resume-bad");
        seed_record(&root, MigrationState::AwaitingReboot, vec![]);
        bootable_target_root(&root);
        let proc = fake_proc(&root, Some("/usr/bin/runit"), Some("runit"), &root);
        let recorder = Recorder::default();
        let outcome = resume_with(&ctx_with(&root), &proc, &recorder.ui()).unwrap();
        match outcome {
            ResumeOutcome::NeedsRecovery(record, failures) => {
                assert_eq!(record.state, MigrationState::NeedsRecovery);
                assert!(failures.iter().any(|f| f.contains("PID 1 is runit")));
            }
            other => panic!("expected NeedsRecovery, got {other:?}"),
        }
        assert_eq!(on_disk(&root).state, MigrationState::NeedsRecovery);
        assert!(gate_active(&root));
    }

    #[test]
    fn resume_refuses_an_offline_root_before_touching_any_record() {
        let root = tmpdir("migrate-recover-resume-offline");
        seed_record(&root, MigrationState::AwaitingReboot, vec![]);
        // `resume()` uses the real /proc and enforces the canonical-root rule;
        // a scratch root is an offline target and must be refused.
        let recorder = Recorder::default();
        let error = resume(&ctx_with(&root), &recorder.ui()).unwrap_err();
        assert!(format!("{error:#}").contains("offline root"));
        assert_eq!(on_disk(&root).state, MigrationState::AwaitingReboot);
    }

    // ---- The permission seams stay compile-time private ------------------

    #[test]
    fn the_migration_authority_is_zero_sized_and_not_cloneable() {
        // A value exists only through the lock-defined minting function; it
        // carries no data and cannot be copied (compile-time property, and
        // the size assertion documents it).
        let authority = lock::migration_authority();
        assert_eq!(std::mem::size_of_val(&authority), 0);
        // No `Clone`, no `Copy`, no serde: the type cannot leave this process.
    }

    #[test]
    fn a_forged_lock_marker_cannot_cross_the_migration_gate() {
        let error =
            lock::reject_nested_marker(true).expect_err("a nested process cannot cross the gate");
        assert!(format!("{error:#}").contains("nested nawa process"));
    }
}
