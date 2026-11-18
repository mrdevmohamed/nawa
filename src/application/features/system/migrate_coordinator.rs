//! The migration coordinator (design §10.3–§10.5): the only code that may
//! create a migration record, drive the durable state machine, and continue
//! approved package operations while the mutation gate is active.
//!
//! §10.3's rules, and how this module satisfies them:
//!
//! * *The coordinator holds the exclusive package-database lock itself.*
//!   `prepare` takes it through the ordinary `lock::acquire` (which also
//!   runs the gate check: a second migration cannot begin while one is
//!   pending — the refusal names the pending migration). It keeps the
//!   guard for the whole preparation phase.
//! * *Pass both the lock guard and the permit through the internal
//!   application call chain without reacquiring the same non-reentrant
//!   lock.* The approved package steps run through
//!   `apply_transaction::install_for_migration`, which receives the held
//!   guard and the [`MigrationPermit`] and installs without a second
//!   acquire (flock is per open file description — reacquiring would
//!   deadlock against our own lock, and an ordinary acquire would be
//!   refused by the now-active gate).
//! * *The permit is not an environment variable, CLI flag, plugin
//!   capability, or inherited subprocess environment.* It is a plain
//!   in-memory value with a module-private seal: nothing outside this
//!   module can construct one, nothing outside this process can carry
//!   one, and no representation of it exists. A hook-spawned nested
//!   `nawa` receives no permit and is still refused by the gate.
//! * *The gate is cleared only after verified completion or a completed,
//!   verified rollback.* `prepare` never clears the gate on its own:
//!   all-steps-success leaves the record `awaiting-reboot` (gate active
//!   until the post-reboot resume), and a preparation failure lands in
//!   `failed` only when nothing was committed (trivially compensated) —
//!   otherwise `needs-recovery` (gate active; explicit recovery, later
//!   slice).
//!
//! What this slice deliberately does *not* contain: the live-host
//! preflight and the plan presentation (the future `apply` command's job,
//! §12.2 — its refusal of offline roots/chroots belongs there), the real
//! runit→systemd handler and its boot/initramfs steps (Phase 6, blocked
//! on B1/B2/B5), automatic compensation of committed steps (the recovery
//! slice: until then a mid-plan failure is fail-safe `needs-recovery`),
//! and the post-reboot resume/verify/finalize path (also the recovery
//! slice). The state machine itself is domain code and already complete.
//!
//! # The durable lifecycle and its crash boundaries (Phase 5B)
//!
//! `prepare` walks exactly one record-save order; every save is an atomic
//! publication ([`store::save`] → `atomic::write_atomic`: exclusive temp
//! file at 0600, file fsync, rename, directory fsync), so a crash never
//! leaves a torn record — the on-disk state is always the previous or the
//! next save point of this sequence:
//!
//! ```text
//! acquire exclusive lock → save planned → begin transaction →
//! save preparing (gate active) →
//!   per step: save intent → side effect → verify postcondition →
//!             save outcome →
//! save awaiting-reboot → finish the transaction
//! ```
//!
//! What each interruption or crash window leaves on disk:
//!
//! * before the planned save — nothing; a retry starts clean;
//! * after the planned save, before the gate save — a `planned` record,
//!   no gate, no (or a just-begun) transaction. `planned` is not
//!   gate-active (§10.3), so the mutation gate stays open; the record
//!   would still block a re-apply, so an *in-process* failure here
//!   abandons it (best-effort remove, reported in the error). A *process
//!   kill* in this window leaves the planned record behind for recovery
//!   (5C) to inspect and discard;
//! * after the gate save, before the first intent — `preparing`, gate
//!   active, no steps;
//! * between a step's intent and its outcome — the interrupted-step shape
//!   (intent, no outcome); recovery reconciles it against the actual
//!   system state (§10.3), never by re-running blindly;
//! * after the final outcome, before the `awaiting-reboot` save —
//!   `preparing` with every step resolved; recovery may re-conclude;
//! * after the `awaiting-reboot` save, before the transaction finishes —
//!   `awaiting-reboot` (gate active); the transaction history stays
//!   `preparing` until recovery finalizes it.
//!
//! Interruption (Ctrl+C/SIGTERM/SIGHUP) is deferred for the whole
//! preparation: the coordinator holds an [`crate::adapters::interrupt::Guard`], so a
//! signal during a record save or a side effect does not kill the process —
//! the current step completes and the between-steps check turns the pending
//! signal into that step's failure, routed through the normal lifecycle
//! split (nothing committed → `failed`, gate clear; something committed →
//! `needs-recovery`). A third signal still forces an immediate exit (see
//! `interrupt::FORCE_AFTER`): that is one of the crash windows above, and
//! the write-ahead record is what recovery reads.
//!
//! Failure classification never trusts the failing command's claim alone:
//! after a step error the actual package database is inspected (the shared
//! `committed_hook_failed` predicate plus an identity comparison against
//! the pre-step snapshot), so an error *after* the database committed can
//! never be reported as a trivially-compensated failure. The same
//! verification runs when a step *reports success*: the postcondition is
//! confirmed in the database before the outcome is persisted.
//!
//! The record on disk is authoritative for the mutation gate. An ending
//! that cannot be published durably is not a verified outcome: the
//! conclusion escalates to `needs-recovery` (still gate-keeping), and
//! every persistence error is reported through the interaction port —
//! the gate is never cleared to make a failed operation look successful.

// Everything in this module awaits its production consumer: the
// `nawa system migrate apply` command (§12.2) — which also brings the
// live-host preflight (§10.4 stage 2) and the real handler's boot steps
// (Phase 6). The use case itself is complete and driven by tests.
#![allow(dead_code)]

use anyhow::{anyhow, bail, Context, Result};
use std::path::{Path, PathBuf};

use crate::application::features::package::apply_transaction::install_for_migration;
use crate::application::ports::user_interaction::UserInteraction;
use crate::application::context::Ctx;
use crate::adapters::db::InstallReason;
use crate::domain::system::migration::{
    transition, MigrationDefinition, MigrationRecord, MigrationState, StepOutcome, StepRecord,
    Transition,
};
use crate::adapters::lock;
use crate::adapters::migration_record as store;
use crate::transaction::{self, context as tx_context};

/// Unforgeable in-process proof that this coordinator run may continue
/// its approved package operations while the mutation gate is active
/// (§10.3, §14.1). Zero-sized: the authority it proves is structural
/// (the holder is inside the coordinator's call path, beside the guard
/// that proves lock ownership). The seal field is module-private, so
/// the compiler rejects construction anywhere else; there is no
/// `Clone`/`Copy`, no serde, no `FromStr`, no `Display` and no
/// environment spelling that could impersonate one.
#[derive(Debug)]
pub(crate) struct MigrationPermit {
    _seal: Seal,
}

/// Private to this module; nothing outside can name a value of it.
#[derive(Debug)]
struct Seal;

impl MigrationPermit {
    /// Test seam for unit tests in other modules (store, apply path).
    /// Production code mints the permit inside `prepare` and passes it
    /// by reference only.
    #[cfg(test)]
    pub(crate) fn mint_for_tests() -> Self {
        Self { _seal: Seal }
    }
}

/// The caller's statement that the live-host preflight passed and the
/// plan was shown and explicitly approved (§10.4 stages 2–3; §12.4: not
/// a generic auto-confirm flag). The type pins the call-site order —
/// approval is constructed by whoever displayed the plan, *before*
/// `prepare` runs, and is consumed by it. What counts as "shown and
/// approved" for the future `apply` command is that command's policy.
#[derive(Debug)]
pub(crate) struct Approval {
    _private: (),
}

impl Approval {
    /// Record an explicit approval (the user answered yes to the
    /// displayed plan of this exact migration).
    pub(crate) fn explicit() -> Self {
        Self { _private: () }
    }
}

/// One approved preparation step (§10.4 stage 3→5): a stable id, the
/// §11 write-ahead intent, and the operation to execute. The only
/// operation kind so far is installing one already-staged, verified
/// artifact — the real runit→systemd handler's boot/initramfs steps
/// join when the handler lands (Phase 6, blocked on B1/B2/B5).
#[derive(Debug)]
pub(crate) struct PreparationStep {
    /// stable step id within the migration, in execution order
    pub(crate) id: String,
    /// the write-ahead intent persisted before the side effect (§10.3)
    pub(crate) intent: String,
    pub(crate) operation: PackageOperation,
}

