//! The migration domain model (design §10): a migration is a named,
//! versioned transition from a known system state to a known target state —
//! not an arbitrary package hook.
//!
//! This module owns the pure pieces of §10.1–§10.3: the typed and validated
//! [`MigrationDefinition`], the trusted built-in [`HandlerId`] allowlist
//! (§10.2), the durable [`MigrationState`] state machine with its legal
//! [`Transition`]s, and the [`MigrationRecord`] model that persists it. No
//! I/O, no serde, no adapter types: the record's TOML text format and the
//! directory layout belong to the record store (`migration_record`, a
//! top-level module — see its header for the placement rationale; the
//! durable record lives under the selected target root's state directory).
//!
//! §10.3's mutation-gate semantics are part of this model, because they are
//! policy, not I/O: [`MigrationState::keeps_gate`] lists the states during
//! which every Nawa-owned package-mutating command must stop. The check's
//! wiring into the command entry points is the next slice.

// The migration engine lands stage by stage (§10.4): the definition, state
// machine and record model are ready now, while the coordinator, stages and
// CLI commands are later Phase 5 slices — so `dead_code` stays allowed at
// module scope until those consumers exist.
#![allow(dead_code)]

use crate::domain::system::capability::Capability;

/// The pinned record/definition schema version. Fail closed on anything
/// else: a newer record is never silently misread (§10.3: a *versioned*
/// state record).
pub(crate) const SCHEMA_VERSION: u32 = 1;
/// The trusted built-in migration handlers (§10.2). Selected only by an
/// allowlisted, typed handler ID: a repository must not be able to provide
/// an arbitrary executable path or shell command for root to run, and
/// migration metadata may *describe* the handler but can never grant new
/// privilege. New handlers require a separate, justified security design —
/// not a new string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HandlerId {
    /// the first Faseela runit→systemd handler (§10.1's example; its
    /// boot/initramfs internals are Phase 6, blocked on B5/B6)
    FaseelaInitRunitToSystemd,
}

impl HandlerId {
    /// The stable handler identifier as written in definitions and records.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            HandlerId::FaseelaInitRunitToSystemd => "faseela.init.runit_to_systemd",
        }
    }

    /// Strict allowlist parse: anything not spelled exactly like a built-in
    /// handler is refused — including paths and shell commands (§10.2).
    pub(crate) fn parse(raw: &str) -> Result<HandlerId, String> {
        match raw {
            "faseela.init.runit_to_systemd" => Ok(HandlerId::FaseelaInitRunitToSystemd),
            other => Err(format!(
                "unknown migration handler {other:?}: handlers are built in and selected only by an allowlisted ID (§10.2)"
            )),
        }
    }
}

/// Validate a migration id. The id becomes a file name under the record
/// directory and appears in logs and the journal, so it is normalized
/// strictly: non-empty, ≤ 128 bytes, lowercase `a-z0-9._-` with an
/// alphanumeric first character (which also rules out `.` and `..`).
pub(crate) fn check_id(id: &str) -> Result<(), String> {
    let valid = !id.is_empty()
        && id.len() <= 128
        && id
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'));
    if valid {
        Ok(())
    } else {
        Err(format!(
            "invalid migration id {id:?} (allowed: lowercase letters, digits and . _ -, ≤ 128 bytes, alphanumeric first character)"
        ))
    }
}

/// The typed, versioned migration definition (§10.1): a conceptual
/// declaration bound to a trusted built-in handler. Validation is fail
/// closed like the metadata chain: a definition that names an out-of-scope
/// transition or an unplanned handler pairing is a construction error,
/// never silently accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MigrationDefinition {
    /// stable migration id (validated by [`check_id`])
    pub(crate) id: String,
    /// the definition-contract version this declaration follows
    pub(crate) version: u32,
    /// the capability the machine must be at before the migration runs
    pub(crate) from: Capability,
    /// the capability the migration moves the machine to
    pub(crate) to: Capability,
    /// the trusted built-in handler that owns boot/initramfs preparation
    pub(crate) handler: HandlerId,
    /// whether the transition requires a reboot to complete
    pub(crate) requires_reboot: bool,
}

