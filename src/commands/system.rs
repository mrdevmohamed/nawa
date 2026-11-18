//! `nawa system migrate` presentation (design §12.2): the read-only
//! migration-record inspection commands. The use case
//! (`application::features::system::migrate_inspect`) decides what exists
//! and what failed; this module decides how it is printed, over typed
//! values so every rendering rule is unit-testable.
//!
//! All three commands are strictly read-only (§10.3): they never create or
//! modify a record, never transition a state and never execute a handler.
//! The shared lock is already held (the dispatch whole-command lock), so a
//! listing is one coherent snapshot of the record store.

use anyhow::{anyhow, Result};
use std::path::Path;

use crate::application::features::system::migrate_inspect as inspect;
use crate::cli::{MigrateCmd, SystemCmd};
use crate::ctx::Ctx;
use crate::domain::system::migration::{MigrationRecord, StepOutcome, StepRecord};
use crate::migration_record::ActiveGate;

/// `nawa system migrate [list|show|status]`
pub(crate) fn cmd_system(ctx: &Ctx, action: SystemCmd) -> Result<()> {
    match action {
        SystemCmd::Migrate { action } => cmd_migrate(&ctx.root, action),
    }
}

fn cmd_migrate(root: &Path, action: MigrateCmd) -> Result<()> {
    match action {
        MigrateCmd::List => print_list(root),
        MigrateCmd::Show { id } => print_show(root, &id),
        MigrateCmd::Status => print_status(root),
    }
}

fn print_list(root: &Path) -> Result<()> {
    let listing = inspect::list(root)?;
    if listing.is_empty() {
        // §12.2: an empty store is a stated result — no migration record
        // exists under this root, so none was ever started (terminal
        // records would still be listed here)
        crate::nawa_info!(
            "no migration record exists under {} — no migration has ever been started under this root",
            crate::migration_record::migrations_dir(root).display()
        );
        return Ok(());
    }
    for line in render_list(&listing.records) {
        println!("{line}");
    }
    if !listing.unreadable.is_empty() {
        // the readable records are shown above; the unreadable ones are a
        // real defect the user must repair, so the command fails loudly
        let mut error = format!(
            "{} migration record(s) cannot be read:",
            listing.unreadable.len()
        );
        for bad in &listing.unreadable {
            error.push_str(&format!("\n  {}: {}", bad.id, bad.cause));
        }
        return Err(anyhow!("{error}"));
    }
    Ok(())
}

fn print_show(root: &Path, id: &str) -> Result<()> {
    let record = inspect::show(root, id)?;
    for line in render_show(&record) {
        println!("{line}");
    }
    Ok(())
}

fn print_status(root: &Path) -> Result<()> {
    let status = inspect::status(root);
    for line in render_status(&status) {
        println!("{line}");
    }
    // informational (§12.2): the answer is the report, not the exit code
    Ok(())
}

/// The `list` table: one row per record, in the deterministic (sorted) id
/// order the use case returned. Column layout mirrors `nawa history`.
fn render_list(records: &[MigrationRecord]) -> Vec<String> {
    let mut lines = vec![format!(
        "{:<34}  {:<17}  {:<19}  {:<21}  DEF",
        "MIGRATION ID", "STATE", "FROM", "TO"
    )];
    for record in records {
        lines.push(format!(
            "{:<34}  {:<17}  {:<19}  {:<21}  {}",
            record.id,
            record.state.as_str(),
            record.from.to_key(),
            record.to.to_key(),
            record.definition_version
        ));
    }
    lines
}

/// The full record view (`show`): every durable field plus the ordered
/// write-ahead steps. The reboot requirement is derived from the trusted
/// handler binding — the record persists no separate flag (§10.1/§10.2).
fn render_show(record: &MigrationRecord) -> Vec<String> {
    let mut lines = vec![
        format!("Migration {}", record.id),
        format!("  Record schema:      {}", record.schema),
        format!("  Definition version: {}", record.definition_version),
        format!("  Source capability:  {}", record.from.to_key()),
        format!("  Target capability:  {}", record.to.to_key()),
        format!("  State:              {}", record.state.as_str()),
        format!("  Handler:            {}", record.handler.as_str()),
        format!(
            "  Requires reboot:    {} (the trusted handler owns the reboot boundary, §10.4)",
            yes_no(record.requires_reboot())
        ),
    ];
    if record.steps.is_empty() {
        lines.push("  Steps:              none (nothing has been executed yet)".to_string());
        return lines;
    }
    lines.push(format!("  Steps:              {}", record.steps.len()));
    for (index, step) in record.steps.iter().enumerate() {
        lines.extend(render_step(index + 1, step));
    }
    lines
}