/// What one step does. Artifacts are staged before the mutation phase:
/// nothing is fetched or built while the lock is held (§10.4 stage 4).
#[derive(Debug)]
pub(crate) enum PackageOperation {
    /// install one staged archive through the ordinary verified
    /// installer (`apply_transaction::install_for_migration`)
    Install {
        /// the staged archive file
        archive: PathBuf,
        /// the planned package name, for display and history
        package: String,
        /// the sha256 the approved plan pinned (installer-verified)
        expected_sha256: Option<String>,
        reason: InstallReason,
        force: bool,
        allow_unsigned: bool,
        /// provenance for the history record
        source: String,
    },
}

/// The approved plan: the exact steps the coordinator may execute, in
/// order (§10.3: the coordinator performs *only* the operations in its
/// approved plan).
#[derive(Debug)]
pub(crate) struct ApprovedPlan {
    pub(crate) steps: Vec<PreparationStep>,
}

/// Everything `prepare` needs. `approval` is consumed, so one prepared
/// run consumes exactly one approval.
pub(crate) struct PreparationRequest<'a> {
    /// the validated migration definition (its id names the record)
    pub(crate) definition: &'a MigrationDefinition,
    pub(crate) plan: ApprovedPlan,
    /// §10.4: the durable planned record exists only after this
    pub(crate) approval: Approval,
    /// progress output (the future `apply` command supplies the real
    /// presentation adapter)
    pub(crate) ui: &'a UserInteraction,
}

/// How one `prepare` run ended (the record is left in the matching
/// durable state on disk).
#[derive(Debug)]
pub(crate) enum PreparationOutcome {
    /// every step succeeded: `awaiting-reboot` — the gate stays active
    /// until the post-reboot resume verifies the new init (§10.4 stage
    /// 6: Nawa exits with a pending-migration status; it does not
    /// reboot automatically)
    Committed(MigrationRecord),
    /// a step failed with nothing committed before it: the machine is
    /// unchanged, compensation is trivially complete — `failed`, the
    /// gate is clear, and a retry is a new, re-approved plan (§10.3:
    /// terminal states have no exits)
    Compensated(MigrationRecord),
    /// a step failed after earlier steps committed and no verified
    /// compensation exists yet — `needs-recovery`, the gate stays
    /// active; recovery is explicit (the recovery slice)
    Ambiguous(MigrationRecord),
}

/// Run the preparation phase of one migration (§10.4 stage 5): take the
/// exclusive package-database lock, persist the `planned` record, begin
/// preparation (the mutation gate becomes active), execute the approved
/// steps through the ordinary verified installer under the held lock
/// with the write-ahead discipline, then commit `awaiting-reboot` or
/// split the failure by what the package database says was actually
/// committed. See the module header for the full durable lifecycle,
/// crash-boundary inventory and interruption behavior.
pub(crate) fn prepare(ctx: &Ctx, request: PreparationRequest<'_>) -> Result<PreparationOutcome> {
    let PreparationRequest {
        definition,
        plan,
        approval: Approval { _private: () },
        ui,
    } = request;

    // §10.3: beginning preparation requires the coordinator to hold the
    // exclusive package database lock itself — so no gate can race into
    // existence under us, and any pending migration's gate refuses the
    // begin with its precise error (the ordinary acquire path checks).
    let guard = lock::acquire(&ctx.root, lock::Mode::Exclusive)?.ok_or_else(|| {
        anyhow!("internal: the exclusive package database lock is required to prepare a migration")
    })?;
    // the permit lives exactly as long as this coordinator run
    let permit = MigrationPermit { _seal: Seal };
    // Ctrl+C/SIGTERM/SIGHUP are deferred for the whole preparation (the
    // guards nest with the installer's own): a signal does not kill the
    // process mid-save or mid-side-effect — the next between-steps check
    // turns it into that step's failure, which the lifecycle split below
    // resolves (see the module header). Guards drop after `conclude`.
    let _interrupt_guard = crate::adapters::interrupt::Guard::new();

    // A migration id is unique under a root. An existing record (any
    // state) is never silently overwritten: re-running a failed
    // migration is a recovery/re-plan decision for the recovery slice. A
    // record that cannot be read fails the begin closed — never overwrite
    // what cannot be ruled out.
    if store::load(&ctx.root, &definition.id)?.is_some() {
        bail!(
            "a migration record for {} already exists under {} — a pending migration must be recovered first, a terminal one is final (§10.3)",
            definition.id,
            store::migrations_dir(&ctx.root).display()
        );
    }

    // The durable planned record: written only after preflight and
    // explicit approval (the consumed `Approval` proves the caller's
    // order) and immediately before the gate is taken (§10.3).
    let mut record = MigrationRecord::new(definition);
    persist(&ctx.root, &record).with_context(|| {
        format!(
            "migration {}: the planned record could not be created",
            definition.id
        )
    })?;

    // One real package transaction links the record to history (§11):
    // the requested list names the migration so `nawa history` can show
    // which migration drove the changes.
    let mut op = match tx_context::begin_operation(
        &ctx.root,
        transaction::OperationCommand::Migration,
        std::slice::from_ref(&definition.id),
        &[],
    ) {
        Ok(op) => op,
        Err(error) => {
            // the gate never became active; abandon the planned record so
            // a re-apply is not blocked by a lifecycle that never started
            // (§10.3: the planned record exists only immediately before
            // the gate)
            return Err(abandon_planned(
                &ctx.root,
                &record,
                error.context(format!(
                    "migration {}: the preparation transaction could not be begun",
                    definition.id
                )),
            ));
        }
    };

    // BeginPreparation under our own lock: the gate becomes active now.
    // From here on every ordinary exclusive acquisition under this root
    // is refused — ours continue through the held guard instead.
    record.state = match transition(record.state, Transition::BeginPreparation) {
        Ok(state) => state,
        Err(error) => {
            return Err(abandon_gate_failure(
                &ctx.root,
                &record,
                op,
                anyhow::Error::msg(error).context(format!("migration {}", definition.id)),
                ui,
            ));
        }
    };
    if let Err(error) = persist(&ctx.root, &record) {
        return Err(abandon_gate_failure(
            &ctx.root,
            &record,
            op,
            error.context(format!(
                "migration {}: the mutation gate could not be activated",
                definition.id
            )),
            ui,
        ));
    }

    // Write-ahead step loop (§10.3). Every failure — side effect, record
    // save, or deferred interrupt — is routed through the same lifecycle
    // split in `conclude`, so the record, the transaction history and the
    // mutation gate can never silently diverge.
    let total = plan.steps.len();
    let mut failure: Option<anyhow::Error> = None;
    let mut committed_any = false;
    for (index, step) in plan.steps.iter().enumerate() {
        // Ctrl+C during the previous side effect was deferred until that
        // effect completed (the installer's own guard); between steps the
        // system is consistent. The interrupt becomes a step failure like
        // any other: nothing committed → compensated, something committed
        // → needs-recovery.
        if let Err(interrupted) = crate::adapters::interrupt::check() {
            failure = Some(anyhow!(
                "interrupted before migration step {} ({}): {interrupted}",
                step.id,
                step.intent
            ));
            break;
        }

        // the pre-step database snapshot grounds the commit
        // classification after a failure (§10.3: never assume a failed
        // command means that no side effect occurred)
        let before = match &step.operation {
            PackageOperation::Install { package, .. } => {
                match crate::adapters::db::Db::new(&ctx.root).entry(package) {
                    Ok(entry) => entry,
                    Err(error) => {
                        failure = Some(anyhow!(
                            "migration step {}: the pre-step package state could not be read: {error:#}",
                            step.id
                        ));
                        break;
                    }
                }
            }
        };

        // write-ahead: the intent is durable BEFORE the side effect
        record.steps.push(StepRecord {
            id: step.id.clone(),
            intent: Some(step.intent.clone()),
            outcome: None,
        });
        if let Err(error) = persist(&ctx.root, &record) {
            // no side effect ran; the step failed before its effect
            let last = record
                .steps
                .last_mut()
                .expect("a step record was just pushed");
            last.outcome = Some(StepOutcome::Failed {
                reason: format!("the write-ahead intent could not be persisted: {error:#}"),
            });
            failure = Some(anyhow!(
                "migration step {} failed before its side effect: {error:#}",
                step.id
            ));
            break;
        }

        let result = match &step.operation {
            PackageOperation::Install {
                archive,
                package,
                expected_sha256,
                reason,
                force,
                allow_unsigned,
                source,
            } => {
                (ui.info)(&format!(
                    "[{}/{}] {} ({})",
                    index + 1,
                    total,
                    step.intent,
                    step.id
                ));
                install_for_migration(
                    ctx,
                    crate::application::features::package::apply_transaction::MigrationInstall {
                        archive,
                        what: package,
                        number: index + 1,
                        total,
                        expected_sha256: expected_sha256.as_deref(),
                        reason: *reason,
                        force: *force,
                        allow_unsigned: *allow_unsigned,
                        source,
                    },
                    &mut op,
                    ui,
                    &guard,
                    &permit,
                )
            }
        };

        // the outcome is persisted only after the side effect returned,
        // and only for a verified postcondition (§10.3)
        let last = record
            .steps
            .last_mut()
            .expect("a step record was just pushed");
        match result {
            Ok(()) => {
                if let Err(error) = verify_install_postcondition(ctx, &step.operation) {
                    // the step claims success but the postcondition does
                    // not hold: the outcome records the failed
                    // verification, never a success, and the commit
                    // classification reads the actual database
                    let committed = step_committed(ctx, &step.operation, before.as_ref());
                    committed_any |= committed;
                    last.outcome = Some(StepOutcome::Failed {
                        reason: format!("{error:#}"),
                    });
                    failure = Some(
                        persist(&ctx.root, &record)
                            .err()
                            .map(|save_error| {
                                anyhow!(
                                    "migration step {}: the postcondition failed ({error:#}) and the outcome could not be persisted: {save_error:#}",
                                    step.id
                                )
                            })
                            .unwrap_or_else(|| {
                                anyhow!(
                                    "migration step {} failed its postcondition: {error:#}",
                                    step.id
                                )
                            }),
                    );
                    break;
                }
                last.outcome = Some(StepOutcome::Succeeded);
                committed_any = true;
                if let Err(error) = persist(&ctx.root, &record) {
                    // the effect is durable; the outcome may not be — the
                    // record on disk keeps the interrupted-step shape
                    // until recovery reconciles it against the system
                    failure = Some(anyhow!(
                        "migration step {} committed but its outcome could not be persisted: {error:#}",
                        step.id
                    ));
                    break;
                }
            }
            Err(error) => {
                let reason = format!("{error:#}");
                // §10.3: never assume a failed command means that no side
                // effect occurred — classify from the actual database
                let committed = step_committed(ctx, &step.operation, before.as_ref());
                committed_any |= committed;
                last.outcome = Some(StepOutcome::Failed {
                    reason: reason.clone(),
                });
                failure = Some(
                    persist(&ctx.root, &record)
                        .err()
                        .map(|save_error| {
                            anyhow!(
                                "migration step {} failed ({reason}) and its outcome could not be persisted: {save_error:#}",
                                step.id
                            )
                        })
                        .unwrap_or_else(|| anyhow!("migration step {} failed: {reason}", step.id)),
                );
                break;
            }
        }
    }

    // Split the ending by what the package database says was committed
    // (§10.3's compensation rule), publish it durably, and finish the
    // transaction history. The guard drops when `prepare` returns: the
    // flock releases; if the migration is gate-active, every ordinary
    // package mutation is refused from now on (§10.3) until the record
    // reaches a terminal state through resume/recovery.
    conclude(ctx, record, op, failure.as_ref(), committed_any, ui)
}

