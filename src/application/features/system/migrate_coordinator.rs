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

// Everything in this module awaits its production consumer: the
// `nawa system migrate apply` command (§12.2) — which also brings the
// live-host preflight (§10.4 stage 2) and the real handler's boot steps
// (Phase 6). The use case itself is complete and driven by tests.
#![allow(dead_code)]

use anyhow::{anyhow, bail, Context, Result};
use std::path::PathBuf;

use crate::application::features::package::apply_transaction::install_for_migration;
use crate::application::ports::user_interaction::UserInteraction;
use crate::ctx::Ctx;
use crate::db::InstallReason;
use crate::domain::system::migration::{
    transition, MigrationDefinition, MigrationRecord, StepOutcome, StepRecord, Transition,
};
use crate::lock;
use crate::migration_record as store;
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
/// split the failure by compensation.
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

    // A migration id is unique under a root. An existing record (any
    // state) is never silently overwritten: re-running a failed
    // migration is a recovery/re-plan decision for the recovery slice.
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
    store::save(&ctx.root, &record)?;

    // One real package transaction links the record to history (§11):
    // the requested list names the migration so `nawa history` can show
    // which migration drove the changes.
    let mut op = tx_context::begin_operation(
        &ctx.root,
        transaction::OperationCommand::Migration,
        std::slice::from_ref(&definition.id),
        &[],
    )?;

    // BeginPreparation under our own lock: the gate becomes active now.
    // From here on every ordinary exclusive acquisition under this root
    // is refused — ours continue through the held guard instead.
    record.state = transition(record.state, Transition::BeginPreparation)
        .map_err(anyhow::Error::msg)
        .with_context(|| format!("migration {}", definition.id))?;
    store::save(&ctx.root, &record)?;

    let total = plan.steps.len();
    let mut failure: Option<anyhow::Error> = None;
    for (index, step) in plan.steps.iter().enumerate() {
        // Ctrl+C during the previous step was deferred until that side
        // effect completed; between steps the system is consistent, so
        // stop here. An interrupted step has no outcome in the record
        // (§10.3: reconciled on restart by the recovery slice).
        crate::interrupt::check()?;

        // write-ahead: the intent is durable BEFORE the side effect
        record.steps.push(StepRecord {
            id: step.id.clone(),
            intent: Some(step.intent.clone()),
            outcome: None,
        });
        store::save(&ctx.root, &record)?;

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

        // the outcome is persisted only after the side effect returned
        let last = record
            .steps
            .last_mut()
            .expect("a step record was just pushed");
        match result {
            Ok(()) => {
                last.outcome = Some(StepOutcome::Succeeded);
                store::save(&ctx.root, &record)?;
            }
            Err(error) => {
                let reason = format!("{error:#}");
                last.outcome = Some(StepOutcome::Failed {
                    reason: reason.clone(),
                });
                store::save(&ctx.root, &record)?;
                failure = Some(anyhow!("migration step {} failed: {reason}", step.id));
                break;
            }
        }
    }

    // Split the ending by what was committed (§10.3's compensation rule).
    let committed_any = record
        .steps
        .iter()
        .any(|s| matches!(s.outcome, Some(StepOutcome::Succeeded)));
    let outcome = match failure {
        None => {
            // §10.4 stage 6: every preparation postcondition is checked
            // before `awaiting-reboot`. The skeleton's postconditions
            // are the installer's own per-step guarantees (artifact
            // hash pinning, payload conflict checks, database update);
            // the boot-layout and recovery-route postconditions are the
            // real handler's (Phase 6) and are asserted there before
            // this transition in the full flow.
            record.state = transition(record.state, Transition::PreparationCommitted)
                .map_err(anyhow::Error::msg)
                .with_context(|| format!("migration {}", definition.id))?;
            store::save(&ctx.root, &record)?;
            op.finish(None)?;
            (ui.info)(&format!(
                "migration {} is awaiting-reboot — the package mutation gate stays active until the post-reboot resume (§10.4)",
                definition.id
            ));
            PreparationOutcome::Committed(record)
        }
        Some(error) if !committed_any => {
            // nothing was changed, so nothing needs compensating: the
            // gate clears and the migration is final
            record.state = transition(record.state, Transition::PreparationCompensated)
                .map_err(anyhow::Error::msg)
                .with_context(|| format!("migration {}", definition.id))?;
            store::save(&ctx.root, &record)?;
            op.finish(Some(&error))?;
            (ui.warn)(&format!(
                "migration {} failed before any step changed the system: {error:#}",
                definition.id
            ));
            PreparationOutcome::Compensated(record)
        }
        Some(error) => {
            // earlier steps committed and no verified compensation
            // exists yet: fail safe into needs-recovery (the gate stays
            // active; automatic compensation arrives with the recovery
            // slice)
            record.state = transition(record.state, Transition::PreparationAmbiguous)
                .map_err(anyhow::Error::msg)
                .with_context(|| format!("migration {}", definition.id))?;
            store::save(&ctx.root, &record)?;
            op.finish(Some(&error))?;
            (ui.warn)(&format!(
                "migration {} stopped in needs-recovery (the mutation gate stays active): {error:#}",
                definition.id
            ));
            PreparationOutcome::Ambiguous(record)
        }
    };
    // the guard drops with this return: the flock releases; if the
    // migration is gate-active, every ordinary package mutation is
    // refused from now on (§10.3) until the record reaches a terminal
    // state through resume/recovery
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive;
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
        Ctx::from_config(&crate::config::Config {
            root: root.to_path_buf(),
            recipes: root.join("recipes"),
            builddir: root.join("build"),
            profiles: root.join("profiles"),
            jobs: None,
            repo: None,
            install_mode: crate::config::InstallMode::Auto,
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
        let digest = crate::checksum::sha256_of_file(&out).unwrap();
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
        let db = crate::db::Db::new(&root);
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
        let db = crate::db::Db::new(&root);
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
        let db = crate::db::Db::new(&root);
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
}