impl MigrationDefinition {
    /// Build and validate a definition (§10.1, §10.2). The `from`/`to`
    /// identities are limited to the provider-selection scope of §6
    /// (`init-system=runit` / `init-system=systemd`), the transition must
    /// actually move somewhere, and the handler binding must match: the
    /// runit→systemd handler owns exactly the runit→systemd transition and
    /// requires a reboot (§10.4: Nawa exits with a pending-migration
    /// status; it does not reboot automatically).
    pub(crate) fn new(
        id: &str,
        version: u32,
        from: Capability,
        to: Capability,
        handler: HandlerId,
        requires_reboot: bool,
    ) -> Result<MigrationDefinition, String> {
        check_id(id)?;
        if version == 0 {
            return Err(format!("migration {id}: definition version must be ≥ 1"));
        }
        for cap in [&from, &to] {
            if !cap.is_init_system() {
                return Err(format!(
                    "migration {id}: capability {} is outside the init-system scope (§6)",
                    cap.to_key()
                ));
            }
            if !matches!(cap.identity.as_str(), "runit" | "systemd") {
                return Err(format!(
                    "migration {id}: provider selection is limited to runit and systemd (§6), got {}",
                    cap.to_key()
                ));
            }
        }
        if from == to {
            return Err(format!(
                "migration {id}: from and to are both {} — a migration must move somewhere",
                from.to_key()
            ));
        }
        match handler {
            HandlerId::FaseelaInitRunitToSystemd => {
                if from.to_key() != "init-system=runit" || to.to_key() != "init-system=systemd" {
                    return Err(format!(
                        "migration {id}: handler {} owns the init-system=runit → init-system=systemd transition, not {} → {}",
                        handler.as_str(),
                        from.to_key(),
                        to.to_key()
                    ));
                }
                if !requires_reboot {
                    return Err(format!(
                        "migration {id}: handler {} requires a reboot to verify the new init (§10.4)",
                        handler.as_str()
                    ));
                }
            }
        }
        Ok(MigrationDefinition {
            id: id.to_string(),
            version,
            from,
            to,
            handler,
            requires_reboot,
        })
    }
}

/// The durable migration states (§10.3's table). Serialized as the exact
/// lowercase names of that table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MigrationState {
    /// live-host plan passed preflight and explicit approval; the record is
    /// created immediately before the mutation gate (§10.3)
    Planned,
    /// the migration owns a persistent mutation gate while package and boot
    /// preparation runs
    Preparing,
    /// preparation is committed; the new init has not yet been verified
    /// after reboot
    AwaitingReboot,
    /// the live host has rebooted and init/boot postconditions are being
    /// checked
    Verifying,
    /// target state is verified and finalization is complete
    Completed,
    /// a step failed, compensation restored the known old bootable/package
    /// state, and no migration gate remains
    Failed,
    /// verification failed, but a documented rollback action is available
    RollbackRequired,
    /// state is ambiguous or automatic safe continuation/rollback is
    /// impossible
    NeedsRecovery,
}

impl MigrationState {
    /// The exact §10.3 table name.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            MigrationState::Planned => "planned",
            MigrationState::Preparing => "preparing",
            MigrationState::AwaitingReboot => "awaiting-reboot",
            MigrationState::Verifying => "verifying",
            MigrationState::Completed => "completed",
            MigrationState::Failed => "failed",
            MigrationState::RollbackRequired => "rollback-required",
            MigrationState::NeedsRecovery => "needs-recovery",
        }
    }

    /// Strict parse of the §10.3 table name; anything else is an error,
    /// never silently misread.
    pub(crate) fn parse(raw: &str) -> Result<MigrationState, String> {
        for state in [
            MigrationState::Planned,
            MigrationState::Preparing,
            MigrationState::AwaitingReboot,
            MigrationState::Verifying,
            MigrationState::Completed,
            MigrationState::Failed,
            MigrationState::RollbackRequired,
            MigrationState::NeedsRecovery,
        ] {
            if state.as_str() == raw {
                return Ok(state);
            }
        }
        Err(format!(
            "unknown migration state {raw:?} (§10.3 states: planned, preparing, awaiting-reboot, verifying, completed, failed, rollback-required, needs-recovery)"
        ))
    }

    /// The persistent mutation gate is active in exactly the states §10.3
    /// lists: `preparing`, `awaiting-reboot`, `verifying`,
    /// `rollback-required`, `needs-recovery`. `planned` is deliberately not
    /// gate-active: its record is created immediately *before* the gate is
    /// taken, and `BeginPreparation` is the transition that acquires it.
    pub(crate) fn keeps_gate(self) -> bool {
        matches!(
            self,
            MigrationState::Preparing
                | MigrationState::AwaitingReboot
                | MigrationState::Verifying
                | MigrationState::RollbackRequired
                | MigrationState::NeedsRecovery
        )
    }

    /// Terminal: the gate is cleared (verified completion, or a completed
    /// verified rollback recorded as `failed`). No transition leaves these
    /// states — a retry is a new, re-approved plan (§10.4: a stale plan is
    /// rejected and must be regenerated/reapproved).
    pub(crate) fn is_terminal(self) -> bool {
        matches!(self, MigrationState::Completed | MigrationState::Failed)
    }
}