/// One write-ahead step (§10.3): the intent is persisted before the side
/// effect, the outcome after it. An intent without an outcome is an
/// *interrupted* step — it must never read as a success (§10.3: it is
/// reconciled into a durable state on restart, after inspecting the
/// actual system state).
fn render_step(number: usize, step: &StepRecord) -> Vec<String> {
    let mut lines = vec![format!("    {number}. {}", step.id)];
    match &step.intent {
        Some(intent) => lines.push(format!("       intent:  {intent}")),
        None => lines.push("       intent:  none recorded".to_string()),
    }
    match &step.outcome {
        Some(StepOutcome::Succeeded) => lines.push("       outcome: succeeded".to_string()),
        Some(StepOutcome::Failed { reason }) => {
            lines.push(format!("       outcome: failed — {reason}"))
        }
        None => lines.push(
            "       outcome: INTERRUPTED — the write-ahead intent has no recorded outcome (§10.3: reconciled on restart)"
                .to_string(),
        ),
    }
    lines
}

/// The gate summary (`status`): whether package mutations are blocked and
/// by what. Exit code 0 in every case — the report is the answer (§12.2);
/// the machine-checkable refusal already exists as the gate's exit-1
/// error on mutating commands.
fn render_status(status: &inspect::Status) -> Vec<String> {
    let mut lines = vec![
        "Migration status".to_string(),
        format!("  Records:            {}", status.dir.display()),
    ];
    if let Some(error) = &status.scan_error {
        lines.push(
            "  Package mutations:  blocked fail-closed — the record state cannot be verified (§10.3)"
                .to_string(),
        );
        lines.push(format!("    {error}"));
        return lines;
    }
    if status.blocked_by_gate() {
        lines.push(
            "  Package mutations:  blocked by the persistent migration gate (§10.3)".to_string(),
        );
        for gate in &status.gates {
            lines.extend(render_gate(gate));
        }
    } else if !status.unreadable.is_empty() {
        // no gate-keeping state could be read, but the gate refuses
        // fail-closed when record state cannot be verified
        lines.push(
            "  Package mutations:  blocked fail-closed — a migration record cannot be read (§10.3)"
                .to_string(),
        );
        for bad in &status.unreadable {
            lines.push(format!("    {}: {}", bad.id, bad.cause));
        }
    } else {
        lines.push("  Package mutations:  not blocked (no active migration gate)".to_string());
    }
    lines
}

fn render_gate(gate: &ActiveGate) -> Vec<String> {
    vec![format!("    {} is {}", gate.id, gate.state.as_str())]
}