/// Publish `record` durably and verify the publication.
///
/// `atomic::write_atomic` returns Err after a successful rename when only
/// the directory-entry fsync failed: the record is published but its
/// durability is unconfirmed. In that state the on-disk record already *is*
/// the intended one — for a gate-active state the gate is active — and
/// removing it would clear the gate to make a failure look successful. So a
/// failed save is re-read: when the on-disk record equals the intended one,
/// the publication stands (the next save re-confirms durability; a crash
/// that loses the unsynced directory entry simply leaves the older state,
/// which every save boundary treats as its input). Anything else is a real
/// failure carrying what is actually on disk.
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

/// §5B decision: a `planned` record whose gate never became durable must
/// not strand the lifecycle. It is not gate-active (so package mutations
/// stay possible), but it would block every re-apply of this migration id,
/// and no recovery decision can be made from it that a fresh, re-approved
/// plan would not make better. Best-effort remove; a record that cannot be
/// removed stays on disk for inspection and is named in the error for the
/// recovery slice to discard.
fn abandon_planned(root: &Path, record: &MigrationRecord, error: anyhow::Error) -> anyhow::Error {
    match store::remove(root, &record.id) {
        Ok(()) => error.context(format!(
            "migration {}: the abandoned planned record was removed — re-running apply starts a fresh, re-approved plan",
            record.id
        )),
        Err(remove_error) => error.context(format!(
            "migration {}: the abandoned planned record could not be removed ({remove_error:#}) — recovery must inspect and discard it",
            record.id
        )),
    }
}

/// The mutation gate never became durable: end the just-begun transaction
/// (best-effort, reported through the interaction port) and abandon the
/// ungated planned record. The gate is never cleared to make a failure
/// look successful — there is no gate here to clear, and the record that
/// would pretend otherwise is removed rather than left lying about.
fn abandon_gate_failure(
    root: &Path,
    record: &MigrationRecord,
    op: tx_context::OperationHandle,
    error: anyhow::Error,
    ui: &UserInteraction,
) -> anyhow::Error {
    if let Err(finish_error) = op.finish(Some(&error)) {
        (ui.warn)(&format!(
            "migration {}: the aborted preparation transaction could not be finalized ({finish_error:#}) — the history record stays `preparing` until recovery finalizes it",
            record.id
        ));
    }
    abandon_planned(root, record, error)
}

/// Did this step change the package database? Decided from actual state
/// (§10.3: never assume a failed command means that no side effect
/// occurred, and never the other way either):
///
/// * the planned package's identity (presence, version/rel, installed
///   digest, recorded reason) differs from the pre-step snapshot — the
///   database changed, whatever the step reported;
/// * otherwise the installer's shared committed-with-hook-failure
///   predicate decides (a failed post-install hook leaves the package
///   committed with the hook to retry). It needs the planned target, read
///   from the staged archive; an unreadable archive cannot have been
///   installed, so the identity comparison above is the whole answer.
///
/// A database that cannot be read is fail-safe: assume the step committed
/// (the gate stays active; recovery inspects the real state).
fn step_committed(
    ctx: &Ctx,
    operation: &PackageOperation,
    before: Option<&crate::adapters::db::Entry>,
) -> bool {
    let PackageOperation::Install {
        archive,
        package,
        reason,
        ..
    } = operation;
    let current = match crate::adapters::db::Db::new(&ctx.root).entry(package) {
        Ok(current) => current,
        Err(_) => return true,
    };
    if !same_install_state(current.as_ref(), before) {
        return true;
    }
    let target = match crate::install::inspect_archive_state(archive, *reason, None) {
        Ok(target) => target,
        Err(_) => return false,
    };
    let retry = crate::install::hook_needs_retry(&ctx.root, package).unwrap_or(false);
    crate::transaction::history::committed_hook_failed(current.as_ref(), before, &target, retry)
}

/// Whether two installed states are the same package state: presence,
/// version/rel, the installed digest, and the recorded reason (legacy
/// records may leave the reason empty, which stays compatible).
fn same_install_state(
    current: Option<&crate::adapters::db::Entry>,
    before: Option<&crate::adapters::db::Entry>,
) -> bool {
    match (current, before) {
        (None, None) => true,
        (Some(current), Some(before)) => {
            current.version == before.version
                && current.rel == before.rel
                && current.artifact_sha256 == before.artifact_sha256
                && (current.reason.is_empty()
                    || before.reason.is_empty()
                    || current.reason == before.reason)
        }
        _ => false,
    }
}

/// The §10.3 verify step for an install step: confirm the postcondition in
/// the actual database before the outcome is persisted. The planned
/// package must be present at the planned identity, and a plan-pinned
/// digest must be the digest that was installed.
fn verify_install_postcondition(ctx: &Ctx, operation: &PackageOperation) -> Result<()> {
    let PackageOperation::Install {
        archive,
        package,
        reason,
        expected_sha256,
        ..
    } = operation;
    let current = crate::adapters::db::Db::new(&ctx.root)
        .entry(package)?
        .ok_or_else(|| {
            anyhow!(
                "postcondition failed: {package} is not in the package database after the install step"
            )
        })?;
    let target = crate::install::inspect_archive_state(archive, *reason, None)
        .with_context(|| format!("postcondition check for {package}"))?;
    let target_version = target.version.clone().unwrap_or_default();
    if current.version != target_version || Some(current.rel) != target.rel {
        bail!(
            "postcondition failed: {package} is {}-{} in the package database but the plan installed {}-{}",
            current.version,
            current.rel,
            target_version,
            target.rel.map(|rel| rel.to_string()).unwrap_or_default()
        );
    }
    if let Some(pinned) = expected_sha256 {
        if current.artifact_sha256 != *pinned {
            bail!(
                "postcondition failed: {package} was installed from digest {} but the plan pinned {pinned}",
                current.artifact_sha256
            );
        }
    }
    Ok(())
}