/// One legal transition of the §10.3 state machine. Each variant carries
/// the precondition the coordinator must have established *before* asking
/// for the transition; the record store persists the new state only after
/// the event's side effects are durable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Transition {
    /// preflight passed, explicit approval given, exclusive lock and
    /// mutation gate acquired (§10.4 stage 5) — `planned → preparing`
    BeginPreparation,
    /// every preparation postcondition checked and the previous boot/
    /// recovery path retained (§10.4 stage 6) — `preparing → awaiting-reboot`
    PreparationCommitted,
    /// preparation failed; all committed package/boot changes were safely
    /// compensated, so the gate can clear — `preparing → failed`
    PreparationCompensated,
    /// preparation failed and automatic safe compensation is impossible —
    /// `preparing → needs-recovery` (gate stays active)
    PreparationAmbiguous,
    /// the machine rebooted and an explicit resume begins verification
    /// (§10.4 stage 7) — `awaiting-reboot → verifying`
    BeginVerification,
    /// PID 1 and the boot/init postconditions verified (no service state
    /// is queried, §10) — `verifying → completed`
    VerificationSucceeded,
    /// verification failed and a documented rollback action is available —
    /// `verifying → rollback-required`
    VerificationNeedsRollback,
    /// the rollback ran and the old init/boot state was verified again
    /// (§10.3: verify before recording failure and clearing the gate) —
    /// `rollback-required → failed`
    RollbackVerified,
    /// an outcome is ambiguous and automatic safe continuation/rollback is
    /// impossible — from any gate-active state to `needs-recovery`
    Ambiguous,
}

impl Transition {
    /// The §10.3 prose name, for error messages.
    fn describe(self) -> &'static str {
        match self {
            Transition::BeginPreparation => "begin preparation",
            Transition::PreparationCommitted => "commit preparation",
            Transition::PreparationCompensated => "record compensated preparation failure",
            Transition::PreparationAmbiguous => "record ambiguous preparation failure",
            Transition::BeginVerification => "begin post-reboot verification",
            Transition::VerificationSucceeded => "record verified completion",
            Transition::VerificationNeedsRollback => "enter rollback-required",
            Transition::RollbackVerified => "record verified rollback",
            Transition::Ambiguous => "record ambiguous outcome",
        }
    }
}

/// Apply one transition to a state (§10.3). The legal forward path is
/// `planned → preparing → awaiting-reboot → verifying → completed`; the
/// side exits follow the table's rules. Anything else is an error naming
/// the current state — callers must never guess past the machine.
pub(crate) fn transition(
    state: MigrationState,
    event: Transition,
) -> Result<MigrationState, String> {
    let next = match (state, event) {
        (MigrationState::Planned, Transition::BeginPreparation) => MigrationState::Preparing,
        (MigrationState::Preparing, Transition::PreparationCommitted) => {
            MigrationState::AwaitingReboot
        }
        (MigrationState::Preparing, Transition::PreparationCompensated) => MigrationState::Failed,
        (MigrationState::Preparing, Transition::PreparationAmbiguous) => {
            MigrationState::NeedsRecovery
        }
        (MigrationState::AwaitingReboot, Transition::BeginVerification) => {
            MigrationState::Verifying
        }
        (MigrationState::Verifying, Transition::VerificationSucceeded) => MigrationState::Completed,
        (MigrationState::Verifying, Transition::VerificationNeedsRollback) => {
            MigrationState::RollbackRequired
        }
        (MigrationState::RollbackRequired, Transition::RollbackVerified) => MigrationState::Failed,
        // Ambiguity is reachable from every gate-active state (§10.3: any
        // ambiguous outcome becomes needs-recovery and keeps the gate).
        (
            MigrationState::Preparing
            | MigrationState::AwaitingReboot
            | MigrationState::Verifying
            | MigrationState::RollbackRequired,
            Transition::Ambiguous,
        ) => MigrationState::NeedsRecovery,
        (state, event) => {
            return Err(format!(
                "illegal migration transition: cannot {} from {} (§10.3)",
                event.describe(),
                state.as_str()
            ))
        }
    };
    Ok(next)
}