fn yes_no(value: bool) -> &'static str {
    if value {
        "yes"
    } else {
        "no"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::system::capability::Capability;
    use crate::domain::system::migration::{
        HandlerId, MigrationDefinition, MigrationState, StepOutcome, StepRecord,
    };

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

    fn record(id: &str, state: MigrationState) -> MigrationRecord {
        let mut record = MigrationRecord::new(&definition(id));
        record.state = state;
        record
    }

    fn joined(lines: &[String]) -> String {
        lines.join("\n")
    }

    #[test]
    fn list_renders_one_row_per_record_with_the_durable_fields() {
        let mut preparing = record("faseela-runit-to-systemd-v1", MigrationState::Preparing);
        preparing.definition_version = 3;
        let completed = record("aaa-older-migration", MigrationState::Completed);
        // the use case returns records sorted by id; rendering keeps that
        // order (deterministic output)
        let lines = render_list(&[completed, preparing]);
        let text = joined(&lines);
        assert!(
            text.starts_with("MIGRATION ID"),
            "the table has a header: {text}"
        );
        assert!(
            text.contains("aaa-older-migration"),
            "rows render in the given order: {text}"
        );
        assert!(text.contains("preparing"), "{text}");
        assert!(text.contains("init-system=runit"), "{text}");
        assert!(text.contains("init-system=systemd"), "{text}");
        assert!(text.contains("3"), "{text}");
        let completed_row = lines
            .iter()
            .find(|l| l.contains("aaa-older-migration"))
            .unwrap();
        assert!(
            completed_row.contains("completed"),
            "the durable state is rendered: {completed_row}"
        );
    }

    #[test]
    fn list_rendering_is_deterministic() {
        let records = vec![
            record("aaa", MigrationState::Planned),
            record("zzz", MigrationState::Failed),
        ];
        assert_eq!(render_list(&records), render_list(&records));
    }

    #[test]
    fn show_renders_every_durable_field_and_the_reboot_binding() {
        let mut awaiting = record(
            "faseela-runit-to-systemd-v1",
            MigrationState::AwaitingReboot,
        );
        awaiting.steps = vec![
            StepRecord {
                id: "stage-artifacts".to_string(),
                intent: Some("fetch and verify package artifacts".to_string()),
                outcome: Some(StepOutcome::Succeeded),
            },
            StepRecord {
                id: "install-packages".to_string(),
                intent: Some("install the systemd packages".to_string()),
                outcome: Some(StepOutcome::Failed {
                    reason: "package payload conflict".to_string(),
                }),
            },
            // intent without outcome: interrupted (§10.3)
            StepRecord {
                id: "prepare-boot".to_string(),
                intent: Some("prepare boot/initramfs integration".to_string()),
                outcome: None,
            },
        ];
        let text = joined(&render_show(&awaiting));
        assert!(
            text.contains("Migration faseela-runit-to-systemd-v1"),
            "{text}"
        );
        assert!(text.contains("Record schema:      1"), "{text}");
        assert!(text.contains("Definition version: 1"), "{text}");
        assert!(
            text.contains("Source capability:  init-system=runit"),
            "{text}"
        );
        assert!(
            text.contains("Target capability:  init-system=systemd"),
            "{text}"
        );
        assert!(
            text.contains("State:              awaiting-reboot"),
            "{text}"
        );
        assert!(
            text.contains("Handler:            faseela.init.runit_to_systemd"),
            "{text}"
        );
        assert!(
            text.contains("Requires reboot:    yes"),
            "the reboot binding is derived from the trusted handler: {text}"
        );
        assert!(text.contains("Steps:              3"), "{text}");
        assert!(text.contains("1. stage-artifacts"), "{text}");
        assert!(
            text.contains("intent:  fetch and verify package artifacts"),
            "{text}"
        );
        assert!(text.contains("outcome: succeeded"), "{text}");
        assert!(text.contains("2. install-packages"), "{text}");
        assert!(
            text.contains("outcome: failed — package payload conflict"),
            "{text}"
        );
    }

    #[test]
    fn an_interrupted_step_is_displayed_as_interrupted_never_successful() {
        let mut record = record("faseela-runit-to-systemd-v1", MigrationState::Preparing);
        record.steps = vec![StepRecord {
            id: "prepare-boot".to_string(),
            intent: Some("prepare boot/initramfs integration".to_string()),
            outcome: None,
        }];
        let text = joined(&render_show(&record));
        assert!(
            text.contains("outcome: INTERRUPTED"),
            "the interrupted step is explicit: {text}"
        );
        assert!(
            !text.contains("outcome: succeeded"),
            "an intent without outcome must not read as a success: {text}"
        );
        assert!(
            text.contains("reconciled on restart"),
            "the recovery semantics are stated: {text}"
        );
    }

    #[test]
    fn a_failed_step_renders_its_persisted_reason() {
        let mut record = record("faseela-runit-to-systemd-v1", MigrationState::NeedsRecovery);
        record.steps = vec![StepRecord {
            id: "prepare-boot".to_string(),
            intent: Some("prepare boot/initramfs integration".to_string()),
            outcome: Some(StepOutcome::Failed {
                reason: "initramfs generator refused the systemd init".to_string(),
            }),
        }];
        let text = joined(&render_show(&record));
        assert!(
            text.contains("outcome: failed — initramfs generator refused the systemd init"),
            "{text}"
        );
        assert!(
            text.contains("State:              needs-recovery"),
            "{text}"
        );
    }

    #[test]
    fn a_planned_record_renders_no_steps() {
        let record = record("faseela-runit-to-systemd-v1", MigrationState::Planned);
        let text = joined(&render_show(&record));
        assert!(
            text.contains("Steps:              none (nothing has been executed yet)"),
            "{text}"
        );
    }

    #[test]
    fn a_step_without_an_intent_is_reported_not_guessed() {
        let mut record = record("faseela-runit-to-systemd-v1", MigrationState::Preparing);
        record.steps = vec![StepRecord {
            id: "hand-written".to_string(),
            intent: None,
            outcome: None,
        }];
        let text = joined(&render_show(&record));
        assert!(text.contains("intent:  none recorded"), "{text}");
        assert!(text.contains("outcome: INTERRUPTED"), "{text}");
    }

    #[test]
    fn status_renders_an_active_gate_with_ids_and_states() {
        let status = inspect::Status {
            dir: Path::new("/mnt/root/var/lib/nawa/migrations").to_path_buf(),
            blocked: true,
            gates: vec![ActiveGate {
                id: "faseela-runit-to-systemd-v1".to_string(),
                state: MigrationState::AwaitingReboot,
            }],
            unreadable: vec![],
            scan_error: None,
        };
        let text = joined(&render_status(&status));
        assert!(text.contains("Migration status"), "{text}");
        assert!(
            text.contains("Records:            /mnt/root/var/lib/nawa/migrations"),
            "the record directory is shown: {text}"
        );
        assert!(
            text.contains("Package mutations:  blocked by the persistent migration gate"),
            "{text}"
        );
        assert!(
            text.contains("faseela-runit-to-systemd-v1 is awaiting-reboot"),
            "the gate names the migration and its durable state: {text}"
        );
    }

    #[test]
    fn status_renders_no_gate_as_not_blocked() {
        let status = inspect::Status {
            dir: Path::new("/mnt/root/var/lib/nawa/migrations").to_path_buf(),
            blocked: false,
            gates: vec![],
            unreadable: vec![],
            scan_error: None,
        };
        let text = joined(&render_status(&status));
        assert!(
            text.contains("Package mutations:  not blocked (no active migration gate)"),
            "{text}"
        );
    }

    #[test]
    fn status_renders_unreadable_records_as_a_fail_closed_block() {
        let status = inspect::Status {
            dir: Path::new("/mnt/root/var/lib/nawa/migrations").to_path_buf(),
            blocked: true,
            gates: vec![],
            unreadable: vec![inspect::UnreadableRecord {
                id: "broken-migration".to_string(),
                cause: "system migration record /root/var/lib/nawa/migrations/broken-migration.toml: not valid TOML".to_string(),
            }],
            scan_error: None,
        };
        let text = joined(&render_status(&status));
        assert!(
            text.contains("Package mutations:  blocked fail-closed"),
            "the fail-closed refusal is the answer: {text}"
        );
        assert!(text.contains("broken-migration.toml"), "{text}");
        assert!(text.contains("not valid TOML"), "{text}");
    }

    #[test]
    fn status_renders_an_unreadable_record_directory_as_the_answer() {
        let status = inspect::Status {
            dir: Path::new("/mnt/root/var/lib/nawa/migrations").to_path_buf(),
            blocked: true,
            gates: vec![],
            unreadable: vec![],
            scan_error: Some(
                "read migration records /mnt/root/var/lib/nawa/migrations: permission denied"
                    .to_string(),
            ),
        };
        let text = joined(&render_status(&status));
        assert!(
            text.contains("the record state cannot be verified"),
            "{text}"
        );
        assert!(text.contains("permission denied"), "{text}");
    }

    #[test]
    fn multiple_gates_render_one_line_each() {
        let status = inspect::Status {
            dir: Path::new("/root/var/lib/nawa/migrations").to_path_buf(),
            blocked: true,
            gates: vec![
                ActiveGate {
                    id: "aaa-migration".to_string(),
                    state: MigrationState::Preparing,
                },
                ActiveGate {
                    id: "zzz-migration".to_string(),
                    state: MigrationState::RollbackRequired,
                },
            ],
            unreadable: vec![],
            scan_error: None,
        };
        let text = joined(&render_status(&status));
        assert!(text.contains("aaa-migration is preparing"), "{text}");
        assert!(
            text.contains("zzz-migration is rollback-required"),
            "{text}"
        );
    }
}