/// Close the preparation phase: apply the §10.3 ending transition decided
/// by the *verified* commit state, publish it durably, finish the
/// transaction history best-effort, and report the outcome.
///
/// The record on disk is authoritative for the mutation gate: an ending
/// that cannot be published is not a verified outcome, so `Committed` and
/// `Compensated` conclusions escalate to `needs-recovery` (still
/// gate-keeping) when their state cannot be made durable. History
/// finalization is best-effort for the same reason — a history write
/// failure must not undo or mask the migration's outcome, and the
/// outstanding `preparing` record is what recovery finalizes.
fn conclude(
    ctx: &Ctx,
    mut record: MigrationRecord,
    op: tx_context::OperationHandle,
    failure: Option<&anyhow::Error>,
    committed_any: bool,
    ui: &UserInteraction,
) -> Result<PreparationOutcome> {
    let id = record.id.clone();
    let pre_state = record.state;
    let ending = match failure {
        None => Transition::PreparationCommitted,
        Some(_) if !committed_any => Transition::PreparationCompensated,
        Some(_) => Transition::PreparationAmbiguous,
    };
    record.state = transition(pre_state, ending)
        .map_err(anyhow::Error::msg)
        .with_context(|| format!("migration {id}"))?;

    let mut ending_error: Option<anyhow::Error> = None;
    if let Err(error) = persist(&ctx.root, &record) {
        if matches!(ending, Transition::PreparationAmbiguous) {
            // needs-recovery itself could not be published: the record
            // stays in its previous gate-keeping state on disk
            ending_error = Some(error);
        } else {
            // the intended ending never became durable: the durable state
            // is still `preparing` (gate-keeping) — declare needs-recovery
            // from *that* state, which is both legal and the only honest
            // ending (a conclusion that cannot be made durable is not a
            // verified outcome)
            let original = error;
            record.state = transition(pre_state, Transition::PreparationAmbiguous)
                .map_err(anyhow::Error::msg)
                .with_context(|| format!("migration {id}"))?;
            if let Err(retry) = persist(&ctx.root, &record) {
                ending_error =
                    Some(retry.context(format!("the intended ending failed first: {original:#}")));
            }
        }
    }

    // The transaction history is finished best-effort (see above).
    if let Err(error) = op.finish(failure) {
        (ui.warn)(&format!(
            "migration {id}: the preparation transaction history could not be finalized ({error:#}) — the migration record under {} is authoritative and recovery finalizes the history",
            store::migrations_dir(&ctx.root).display()
        ));
    }
    if let Some(error) = &ending_error {
        (ui.warn)(&format!(
            "migration {id}: the record could not be published in its final state ({error:#}) — the on-disk record is the truth the mutation gate follows",
        ));
    }
    match record.state {
        MigrationState::AwaitingReboot => {
            (ui.info)(&format!(
                "migration {id} is awaiting-reboot — the package mutation gate stays active until the post-reboot resume (§10.4)"
            ));
        }
        MigrationState::Failed => {
            (ui.warn)(&format!(
                "migration {id} failed before any step changed the system: {:#}",
                failure.expect("`failed` is only concluded for a step failure")
            ));
        }
        _ => {
            let reason = match failure {
                Some(error) => format!("{error:#}"),
                None => match &ending_error {
                    Some(error) => {
                        format!("the concluding record state could not be published: {error:#}")
                    }
                    // the intended ending never became durable, so the
                    // escalation to needs-recovery was published instead
                    None => concat!(
                        "the intended conclusion could not be published, so the",
                        " durable record was declared needs-recovery instead",
                    )
                    .to_string(),
                },
            };
            (ui.warn)(&format!(
                "migration {id} stopped in needs-recovery (the mutation gate stays active): {reason}"
            ));
        }
    }
    Ok(match record.state {
        MigrationState::AwaitingReboot => PreparationOutcome::Committed(record),
        MigrationState::Failed => PreparationOutcome::Compensated(record),
        _ => PreparationOutcome::Ambiguous(record),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::archive;
    use crate::domain::system::capability::Capability;
    use crate::domain::system::migration::{HandlerId, MigrationState};
    use std::cell::RefCell;
    use std::path::Path;
    use std::rc::Rc;

    // ---- fixtures ------------------------------------------------------

    fn cap(key: &str) -> Capability {
        Capability::parse(key).unwrap()
    }

    fn definition() -> MigrationDefinition {
        MigrationDefinition::new(
            "faseela-runit-to-systemd-v1",
            1,
            cap("init-system=runit"),
            cap("init-system=systemd"),
            HandlerId::FaseelaInitRunitToSystemd,
            true,
        )
        .unwrap()
    }

    fn ctx_with(root: &std::path::Path) -> Ctx {
        Ctx::from_config(&crate::adapters::config::Config {
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

    /// A fresh scratch root with a minimal but real package archive:
    /// `.PKGINFO` plus one payload file, so the coordinator's install
    /// steps run through the ordinary verified installer. Returns the
    /// archive path and its sha256, pinned the way a real plan pins its
    /// staged artifacts (§10.4 stage 4).
    fn staged_archive(root: &std::path::Path, name: &str, version: &str) -> (PathBuf, String) {
        let pkgdir = root.join("stage").join(name);
        std::fs::create_dir_all(pkgdir.join("usr/share")).unwrap();
        std::fs::write(pkgdir.join("usr/share/marker"), name).unwrap();
        let pkginfo = format!("pkgname = {name}\npkgver = {version}\npkgrel = 1\narch = x86_64\n");
        let out = root.join("stage").join(format!("{name}.nawa.tar.zst"));
        archive::create(&pkgdir, &pkginfo, &out).unwrap();
        let digest = crate::adapters::checksum::sha256_of_file(&out).unwrap();
        (out, digest)
    }

    /// The recorder adapter: records every port call, approves nothing.
    #[derive(Default)]
    struct Recorder {
        lines: Rc<RefCell<Vec<String>>>,
    }

    impl Recorder {
        fn ui(&self) -> UserInteraction {
            let lines = Rc::clone(&self.lines);
            let stderr_line =
                Box::new(move |l: &str| lines.borrow_mut().push(format!("STDERR {l}")));
            let lines = Rc::clone(&self.lines);
            let info = Box::new(move |l: &str| lines.borrow_mut().push(format!("INFO {l}")));
            let lines = Rc::clone(&self.lines);
            let warn = Box::new(move |l: &str| lines.borrow_mut().push(format!("WARN {l}")));
            let lines = Rc::clone(&self.lines);
            let show_plan = Box::new(
                move |_view: &crate::application::ports::user_interaction::PlanView| {
                    lines.borrow_mut().push("SHOW".to_string());
                },
            );
            let choose =
                Box::new(|_prompt: &str, _d: char, _a: &[char]| -> Result<char> { Ok('r') });
            let confirm = Box::new(|_prompt: &str| -> Result<()> { Ok(()) });
            UserInteraction {
                stderr_line,
                info,
                warn,
                show_plan,
                choose,
                confirm,
            }
        }
    }

    fn install_step(name: &str, archive: &Path, sha: &str, pin: bool) -> PreparationStep {
        PreparationStep {
            id: format!("install-{name}"),
            intent: format!("install the staged {name} artifact"),
            operation: PackageOperation::Install {
                archive: archive.to_path_buf(),
                package: name.to_string(),
                expected_sha256: pin.then(|| sha.to_string()),
                reason: InstallReason::Explicit,
                force: false,
                allow_unsigned: true,
                source: "migration-staged".to_string(),
            },
        }
    }

    fn prepare_with(ctx: &Ctx, steps: Vec<PreparationStep>) -> Result<PreparationOutcome> {
        let recorder = Recorder::default();
        let ui = recorder.ui();
        prepare(
            ctx,
            PreparationRequest {
                definition: &definition(),
                plan: ApprovedPlan { steps },
                approval: Approval::explicit(),
                ui: &ui,
            },
        )
    }

    fn failed_reason(step: &StepRecord) -> String {
        match &step.outcome {
            Some(StepOutcome::Failed { reason }) => reason.clone(),
            other => panic!("expected a failed outcome, got {other:?}"),
        }
    }

    // ---- the lifecycle -------------------------------------------------

    #[test]
    fn all_steps_succeed_into_awaiting_reboot_with_the_gate_active() {
        let root = crate::testutil::tmpdir("migrate-coord-commit");
        let ctx = ctx_with(&root);
        let (archive, sha) = staged_archive(&root, "sysd", "1");
        let outcome = prepare_with(&ctx, vec![install_step("sysd", &archive, &sha, true)])
            .expect("the staged install succeeds");
        let record = match &outcome {
            PreparationOutcome::Committed(record) => record.clone(),
            other => panic!("expected Committed, got {other:?}"),
        };
        assert_eq!(record.state, MigrationState::AwaitingReboot);
        assert_eq!(
            record.steps,
            vec![StepRecord {
                id: "install-sysd".to_string(),
                intent: Some("install the staged sysd artifact".to_string()),
                outcome: Some(StepOutcome::Succeeded),
            }],
            "the write-ahead intent and the post-effect outcome are both durable"
        );
        // the record is on disk in the committed state
        let on_disk = store::load(&root, record.id.as_str())
            .unwrap()
            .expect("the record was published");
        assert_eq!(on_disk.state, MigrationState::AwaitingReboot);
        // the real package was installed through the ordinary installer
        let db = crate::adapters::db::Db::new(&root);
        assert!(
            db.entry("sysd").unwrap().is_some(),
            "the approved step really installed the package"
        );
        // and the gate is now active: an ordinary exclusive acquisition
        // is refused naming the migration (§10.3)
        let error = lock::acquire(&root, lock::Mode::Exclusive).unwrap_err();
        let text = format!("{error:#}");
        assert!(text.contains("faseela-runit-to-systemd-v1"), "{text}");
        assert!(text.contains("awaiting-reboot"), "{text}");
        // history links the transaction to the migration (§11)
        let listing = crate::transaction::history::list_records(&root).unwrap();
        assert_eq!(listing.len(), 1, "{listing:?}");
        assert_eq!(listing[0].command, "migrate");
        assert_eq!(
            listing[0].requested_packages,
            vec!["faseela-runit-to-systemd-v1".to_string()]
        );
        assert_eq!(
            listing[0].status().unwrap(),
            crate::transaction::history::TxStatus::Completed
        );
    }

    #[test]
    fn a_first_step_failure_compensates_trivially_and_clears_the_gate() {
        let root = crate::testutil::tmpdir("migrate-coord-firstfail");
        let ctx = ctx_with(&root);
        // a nonexistent archive: the very first step fails before any
        // side effect, so nothing was committed
        let step = PreparationStep {
            id: "install-sysd".to_string(),
            intent: "install the staged sysd artifact".to_string(),
            operation: PackageOperation::Install {
                archive: root.join("stage/missing.nawa.tar.zst"),
                package: "sysd".to_string(),
                expected_sha256: None,
                reason: InstallReason::Explicit,
                force: false,
                allow_unsigned: true,
                source: "migration-staged".to_string(),
            },
        };
        let outcome =
            prepare_with(&ctx, vec![step]).expect("the run itself completes (with a failed step)");
        let record = match &outcome {
            PreparationOutcome::Compensated(record) => record.clone(),
            other => panic!("expected Compensated, got {other:?}"),
        };
        assert_eq!(record.state, MigrationState::Failed);
        // the failed step keeps its intent and names the failure
        let reason = failed_reason(&record.steps[0]);
        assert!(
            !reason.is_empty(),
            "the failure reason is durable: {reason}"
        );
        // nothing was installed
        let db = crate::adapters::db::Db::new(&root);
        assert!(db.entry("sysd").unwrap().is_none());
        // the gate is clear: an ordinary exclusive acquisition works
        let guard = lock::acquire(&root, lock::Mode::Exclusive)
            .unwrap()
            .expect("no gate is active");
        drop(guard);
        // history shows the failed transaction
        let listing = crate::transaction::history::list_records(&root).unwrap();
        assert_eq!(
            listing[0].status().unwrap(),
            crate::transaction::history::TxStatus::Failed
        );
    }

    #[test]
    fn a_mid_plan_failure_lands_in_needs_recovery_with_the_gate_active() {
        let root = crate::testutil::tmpdir("migrate-coord-midfail");
        let ctx = ctx_with(&root);
        let (archive, sha) = staged_archive(&root, "sysd", "1");
        // step 1 succeeds; step 2 points at a missing archive
        let steps = vec![
            install_step("sysd", &archive, &sha, true),
            PreparationStep {
                id: "install-integration".to_string(),
                intent: "install the staged integration artifact".to_string(),
                operation: PackageOperation::Install {
                    archive: root.join("stage/missing.nawa.tar.zst"),
                    package: "integration".to_string(),
                    expected_sha256: None,
                    reason: InstallReason::Explicit,
                    force: false,
                    allow_unsigned: true,
                    source: "migration-staged".to_string(),
                },
            },
        ];
        let outcome = prepare_with(&ctx, steps).expect("the run completes with a failed step");
        let record = match &outcome {
            PreparationOutcome::Ambiguous(record) => record.clone(),
            other => panic!("expected Ambiguous, got {other:?}"),
        };
        assert_eq!(record.state, MigrationState::NeedsRecovery);
        assert_eq!(record.steps.len(), 2, "the failed step is also recorded");
        assert_eq!(record.steps[0].outcome, Some(StepOutcome::Succeeded));
        let reason = failed_reason(&record.steps[1]);
        assert!(
            reason.contains("missing.nawa.tar.zst") || !reason.is_empty(),
            "the failure is durable: {reason}"
        );
        // the first package really was installed (it committed)
        let db = crate::adapters::db::Db::new(&root);
        assert!(db.entry("sysd").unwrap().is_some());
        // the gate stays active (needs-recovery is gate-keeping)
        let error = lock::acquire(&root, lock::Mode::Exclusive).unwrap_err();
        assert!(format!("{error:#}").contains("needs-recovery"));
        // history shows the partial transaction
        let listing = crate::transaction::history::list_records(&root).unwrap();
        assert_eq!(
            listing[0].status().unwrap(),
            crate::transaction::history::TxStatus::Partial
        );
    }

    #[test]
    fn the_begin_refuses_when_a_record_already_exists() {
        let root = crate::testutil::tmpdir("migrate-coord-existing");
        let ctx = ctx_with(&root);
        // any record with the migration's id blocks the begin
        let mut record = MigrationRecord::new(&definition());
        record.state = MigrationState::Failed;
        store::save(&root, &record).unwrap();
        let error = prepare_with(&ctx, vec![]).unwrap_err();
        let text = format!("{error:#}");
        assert!(
            text.contains("already exists"),
            "the refusal names the existing record: {text}"
        );
        assert!(text.contains("faseela-runit-to-systemd-v1"), "{text}");
        // the existing record was not modified
        let on_disk = store::load(&root, "faseela-runit-to-systemd-v1")
            .unwrap()
            .unwrap();
        assert_eq!(on_disk.state, MigrationState::Failed);
    }

    #[test]
    fn the_begin_refuses_while_another_migration_is_pending() {
        let root = crate::testutil::tmpdir("migrate-coord-pending");
        let ctx = ctx_with(&root);
        // a pending migration under a different id holds the gate
        let other = MigrationDefinition::new(
            "other-migration",
            1,
            cap("init-system=runit"),
            cap("init-system=systemd"),
            HandlerId::FaseelaInitRunitToSystemd,
            true,
        )
        .unwrap();
        let mut record = MigrationRecord::new(&other);
        record.state = MigrationState::Preparing;
        store::save(&root, &record).unwrap();
        let error = prepare_with(&ctx, vec![]).unwrap_err();
        let text = format!("{error:#}");
        assert!(
            text.contains("other-migration is preparing"),
            "the gate refusal names the pending migration: {text}"
        );
    }

    #[test]
    fn an_empty_plan_still_walks_the_full_lifecycle() {
        // a plan with no steps: planned → preparing → awaiting-reboot
        // with no side effects (the §10.3 shape must not depend on
        // there being steps to run)
        let root = crate::testutil::tmpdir("migrate-coord-empty");
        let ctx = ctx_with(&root);
        let outcome = prepare_with(&ctx, vec![]).expect("an empty plan prepares trivially");
        let record = match &outcome {
            PreparationOutcome::Committed(record) => record.clone(),
            other => panic!("expected Committed, got {other:?}"),
        };
        assert_eq!(record.state, MigrationState::AwaitingReboot);
        assert!(record.steps.is_empty());
        assert!(store::load(&root, record.id.as_str()).unwrap().is_some());
        // the gate is active (awaiting-reboot keeps it)
        let error = lock::acquire(&root, lock::Mode::Exclusive).unwrap_err();
        assert!(format!("{error:#}").contains("awaiting-reboot"));
    }

    #[test]
    fn an_interrupted_step_shape_is_the_write_ahead_crash_window() {
        // §10.3: the intent is persisted before the side effect, the
        // outcome after it — so a crash between the two leaves exactly
        // this shape on disk, which the recovery slice reconciles. The
        // coordinator produces it structurally (the two saves surround
        // the side effect); the store must roundtrip it honestly.
        let root = crate::testutil::tmpdir("migrate-coord-window");
        let mut record = MigrationRecord::new(&definition());
        record.state = MigrationState::Preparing;
        record.steps = vec![StepRecord {
            id: "install-sysd".to_string(),
            intent: Some("install the staged sysd artifact".to_string()),
            outcome: None,
        }];
        store::save(&root, &record).unwrap();
        let on_disk = store::load(&root, record.id.as_str())
            .unwrap()
            .expect("the interrupted record roundtrips");
        assert_eq!(
            on_disk.steps[0].intent.as_deref(),
            Some("install the staged sysd artifact")
        );
        assert_eq!(
            on_disk.steps[0].outcome, None,
            "the crash window is preserved"
        );
    }

    // ---- Phase 5B: durable lifecycle failure boundaries -----------------
    //
    // Every test here pins one crash/interruption boundary of the
    // write-ahead lifecycle documented in the module header: what is on
    // disk, whether the mutation gate is active, what the transaction
    // history says, and whether the system was changed — after each
    // failure shape.

    /// RAII: an armed atomic-write injection is always cleared, even when
    /// an assertion fails mid-test (thread-locals survive across pooled
    /// test threads).
    struct Armed;
    impl Drop for Armed {
        fn drop(&mut self) {
            crate::adapters::atomic::failure_injection::clear();
        }
    }

    /// Arm `point` on the `nth` atomic write whose path contains
    /// `path_contains` (record saves live under `migrations/`, history
    /// records under `history/tx-*.toml`; journal step files match
    /// neither, so the counts are independent of install internals).
    fn arm(point: crate::adapters::atomic::FailPoint, nth: u32, path_contains: &'static str) -> Armed {
        crate::adapters::atomic::failure_injection::arm_at(point, nth, path_contains);
        Armed
    }

    /// RAII: a simulated interrupt is always cleared.
    struct Simulated;
    impl Drop for Simulated {
        fn drop(&mut self) {
            crate::adapters::interrupt::clear_simulated_interrupt();
        }
    }

    /// Like [`prepare_with`], but also returns the interaction record so
    /// tests can assert what the user was told.
    fn prepare_capture(
        ctx: &Ctx,
        steps: Vec<PreparationStep>,
    ) -> (Result<PreparationOutcome>, Rc<RefCell<Vec<String>>>) {
        let recorder = Recorder::default();
        let ui = recorder.ui();
        let outcome = prepare(
            ctx,
            PreparationRequest {
                definition: &definition(),
                plan: ApprovedPlan { steps },
                approval: Approval::explicit(),
                ui: &ui,
            },
        );
        (outcome, Rc::clone(&recorder.lines))
    }

    fn committed(record: &PreparationOutcome) -> MigrationRecord {
        match record {
            PreparationOutcome::Committed(record) => record.clone(),
            other => panic!("expected Committed, got {other:?}"),
        }
    }

    fn compensated(record: &PreparationOutcome) -> MigrationRecord {
        match record {
            PreparationOutcome::Compensated(record) => record.clone(),
            other => panic!("expected Compensated, got {other:?}"),
        }
    }

    fn ambiguous(record: &PreparationOutcome) -> MigrationRecord {
        match record {
            PreparationOutcome::Ambiguous(record) => record.clone(),
            other => panic!("expected Ambiguous, got {other:?}"),
        }
    }

    fn told(lines: &Rc<RefCell<Vec<String>>>) -> Vec<String> {
        lines.borrow().clone()
    }

    #[test]
    fn a_publish_only_gate_save_failure_leaves_the_gate_active_and_the_run_completes() {
        // The rename already happened; only the directory-entry fsync
        // failed: `store::save` returns Err while the record on disk IS
        // the intended `preparing` state. The coordinator must re-read
        // and treat "on-disk == intended" as success — removing the
        // record here would clear a gate that is durably active (§10.3).
        let root = crate::testutil::tmpdir("migrate-coord-publish-gate");
        let ctx = ctx_with(&root);
        // record saves under `migrations/`: 1 planned, 2 the gate
        let _armed = arm(crate::adapters::atomic::FailPoint::Published, 2, "migrations");
        let outcome =
            prepare_with(&ctx, vec![]).expect("the published-but-unconfirmed gate save stands");
        let record = committed(&outcome);
        assert_eq!(record.state, MigrationState::AwaitingReboot);
        let on_disk = store::load(&root, record.id.as_str()).unwrap().unwrap();
        assert_eq!(on_disk.state, MigrationState::AwaitingReboot);
        let error = lock::acquire(&root, lock::Mode::Exclusive).unwrap_err();
        assert!(
            format!("{error:#}").contains("awaiting-reboot"),
            "{error:#}"
        );
    }

    #[test]
    fn a_failed_gate_publication_abandons_the_ungated_planned_record() {
        // The gate save failed before publishing: the on-disk record is
        // still `planned` — not gate-active, but it would block every
        // re-apply. The in-process failure abandons it instead of
        // stranding a lifecycle that never started.
        let root = crate::testutil::tmpdir("migrate-coord-gate-wrote");
        let ctx = ctx_with(&root);
        let _armed = arm(crate::adapters::atomic::FailPoint::Wrote, 2, "migrations");
        let error = prepare_with(&ctx, vec![]).unwrap_err();
        let text = format!("{error:#}");
        assert!(
            text.contains("the mutation gate could not be activated"),
            "{text}"
        );
        assert!(
            text.contains("abandoned planned record was removed"),
            "{text}"
        );
        assert!(store::load(&root, "faseela-runit-to-systemd-v1")
            .unwrap()
            .is_none());
        // the gate never became active: ordinary acquisitions work
        lock::acquire(&root, lock::Mode::Exclusive)
            .unwrap()
            .expect("no gate is active");
        // the just-begun transaction was finalized as failed
        let listing = crate::transaction::history::list_records(&root).unwrap();
        assert_eq!(listing.len(), 1, "{listing:?}");
        assert_eq!(
            listing[0].status().unwrap(),
            crate::transaction::history::TxStatus::Failed
        );
    }

    #[test]
    fn a_failed_planned_publication_fails_closed_and_a_retry_starts_clean() {
        let root = crate::testutil::tmpdir("migrate-coord-planned-wrote");
        let ctx = ctx_with(&root);
        {
            let _armed = arm(crate::adapters::atomic::FailPoint::Wrote, 1, "migrations");
            let error = prepare_with(&ctx, vec![]).unwrap_err();
            let text = format!("{error:#}");
            assert!(
                text.contains("the planned record could not be created"),
                "{text}"
            );
            assert!(store::load(&root, "faseela-runit-to-systemd-v1")
                .unwrap()
                .is_none());
            lock::acquire(&root, lock::Mode::Exclusive)
                .unwrap()
                .expect("nothing was activated");
        }
        // nothing is stranded: a retry starts a fresh, re-approved plan
        let outcome = prepare_with(&ctx, vec![]).expect("the retry succeeds");
        assert!(matches!(outcome, PreparationOutcome::Committed(_)));
    }

    #[test]
    fn a_conclusion_that_cannot_be_published_escalates_to_needs_recovery() {
        // Record saves under `migrations/` in a step-less plan: 1 planned,
        // 2 the gate, 3 the awaiting-reboot conclusion. Fail the
        // conclusion: an ending that cannot be made durable is not a
        // verified outcome, so the record is re-declared needs-recovery
        // from the still-durable `preparing` state (§10.3) and the gate
        // stays active.
        let root = crate::testutil::tmpdir("migrate-coord-conclude-wrote");
        let ctx = ctx_with(&root);
        let _armed = arm(crate::adapters::atomic::FailPoint::Wrote, 3, "migrations");
        let (outcome, lines) = prepare_capture(&ctx, vec![]);
        let record = ambiguous(&outcome.expect("the escalation concludes the run"));
        assert_eq!(record.state, MigrationState::NeedsRecovery);
        let on_disk = store::load(&root, record.id.as_str()).unwrap().unwrap();
        assert_eq!(on_disk.state, MigrationState::NeedsRecovery);
        assert!(
            told(&lines).iter().any(|l| l.contains("needs-recovery")),
            "{:?}",
            told(&lines)
        );
        let error = lock::acquire(&root, lock::Mode::Exclusive).unwrap_err();
        assert!(format!("{error:#}").contains("needs-recovery"), "{error:#}");
        // the transaction itself had no failing steps; the migration
        // record is what governs the gate
        let listing = crate::transaction::history::list_records(&root).unwrap();
        assert_eq!(
            listing[0].status().unwrap(),
            crate::transaction::history::TxStatus::Completed
        );
    }

    #[test]
    fn a_transaction_history_failure_does_not_mask_the_migration_outcome() {
        // History records under `history/transactions/*.toml`: 1 the begun
        // transaction's initial record, 2 its final record in
        // `op.finish`. Fail the finish: the record on disk is
        // authoritative for the gate, the migration outcome stands, and
        // the failure is reported through the interaction port — it must
        // not undo or mask the committed preparation.
        let root = crate::testutil::tmpdir("migrate-coord-finish-wrote");
        let ctx = ctx_with(&root);
        let _armed = arm(crate::adapters::atomic::FailPoint::Wrote, 2, "transactions");
        let (outcome, lines) = prepare_capture(&ctx, vec![]);
        let record = committed(&outcome.expect("the migration outcome stands"));
        assert_eq!(record.state, MigrationState::AwaitingReboot);
        let on_disk = store::load(&root, record.id.as_str()).unwrap().unwrap();
        assert_eq!(on_disk.state, MigrationState::AwaitingReboot);
        let error = lock::acquire(&root, lock::Mode::Exclusive).unwrap_err();
        assert!(
            format!("{error:#}").contains("awaiting-reboot"),
            "{error:#}"
        );
        assert!(
            told(&lines)
                .iter()
                .any(|l| l.contains("could not be finalized")),
            "{:?}",
            told(&lines)
        );
    }

    #[test]
    fn a_write_ahead_intent_failure_fails_the_step_before_its_side_effect() {
        // Record saves under `migrations/` with one step: 1 planned,
        // 2 the gate, 3 the step intent. Fail the intent save: the side
        // effect must never run, nothing was committed, the gate clears.
        let root = crate::testutil::tmpdir("migrate-coord-intent-wrote");
        let ctx = ctx_with(&root);
        let (archive, sha) = staged_archive(&root, "sysd", "1");
        let _armed = arm(crate::adapters::atomic::FailPoint::Wrote, 3, "migrations");
        let (outcome, _lines) =
            prepare_capture(&ctx, vec![install_step("sysd", &archive, &sha, false)]);
        let record = compensated(&outcome.expect("the run completes with a failed step"));
        assert_eq!(record.state, MigrationState::Failed);
        let reason = failed_reason(&record.steps[0]);
        assert!(reason.contains("write-ahead intent"), "{reason}");
        assert!(
            crate::adapters::db::Db::new(&root).entry("sysd").unwrap().is_none(),
            "the side effect never ran"
        );
        lock::acquire(&root, lock::Mode::Exclusive)
            .unwrap()
            .expect("nothing was committed, so no gate");
    }

    #[test]
    fn an_outcome_that_cannot_be_persisted_after_a_commit_lands_in_needs_recovery() {
        // Record saves under `migrations/` with one step: 1 planned,
        // 2 the gate, 3 the intent, 4 the outcome. The install committed
        // (the package is in the database); failing the outcome save
        // leaves the interrupted-step shape on disk, and the conclusion
        // must be needs-recovery with the gate active — never `failed`
        // over a changed system.
        let root = crate::testutil::tmpdir("migrate-coord-outcome-wrote");
        let ctx = ctx_with(&root);
        let (archive, sha) = staged_archive(&root, "sysd", "1");
        let _armed = arm(crate::adapters::atomic::FailPoint::Wrote, 4, "migrations");
        let (outcome, _lines) =
            prepare_capture(&ctx, vec![install_step("sysd", &archive, &sha, false)]);
        let record = ambiguous(&outcome.expect("the run completes with a failed step"));
        assert_eq!(record.state, MigrationState::NeedsRecovery);
        let on_disk = store::load(&root, record.id.as_str()).unwrap().unwrap();
        assert_eq!(on_disk.state, MigrationState::NeedsRecovery);
        assert!(
            crate::adapters::db::Db::new(&root).entry("sysd").unwrap().is_some(),
            "the committed install stays committed"
        );
        let error = lock::acquire(&root, lock::Mode::Exclusive).unwrap_err();
        assert!(format!("{error:#}").contains("needs-recovery"), "{error:#}");
        let listing = crate::transaction::history::list_records(&root).unwrap();
        assert_eq!(
            listing[0].status().unwrap(),
            crate::transaction::history::TxStatus::Partial
        );
    }

    #[test]
    fn a_failure_after_the_db_commit_is_classified_committed() {
        // The step reports Err after the installer has committed the
        // package (the natural real-world shape is a failing
        // post-install hook, which cannot run under a test --root as a
        // non-root user; the same shape is injected here by failing the
        // step-result journal write, which happens after the database
        // entry is committed). The outcome says failed, but the database
        // says committed: the classification must read the database
        // truth (§10.3), so the migration lands in needs-recovery with
        // the gate active — never in `failed` with a cleared gate over a
        // changed system.
        let root = crate::testutil::tmpdir("migrate-coord-after-commit");
        let ctx = ctx_with(&root);
        let (archive, sha) = staged_archive(&root, "sysd", "1");
        // journal writes for one step: 1 the transaction state at begin,
        // 2 the step intent, 3 the step result (after the db commit)
        let _armed = arm(crate::adapters::atomic::FailPoint::Wrote, 3, "journal");
        let (outcome, _lines) =
            prepare_capture(&ctx, vec![install_step("sysd", &archive, &sha, false)]);
        let record = ambiguous(&outcome.expect("the run completes with a failed step"));
        assert_eq!(record.state, MigrationState::NeedsRecovery);
        assert_eq!(record.steps.len(), 1);
        assert!(!failed_reason(&record.steps[0]).is_empty());
        assert!(
            crate::adapters::db::Db::new(&root).entry("sysd").unwrap().is_some(),
            "the package was committed before the failure was reported"
        );
        let error = lock::acquire(&root, lock::Mode::Exclusive).unwrap_err();
        assert!(format!("{error:#}").contains("needs-recovery"), "{error:#}");
        // the change was never recorded in the transaction history (its
        // journal write failed), so the bare transaction says failed —
        // the migration record is what keeps the system safe
        let listing = crate::transaction::history::list_records(&root).unwrap();
        assert_eq!(
            listing[0].status().unwrap(),
            crate::transaction::history::TxStatus::Failed
        );
    }

    #[test]
    fn an_interrupt_before_the_first_step_compensates_trivially() {
        // The deferred interrupt surfaces at the between-steps check
        // before any intent was written and before any side effect ran:
        // nothing was committed, the gate clears, the migration is final.
        let root = crate::testutil::tmpdir("migrate-coord-interrupt-first");
        let ctx = ctx_with(&root);
        let (archive, sha) = staged_archive(&root, "sysd", "1");
        let _cleared = Simulated;
        crate::adapters::interrupt::simulate_interrupt();
        let (outcome, _lines) =
            prepare_capture(&ctx, vec![install_step("sysd", &archive, &sha, false)]);
        let record = compensated(&outcome.expect("the deferred interrupt is a step failure"));
        assert_eq!(record.state, MigrationState::Failed);
        assert!(
            record.steps.is_empty(),
            "the interrupt hit before any intent was written"
        );
        assert!(crate::adapters::db::Db::new(&root).entry("sysd").unwrap().is_none());
        lock::acquire(&root, lock::Mode::Exclusive)
            .unwrap()
            .expect("nothing was committed, so no gate");
        let listing = crate::transaction::history::list_records(&root).unwrap();
        assert_eq!(
            listing[0].status().unwrap(),
            crate::transaction::history::TxStatus::Failed
        );
    }

    #[test]
    fn an_interrupt_after_a_commit_concludes_needs_recovery() {
        // The ending split for a deferred interrupt that surfaces after a
        // step already committed (exercised at the conclusion, where the
        // split lives): needs-recovery, gate active, history partial.
        let root = crate::testutil::tmpdir("migrate-coord-interrupt-commit");
        let ctx = ctx_with(&root);
        let mut record = MigrationRecord::new(&definition());
        record.state = MigrationState::Preparing;
        record.steps = vec![StepRecord {
            id: "install-sysd".to_string(),
            intent: Some("install the staged sysd artifact".to_string()),
            outcome: Some(StepOutcome::Succeeded),
        }];
        store::save(&root, &record).unwrap();
        let op = tx_context::begin_operation(
            &root,
            transaction::OperationCommand::Migration,
            std::slice::from_ref(&record.id),
            &[],
        )
        .unwrap();
        let recorder = Recorder::default();
        let ui = recorder.ui();
        let failure =
            anyhow!("interrupted before migration step install-integration (the integration step)");
        let outcome = conclude(&ctx, record, op, Some(&failure), true, &ui)
            .expect("the conclusion completes");
        let record = match &outcome {
            PreparationOutcome::Ambiguous(record) => record.clone(),
            other => panic!("expected Ambiguous, got {other:?}"),
        };
        assert_eq!(record.state, MigrationState::NeedsRecovery);
        let on_disk = store::load(&root, record.id.as_str()).unwrap().unwrap();
        assert_eq!(on_disk.state, MigrationState::NeedsRecovery);
        let error = lock::acquire(&root, lock::Mode::Exclusive).unwrap_err();
        assert!(format!("{error:#}").contains("needs-recovery"), "{error:#}");
        let listing = crate::transaction::history::list_records(&root).unwrap();
        // the synthetic operation carries no recorded package changes, so
        // the bare transaction is finalized as failed; with real recorded
        // changes the same conclusion is Partial (see the mid-plan test)
        assert_eq!(
            listing[0].status().unwrap(),
            crate::transaction::history::TxStatus::Failed
        );
    }

    #[test]
    fn a_planned_record_left_by_a_crash_does_not_hold_the_gate() {
        // Crash window: killed after the planned save, before the gate
        // save. `planned` is deliberately not gate-active (§10.3), so
        // package mutations stay possible while recovery decides what to
        // do with the record — and the begin still refuses to clobber it.
        let root = crate::testutil::tmpdir("migrate-coord-planned-gate");
        let ctx = ctx_with(&root);
        let record = MigrationRecord::new(&definition());
        assert_eq!(record.state, MigrationState::Planned);
        store::save(&root, &record).unwrap();
        lock::acquire(&root, lock::Mode::Exclusive)
            .unwrap()
            .expect("a planned record holds no gate");
        let error = prepare_with(&ctx, vec![]).unwrap_err();
        assert!(format!("{error:#}").contains("already exists"), "{error:#}");
    }

    #[test]
    fn crash_window_records_between_gate_and_conclusion_hold_the_gate() {
        // The shapes a killed process leaves between the gate save and
        // the conclusion — an intent without its outcome, and every step
        // resolved but the conclusion unpublished — both keep the
        // mutation gate active.
        let root = crate::testutil::tmpdir("migrate-coord-window-gates");
        let mut record = MigrationRecord::new(&definition());
        record.state = MigrationState::Preparing;
        record.steps = vec![StepRecord {
            id: "install-sysd".to_string(),
            intent: Some("install the staged sysd artifact".to_string()),
            outcome: None,
        }];
        store::save(&root, &record).unwrap();
        let error = lock::acquire(&root, lock::Mode::Exclusive).unwrap_err();
        assert!(format!("{error:#}").contains("preparing"), "{error:#}");

        let mut resolved = MigrationRecord::new(&definition());
        resolved.state = MigrationState::Preparing;
        resolved.steps = vec![StepRecord {
            id: "install-sysd".to_string(),
            intent: Some("install the staged sysd artifact".to_string()),
            outcome: Some(StepOutcome::Succeeded),
        }];
        store::save(&root, &resolved).unwrap();
        let error = lock::acquire(&root, lock::Mode::Exclusive).unwrap_err();
        assert!(format!("{error:#}").contains("preparing"), "{error:#}");
    }

    #[test]
    fn the_install_postcondition_is_verified_in_the_database() {
        // §10.3 verify: before an outcome is persisted, the planned
        // identity must hold in the actual database, and a plan-pinned
        // digest must be the digest that was installed.
        let root = crate::testutil::tmpdir("migrate-coord-postcondition");
        let ctx = ctx_with(&root);
        let (archive, _sha) = staged_archive(&root, "sysd", "1");
        let operation = |pin: Option<String>| PackageOperation::Install {
            archive: archive.clone(),
            package: "sysd".to_string(),
            expected_sha256: pin,
            reason: InstallReason::Explicit,
            force: false,
            allow_unsigned: true,
            source: "migration-staged".to_string(),
        };
        let hashes = std::collections::HashMap::new();
        // nothing installed: the postcondition fails closed
        let error = verify_install_postcondition(&ctx, &operation(None)).unwrap_err();
        assert!(
            format!("{error:#}").contains("not in the package database"),
            "{error:#}"
        );
        // the planned identity holds
        let db = crate::adapters::db::Db::new(&root);
        db.add(
            &crate::testutil::entry("sysd", "1", "digest-a"),
            &[],
            &hashes,
        )
        .unwrap();
        verify_install_postcondition(&ctx, &operation(None))
            .expect("the planned identity is confirmed");
        // a different installed version is a failed verification
        db.add(
            &crate::testutil::entry("sysd", "2", "digest-b"),
            &[],
            &hashes,
        )
        .unwrap();
        let error = verify_install_postcondition(&ctx, &operation(None)).unwrap_err();
        assert!(
            format!("{error:#}").contains("postcondition failed"),
            "{error:#}"
        );
        // a pinned digest that was not installed is a failed verification
        db.add(
            &crate::testutil::entry("sysd", "1", "digest-a"),
            &[],
            &hashes,
        )
        .unwrap();
        let error =
            verify_install_postcondition(&ctx, &operation(Some("pinned".to_string()))).unwrap_err();
        assert!(format!("{error:#}").contains("digest"), "{error:#}");
    }

    #[test]
    fn the_commit_classification_reads_the_database_truth() {
        // §10.3: a failed step is classified from the actual package
        // database, not from the failing command's claim.
        let root = crate::testutil::tmpdir("migrate-coord-classify");
        let ctx = ctx_with(&root);
        let missing = PackageOperation::Install {
            archive: root.join("stage/missing.nawa.tar.zst"),
            package: "sysd".to_string(),
            expected_sha256: None,
            reason: InstallReason::Explicit,
            force: false,
            allow_unsigned: true,
            source: "migration-staged".to_string(),
        };
        // nothing installed and nothing readable: not committed
        assert!(!step_committed(&ctx, &missing, None));
        // the package IS in the database (an error after the commit):
        // committed, decided without reading the missing archive
        let db = crate::adapters::db::Db::new(&root);
        db.add(
            &crate::testutil::entry("sysd", "1", "digest-a"),
            &[],
            &std::collections::HashMap::new(),
        )
        .unwrap();
        assert!(step_committed(&ctx, &missing, None));
        // the same state before and after, no pending hook: not committed.
        // The identity comparison uses the real archive digest —
        // `inspect_archive_state` reads it from the archive bytes.
        let (archive, sha) = staged_archive(&root, "sysd", "1");
        let real = PackageOperation::Install {
            archive: archive.clone(),
            package: "sysd".to_string(),
            expected_sha256: None,
            reason: InstallReason::Explicit,
            force: false,
            allow_unsigned: true,
            source: "migration-staged".to_string(),
        };
        let before = crate::testutil::entry("sysd", "1", &sha);
        db.add(&before, &[], &std::collections::HashMap::new())
            .unwrap();
        assert!(!step_committed(&ctx, &real, Some(&before)));
        // the same state but a post-install hook still pending (missing
        // status counts as pending): committed with the hook to retry
        let hooks = root.join("var/lib/nawa/local/sysd/hooks");
        std::fs::create_dir_all(hooks.parent().unwrap()).unwrap();
        std::fs::write(
            &hooks,
            toml::to_string(&archive::Hooks {
                post_install: "true".to_string(),
                pre_remove: String::new(),
                post_remove: String::new(),
            })
            .unwrap(),
        )
        .unwrap();
        assert!(step_committed(&ctx, &real, Some(&before)));
    }
}