/// One write-ahead migration step (§10.3, §11): the intent is persisted
/// before the side effect and the outcome after it; a missing outcome
/// means an intent without result — an interrupted step, reconciled into a
/// durable state on restart after inspecting actual system state (§10.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StepRecord {
    /// stable step id within the migration, in declaration order
    pub(crate) id: String,
    /// the intended side effect, persisted *before* it runs
    pub(crate) intent: Option<String>,
    /// the observed result, persisted after the side effect
    pub(crate) outcome: Option<StepOutcome>,
}

/// The outcome of one migration step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StepOutcome {
    /// the side effect completed and its postcondition holds
    Succeeded,
    /// the side effect failed; the reason is persisted for the journal
    /// (§11) and recovery instructions
    Failed { reason: String },
}

/// The durable migration record (§10.3): one versioned file per migration
/// under the selected target root's state directory. The record is rooted
/// at the target root — never the host — and the durable `planned` record
/// is created only after the live-host apply preflight and explicit
/// authorization succeed (offline planning stays read-only). The text
/// format and publication mechanics live in the application-layer store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MigrationRecord {
    /// the record schema, pinned to [`SCHEMA_VERSION`]
    pub(crate) schema: u32,
    /// the migration this record belongs to
    pub(crate) id: String,
    /// the definition-contract version the migration was created from
    /// (§11: migration ID and definition version)
    pub(crate) definition_version: u32,
    /// the source capability (§11: source provider)
    pub(crate) from: Capability,
    /// the target capability (§11: target provider)
    pub(crate) to: Capability,
    /// the trusted built-in handler executing the migration
    pub(crate) handler: HandlerId,
    /// the current durable state
    pub(crate) state: MigrationState,
    /// ordered write-ahead steps (§10.3: intent before, result after)
    pub(crate) steps: Vec<StepRecord>,
}

impl MigrationRecord {
    /// The fresh record of an approved plan: `planned`, no steps yet.
    pub(crate) fn new(definition: &MigrationDefinition) -> MigrationRecord {
        MigrationRecord {
            schema: SCHEMA_VERSION,
            id: definition.id.clone(),
            definition_version: definition.version,
            from: definition.from.clone(),
            to: definition.to.clone(),
            handler: definition.handler,
            state: MigrationState::Planned,
            steps: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cap(key: &str) -> Capability {
        Capability::parse(key).unwrap()
    }

    fn runit_to_systemd() -> MigrationDefinition {
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

    #[test]
    fn the_section_10_1_example_definition_is_valid() {
        let d = runit_to_systemd();
        assert_eq!(d.id, "faseela-runit-to-systemd-v1");
        assert_eq!(d.version, 1);
        assert_eq!(d.from.to_key(), "init-system=runit");
        assert_eq!(d.to.to_key(), "init-system=systemd");
        assert_eq!(d.handler, HandlerId::FaseelaInitRunitToSystemd);
        assert!(d.requires_reboot);
    }

    #[test]
    fn definitions_are_validated_fail_closed() {
        // file-name-unsafe or non-normalized ids
        let long = "x".repeat(129);
        for id in [
            "",
            "Faseela",
            "has space",
            "a/b",
            "../evil",
            "-leading",
            long.as_str(),
        ] {
            let e = MigrationDefinition::new(
                id,
                1,
                cap("init-system=runit"),
                cap("init-system=systemd"),
                HandlerId::FaseelaInitRunitToSystemd,
                true,
            )
            .unwrap_err();
            assert!(e.contains("invalid migration id"), "{id}: {e}");
        }
        // version
        let e = MigrationDefinition::new(
            "m1",
            0,
            cap("init-system=runit"),
            cap("init-system=systemd"),
            HandlerId::FaseelaInitRunitToSystemd,
            true,
        )
        .unwrap_err();
        assert!(e.contains("version must be ≥ 1"), "{e}");
        // out-of-scope capability domain
        let e = MigrationDefinition::new(
            "m1",
            1,
            cap("session-manager=elogind"),
            cap("init-system=systemd"),
            HandlerId::FaseelaInitRunitToSystemd,
            true,
        )
        .unwrap_err();
        assert!(e.contains("outside the init-system scope"), "{e}");
        // out-of-scope identity (§6: only runit and systemd)
        let e = MigrationDefinition::new(
            "m1",
            1,
            cap("init-system=openrc"),
            cap("init-system=systemd"),
            HandlerId::FaseelaInitRunitToSystemd,
            true,
        )
        .unwrap_err();
        assert!(e.contains("limited to runit and systemd"), "{e}");
        // a migration must move somewhere
        let e = MigrationDefinition::new(
            "m1",
            1,
            cap("init-system=runit"),
            cap("init-system=runit"),
            HandlerId::FaseelaInitRunitToSystemd,
            true,
        )
        .unwrap_err();
        assert!(e.contains("must move somewhere"), "{e}");
        // handler/transition binding
        let e = MigrationDefinition::new(
            "m1",
            1,
            cap("init-system=systemd"),
            cap("init-system=runit"),
            HandlerId::FaseelaInitRunitToSystemd,
            true,
        )
        .unwrap_err();
        assert!(e.contains("owns the init-system=runit"), "{e}");
        // the runit→systemd handler requires the reboot boundary
        let e = MigrationDefinition::new(
            "m1",
            1,
            cap("init-system=runit"),
            cap("init-system=systemd"),
            HandlerId::FaseelaInitRunitToSystemd,
            false,
        )
        .unwrap_err();
        assert!(e.contains("requires a reboot"), "{e}");
    }

    #[test]
    fn ids_that_look_like_paths_are_rejected() {
        assert!(check_id(".").is_err());
        assert!(check_id("..").is_err());
        assert!(check_id("a/../b").is_err());
        assert!(check_id("UPPER").is_err());
        assert!(check_id("faseela-runit-to-systemd-v1").is_ok());
        assert!(check_id("m_1.2-v3").is_ok());
    }

    #[test]
    fn handler_ids_are_an_allowlist_not_paths() {
        assert_eq!(
            HandlerId::parse("faseela.init.runit_to_systemd").unwrap(),
            HandlerId::FaseelaInitRunitToSystemd
        );
        for raw in [
            "/usr/libexec/migrate",
            "./migrate.sh",
            "sh -c 'evil'",
            "faseela.init.runit_to_systemd ",
            "",
            "future.handler",
        ] {
            let e = HandlerId::parse(raw).unwrap_err();
            assert!(
                e.contains("built in and selected only by an allowlisted ID"),
                "{raw:?}: {e}"
            );
        }
    }

    #[test]
    fn states_roundtrip_through_the_section_10_3_names() {
        for state in [
            MigrationState::Planned,
            MigrationState::Preparing,
            MigrationState::AwaitingReboot,
            MigrationState::Verifying,
            MigrationState::Completed,
            MigrationState::Failed,
            MigrationState::RollbackRequired,
            MigrationState::NeedsRecovery,
        ] {
            assert_eq!(MigrationState::parse(state.as_str()).unwrap(), state);
        }
        assert!(MigrationState::parse("prepped").is_err());
        assert!(MigrationState::parse("").is_err());
    }

    #[test]
    fn the_legal_forward_path_walks_to_completed() {
        let mut state = MigrationState::Planned;
        for event in [
            Transition::BeginPreparation,
            Transition::PreparationCommitted,
            Transition::BeginVerification,
            Transition::VerificationSucceeded,
        ] {
            state = transition(state, event).unwrap();
        }
        assert_eq!(state, MigrationState::Completed);
        assert!(!state.keeps_gate());
        assert!(state.is_terminal());
    }

    #[test]
    fn preparing_failures_split_by_whether_compensation_succeeded() {
        // compensated → failed, gate cleared (§10.3: a failure in preparing
        // may become failed only after all committed changes are
        // compensated)
        let failed = transition(
            MigrationState::Preparing,
            Transition::PreparationCompensated,
        )
        .unwrap();
        assert_eq!(failed, MigrationState::Failed);
        assert!(!failed.keeps_gate());
        assert!(failed.is_terminal());

        // uncompensated/ambiguous → needs-recovery, gate stays active
        let ambiguous =
            transition(MigrationState::Preparing, Transition::PreparationAmbiguous).unwrap();
        assert_eq!(ambiguous, MigrationState::NeedsRecovery);
        assert!(ambiguous.keeps_gate());
        assert!(!ambiguous.is_terminal());
    }

    #[test]
    fn post_reboot_verification_failure_enters_rollback_then_failed() {
        let rollback = transition(
            MigrationState::Verifying,
            Transition::VerificationNeedsRollback,
        )
        .unwrap();
        assert_eq!(rollback, MigrationState::RollbackRequired);
        assert!(rollback.keeps_gate(), "rollback-required keeps the gate");

        // the gate clears only after the old init/boot state is verified
        let failed = transition(rollback, Transition::RollbackVerified).unwrap();
        assert_eq!(failed, MigrationState::Failed);
        assert!(!failed.keeps_gate());
    }

    #[test]
    fn ambiguity_is_reachable_from_every_gate_active_state() {
        for state in [
            MigrationState::Preparing,
            MigrationState::AwaitingReboot,
            MigrationState::Verifying,
            MigrationState::RollbackRequired,
        ] {
            let next = transition(state, Transition::Ambiguous).unwrap();
            assert_eq!(next, MigrationState::NeedsRecovery, "from {state:?}");
            assert!(next.keeps_gate());
        }
        // terminal states have no exits — including ambiguity
        for state in [MigrationState::Completed, MigrationState::Failed] {
            assert!(transition(state, Transition::Ambiguous).is_err());
        }
    }

    #[test]
    fn illegal_transitions_are_named_errors() {
        // skipping the reboot boundary
        let e = transition(MigrationState::Planned, Transition::BeginVerification).unwrap_err();
        assert!(
            e.contains("cannot begin post-reboot verification from planned"),
            "{e}"
        );
        // verifying again without a commit
        let e = transition(
            MigrationState::AwaitingReboot,
            Transition::VerificationSucceeded,
        )
        .unwrap_err();
        assert!(
            e.contains("cannot record verified completion from awaiting-reboot"),
            "{e}"
        );
        // completing a rollback that was never requested
        let e = transition(MigrationState::Verifying, Transition::RollbackVerified).unwrap_err();
        assert!(
            e.contains("cannot record verified rollback from verifying"),
            "{e}"
        );
        // terminal states are final
        let e = transition(MigrationState::Completed, Transition::BeginPreparation).unwrap_err();
        assert!(e.contains("cannot begin preparation from completed"), "{e}");
        // the gate can only be taken once, from planned
        let e = transition(MigrationState::Verifying, Transition::BeginPreparation).unwrap_err();
        assert!(e.contains("cannot begin preparation from verifying"), "{e}");
    }

    #[test]
    fn the_gate_policy_matches_the_section_10_3_list() {
        let active = [
            MigrationState::Preparing,
            MigrationState::AwaitingReboot,
            MigrationState::Verifying,
            MigrationState::RollbackRequired,
            MigrationState::NeedsRecovery,
        ];
        let inactive = [
            MigrationState::Planned,
            MigrationState::Completed,
            MigrationState::Failed,
        ];
        for state in active {
            assert!(state.keeps_gate(), "{state:?} must keep the gate");
        }
        for state in inactive {
            assert!(!state.keeps_gate(), "{state:?} must not keep the gate");
        }
    }

    #[test]
    fn a_fresh_record_is_planned_with_the_definition_bound() {
        let d = runit_to_systemd();
        let record = MigrationRecord::new(&d);
        assert_eq!(record.schema, SCHEMA_VERSION);
        assert_eq!(record.id, d.id);
        assert_eq!(record.definition_version, d.version);
        assert_eq!(record.from, d.from);
        assert_eq!(record.to, d.to);
        assert_eq!(record.handler, d.handler);
        assert_eq!(record.state, MigrationState::Planned);
        assert!(record.steps.is_empty());
    }

    #[test]
    fn steps_carry_write_ahead_intents_and_outcomes() {
        // §10.3: intent persisted before the side effect, outcome after; an
        // intent without outcome is an interrupted step
        let interrupted = StepRecord {
            id: "prepare-boot".to_string(),
            intent: Some("prepare boot/initramfs integration".to_string()),
            outcome: None,
        };
        assert_eq!(interrupted.outcome, None);
        let succeeded = StepRecord {
            id: "stage-artifacts".to_string(),
            intent: Some("fetch and verify package artifacts".to_string()),
            outcome: Some(StepOutcome::Succeeded),
        };
        let failed = StepRecord {
            id: "prepare-boot".to_string(),
            intent: Some("prepare boot/initramfs integration".to_string()),
            outcome: Some(StepOutcome::Failed {
                reason: "initramfs generator refused the systemd init".to_string(),
            }),
        };
        assert_ne!(succeeded, failed);
    }
}
