//! `nawa system` inspection and `nawa system migrate` presentation
//! (design §5, §7, §8 and §12): read-only host/profile inspection, init-system
//! explanation and reconciliation planning, plus migration-record commands
//! and explicit recovery. Application use cases own detection, profile
//! loading and migration state; this module renders their typed results and
//! never infers missing system facts.
//!
//! System status, profile display, plan and explanation are read-only.
//! Migration `list`/`show`/`status` are read-only record inspections; the
//! shared lock gives a coherent snapshot. `apply` and `resume` are separate
//! live-host operations and remain fail-closed at the external boot-handler
//! boundary.

use anyhow::{anyhow, Result};
use std::path::Path;

use crate::application::features::system::detect;
use crate::application::features::system::migrate_apply as apply_use;
use crate::application::features::system::migrate_inspect as inspect;
use crate::application::features::system::migrate_recover as recover;
use crate::application::features::system::profile as system_profile;
use crate::application::ports::user_interaction::UserInteraction;
use crate::cli::{MigrateCmd, SystemCmd, SystemProfileCmd};
use crate::application::context::Ctx;
use crate::domain::system::migration::{MigrationRecord, StepOutcome, StepRecord};
use crate::adapters::migration_record::ActiveGate;

/// `nawa system migrate [list|show|status|resume]`
pub(crate) fn cmd_system(ctx: &Ctx, action: SystemCmd) -> Result<()> {
    match action {
        SystemCmd::Status => print_system_status(&ctx.root),
        SystemCmd::Profile { action: SystemProfileCmd::Show } => print_system_profile(&ctx.root),
        SystemCmd::Plan => print_system_plan(ctx),
        SystemCmd::Explain { topic } => print_system_explanation(&ctx.root, &topic),
        SystemCmd::Migrate { action } => cmd_migrate(ctx, action),
    }
}

fn print_system_status(root: &Path) -> Result<()> {
    let observed = detect::observe(root, Path::new("/proc"));
    for line in render_system_status(&observed) { println!("{line}"); }
    Ok(())
}

fn print_system_profile(root: &Path) -> Result<()> {
    let effective = system_profile::load_effective(root)?;
    for line in render_system_profile(&effective) { println!("{line}"); }
    Ok(())
}

fn print_system_plan(ctx: &Ctx) -> Result<()> {
    // Compose runtime/profile/filesystem evidence and installed package
    // versions once. Repository planning consumes this same snapshot rather
    // than re-reading the local package database in a second use case.
    let snapshot = crate::application::features::system::observation::observe(
        &ctx.root,
        Path::new("/proc"),
    );
    for line in render_system_plan(&snapshot.observed) {
        println!("{line}");
    }

    let installed_versions = match &snapshot.packages {
        crate::application::features::system::observation::PackageInventory::Available(packages) => packages,
        crate::application::features::system::observation::PackageInventory::Unavailable { reason } => {
            println!(
                "  Repository plan: unresolved — installed package inventory could not be read: {reason}"
            );
            return Ok(());
        }
    };

    // The system planner reuses the authenticated transition resolver used by
    // ordinary upgrade previews. It never has a separate provider-selection
    // implementation and it never applies the resulting migration.
    let Some(repository) = ctx.repo.as_deref() else {
        println!("  Repository plan: unresolved — no repository is configured; no package provider was selected.");
        return Ok(());
    };
    let repo_ref = match crate::fetch::RepoRef::parse(repository) {
        Ok(repo_ref) => repo_ref,
        Err(error) => {
            println!(
                "  Repository plan: unresolved — repository configuration is invalid: {error:#}"
            );
            return Ok(());
        }
    };
    match crate::upgrade::preview_system_transition(
        &ctx.root,
        &repo_ref,
        &snapshot.observed,
        installed_versions,
    ) {
        Ok(Some(lines)) => {
            println!();
            for line in lines {
                println!("{line}");
            }
        }
        Ok(None) => println!("  Repository plan: no separate init-system transition is required by the observed runtime and target profile."),
        Err(error) => println!(
            "  Repository plan: unresolved — authenticated repository metadata could not be loaded or verified: {error:#}"
        ),
    }
    Ok(())
}

fn print_system_explanation(root: &Path, topic: &str) -> Result<()> {
    if topic != "init-system" {
        return Err(anyhow!("unsupported system topic {topic:?}; the supported topic is `init-system`"));
    }
    let observed = detect::observe(root, Path::new("/proc"));
    for line in render_system_explanation(&observed) { println!("{line}"); }
    Ok(())
}

fn cmd_migrate(ctx: &Ctx, action: MigrateCmd) -> Result<()> {
    match action {
        MigrateCmd::List => print_list(&ctx.root),
        MigrateCmd::Show { id } => print_show(&ctx.root, &id),
        MigrateCmd::Status => print_status(&ctx.root),
        MigrateCmd::Plan { id } => print_plan(&ctx.root, &id),
        MigrateCmd::Apply {
            id,
            from,
            to,
            acknowledge_reboot,
            acknowledge_recovery,
        } => print_apply(
            ctx,
            &id,
            &from,
            &to,
            acknowledge_reboot,
            acknowledge_recovery,
        ),
        MigrateCmd::Resume => print_resume(ctx),
    }
}

fn render_system_status(observed: &detect::ObservedSystem) -> Vec<String> {
    let mut lines = vec!["System status".to_string(), format!("  Target root: {}", observed.root.display())];
    let (context, evidence) = match &observed.context {
        detect::Context::LiveHost { evidence } => ("live host", evidence.as_slice()),
        detect::Context::Container { evidence } => ("container", evidence.as_slice()),
        detect::Context::Chroot { evidence } => ("chroot", evidence.as_slice()),
        detect::Context::OfflineRoot { .. } => ("offline target root", &[][..]),
        detect::Context::Unknown { .. } => ("unknown environment", &[][..]),
    };
    lines.push(format!("  Execution context: {context}"));
    if let detect::Context::Unknown { reason } = &observed.context { lines.push(format!("    reason: {reason}")); }
    for item in evidence { lines.push(format!("    evidence: {} = {}", item.source, item.value)); }
    match &observed.runtime {
        detect::RuntimeInit::Observed { kind, confidence, evidence } => {
            lines.push(format!("  Runtime PID 1:     {} ({confidence:?} evidence)", kind.capability_identity()));
            for item in evidence { lines.push(format!("    evidence: {} = {}", item.source, item.value)); }
        }
        detect::RuntimeInit::Unknown { reason } => lines.push(format!("  Runtime PID 1:     unknown ({reason})")),
    }
    if observed.installed.candidates.is_empty() {
        lines.push("  Installed init:    no candidate identified (absence is not proof of absence)".to_string());
    } else {
        lines.push("  Installed init candidates:".to_string());
        for candidate in &observed.installed.candidates {
            lines.push(format!("    - {}: {} ({})", candidate.kind.capability_identity(), candidate.path.display(), candidate.evidence));
        }
    }
    match &observed.desired {
        detect::DesiredInit::Declared { identity, source } => lines.push(format!("  Desired init:      {identity} (from {source})")),
        detect::DesiredInit::NotDeclared => lines.push("  Desired init:      not declared by a system profile".to_string()),
        detect::DesiredInit::Invalid { reason } => lines.push(format!("  Desired init:      invalid profile (fails closed): {reason}")),
    }
    lines
}

fn render_system_profile(effective: &system_profile::EffectiveProfile) -> Vec<String> {
    let Some(profile) = &effective.profile else {
        return vec!["System profile".to_string(),
            "  No distribution default or administrator override is declared.".to_string(),
            "  No target init provider will be inferred.".to_string()];
    };
    let mut lines = vec![
        "Effective system profile".to_string(),
        format!("  Schema:        {}", profile.schema),
        format!("  Distribution:  {}", profile.distribution.as_deref().unwrap_or("not declared")),
        format!("  Profile:       {}", profile.profile.as_deref().unwrap_or("not declared")),
        format!("  Architecture:  {}", profile.architecture.as_deref().unwrap_or("not declared")),
    ];
    if profile.providers.is_empty() { lines.push("  Providers:     none declared".to_string()); }
    else {
        lines.push("  Providers:".to_string());
        for (role, identity) in &profile.providers {
            let source = effective.role_sources.get(role).map(|p| p.display().to_string()).unwrap_or_else(|| "source unavailable".to_string());
            lines.push(format!("    {role} = {identity} (from {source})"));
        }
    }
    lines
}

fn render_system_plan(observed: &detect::ObservedSystem) -> Vec<String> {
    let mut lines = vec!["System plan (read-only; no package, boot or profile changes)".to_string(),
        format!("  Root: {}", observed.root.display())];
    match &observed.context {
        detect::Context::LiveHost { .. } => lines.push("  Context: live host".to_string()),
        detect::Context::Container { .. } => lines.push("  Context: container (migration apply is refused)".to_string()),
        detect::Context::Chroot { .. } => lines.push("  Context: chroot (migration apply is refused)".to_string()),
        detect::Context::OfflineRoot { .. } => lines.push("  Context: offline target root".to_string()),
        detect::Context::Unknown { reason } => lines.push(format!("  Context: unknown ({reason})")),
    }
    let runtime = match &observed.runtime {
        detect::RuntimeInit::Observed {
            kind, confidence, ..
        } => Some((kind.capability_identity(), *confidence)),
        detect::RuntimeInit::Unknown { .. } => None,
    };
    let desired = match &observed.desired {
        detect::DesiredInit::Declared { identity, source } => { lines.push(format!("  Desired init: {identity} (from {source})")); Some(identity.as_str()) }
        detect::DesiredInit::NotDeclared => { lines.push("  Desired init: not declared".to_string()); None }
        detect::DesiredInit::Invalid { reason } => { lines.push(format!("  Desired init: invalid ({reason})")); None }
    };
    match (runtime, desired) {
        (Some((running, detect::Confidence::Proven)), Some(target)) if running == target => {
            lines.push(format!("  Decision: runtime already matches init-system={target}."));
        }
        (Some((running, detect::Confidence::Proven)), Some(target)) => {
            lines.push(format!("  Decision: transition required from init-system={running} to init-system={target}."));
            if running == "runit" && target == "systemd" {
                lines.push("  Built-in candidate: faseela-runit-to-systemd-v1 (not executable yet; inspect with nawa system migrate plan).".to_string());
            } else {
                lines.push("  No built-in migration handler supports this transition; no action is proposed.".to_string());
            }
        }
        (Some((running, detect::Confidence::Indicated)), Some(_)) => {
            lines.push(format!(
                "  Decision: unresolved — PID 1 identity is only indicated by one evidence source ({running}); proven runtime identity is required."
            ));
        }
        (None, Some(_)) => lines.push("  Decision: unresolved — runtime PID 1 is unknown; the target profile is not proof of the running init.".to_string()),
        (_, None) => lines.push("  Decision: unresolved — a valid target init provider is not declared; no provider will be guessed.".to_string()),
    }
    if observed.installed.candidates.is_empty() { lines.push("  Installed candidates: none identified from target-root evidence.".to_string()); }
    else {
        lines.push("  Installed candidates:".to_string());
        for candidate in &observed.installed.candidates {
            lines.push(format!("    - {} at {} ({})", candidate.kind.capability_identity(), candidate.path.display(), candidate.evidence));
        }
    }
    lines.push("  Package actions: resolved separately from authenticated repository metadata; this inspection does not apply them.".to_string());
    lines
}

fn render_system_explanation(observed: &detect::ObservedSystem) -> Vec<String> {
    let mut lines = vec![
        "Capability: init-system".to_string(),
        "  Role: exclusive system init provider; supported migration identities are runit and systemd.".to_string(),
        "  Runtime: derived only from attributable PID 1 evidence (/proc/1/exe and /proc/1/comm); unknown or contradictory evidence is never guessed.".to_string(),
        "  Installed: target-root symlinks and package-state evidence are candidates, not proof of the running PID 1.".to_string(),
        "  Desired: read from the effective system profile; administrator overrides take precedence over distribution defaults.".to_string(),
        "  Package metadata: recipe capabilities are carried into .PKGINFO and authenticated signed repository index v4; legacy v1-v3 metadata remains unknown, not empty.".to_string(),
        "  Selection: install/update planning explains the provider declared by the target profile and verified candidate metadata; it does not switch the running init.".to_string(),
        "  Boundary: Nawa does not query or manage service state, start services, or reboot the machine.".to_string(),
    ];
    lines.extend(render_system_status(observed).into_iter().skip(1));
    lines
}
fn print_plan(root: &Path, id: &str) -> Result<()> {
    let definition = apply_use::builtin_definition();
    if id != definition.id {
        return Err(anyhow!(
            "no built-in migration named {id:?} — this Nawa provides {}",
            definition.id
        ));
    }
    let plan = apply_use::plan(root, Path::new("/proc"));
    for line in render_plan(&plan) {
        println!("{line}");
    }
    Ok(())
}

fn print_apply(
    ctx: &Ctx,
    id: &str,
    from: &str,
    to: &str,
    acknowledge_reboot: bool,
    acknowledge_recovery: bool,
) -> Result<()> {
    let source = crate::domain::system::capability::Capability::parse(from)
        .map_err(anyhow::Error::msg)?;
    let target = crate::domain::system::capability::Capability::parse(to)
        .map_err(anyhow::Error::msg)?;
    let approval = apply_use::MigrationApproval::explicit(
        id,
        &source,
        &target,
        acknowledge_reboot,
        acknowledge_recovery,
    )?;
    match apply_use::apply(&ctx.root, Path::new("/proc"), approval)? {
        apply_use::ApplyRefusal::HandlerBlocked(blockers) => {
            for line in render_apply_refusal(&blockers) {
                println!("{line}");
            }
        }
    }
    Ok(())
}

/// Presentation adapter for the recovery use case. Recovery never prompts,
/// so `choose`/`confirm` are inert; progress and warnings still flow through
/// the real status macros (functional core / imperative shell).
fn recovery_ui() -> UserInteraction {
    UserInteraction {
        stderr_line: Box::new(|line| eprintln!("{line}")),
        info: Box::new(|line| crate::nawa_info!("{}", line)),
        warn: Box::new(|line| crate::nawa_warn!("{}", line)),
        show_plan: Box::new(|_| {}),
        choose: Box::new(|_prompt, _default, _allowed| anyhow::bail!("migration recovery does not prompt")),
        confirm: Box::new(|_prompt| anyhow::bail!("migration recovery does not confirm")),
    }
}

fn print_resume(ctx: &Ctx) -> Result<()> {
    let ui = recovery_ui();
    let (report, outcome) = recover::recover_and_resume(ctx, &ui)?;
    if report.notes.is_empty() {
        crate::nawa_info!("no interrupted migration needed recovery");
    }
    for note in &report.notes {
        println!("{}", render_recovery_note(note));
    }
    if let Some(outcome) = outcome {
        for line in render_resume_outcome(&outcome) {
            println!("{line}");
        }
    }
    Ok(())
}

fn print_list(root: &Path) -> Result<()> {
    let listing = inspect::list(root)?;
    if listing.is_empty() {
        // §12.2: an empty store is a stated result — no migration record
        // exists under this root, so none was ever started (terminal
        // records would still be listed here)
        crate::nawa_info!(
            "no migration record exists under {} — no migration has ever been started under this root",
            crate::adapters::migration_record::migrations_dir(root).display()
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

/// The read-only migration plan (§12.1–2), rendered from typed data. It states
/// whether the facts describe the live host or an offline root and never
/// claims a boot the built-in handler cannot yet perform.
fn render_plan(plan: &apply_use::MigrationPlan) -> Vec<String> {
    let d = &plan.definition;
    let context = match &plan.observed.context {
        crate::application::features::system::detect::Context::LiveHost { .. } => "the live host",
        crate::application::features::system::detect::Context::Container { .. } => "a container",
        crate::application::features::system::detect::Context::Chroot { .. } => "a chroot",
        crate::application::features::system::detect::Context::OfflineRoot { .. } => {
            "an offline target root"
        }
        crate::application::features::system::detect::Context::Unknown { .. } => {
            "an unverified environment"
        }
    };
    let runtime = match &plan.observed.runtime {
        crate::application::features::system::detect::RuntimeInit::Observed { kind, .. } => {
            kind.capability_identity()
        }
        crate::application::features::system::detect::RuntimeInit::Unknown { .. } => {
            "unknown".to_string()
        }
    };
    let desired = match &plan.observed.desired {
        crate::application::features::system::detect::DesiredInit::Declared { identity, .. } => {
            identity.clone()
        }
        crate::application::features::system::detect::DesiredInit::NotDeclared => {
            "not declared".to_string()
        }
        crate::application::features::system::detect::DesiredInit::Invalid { .. } => {
            "invalid (fails closed)".to_string()
        }
    };
    let mut lines = vec![
        format!("Migration plan for {} (definition v{})", d.id, d.version),
        format!("  Describes:            {context}"),
        format!("  Source capability:    {}", d.from.to_key()),
        format!("  Target capability:    {}", d.to.to_key()),
        format!("  Trusted handler:      {} (reboot required: {})", d.handler.as_str(), yes_no(d.requires_reboot)),
        format!("  Observed PID 1:       {runtime}"),
        format!("  Desired init:         {desired}"),
        "  Package actions:      from authenticated repository metadata at apply time".to_string(),
    ];
    lines.push("  Boot/init actions:".to_string());
    for action in &plan.boot_actions {
        lines.push(format!("    - {action}"));
    }
    lines.push("  Rollback limits:".to_string());
    for limit in &plan.rollback_limits {
        lines.push(format!("    - {limit}"));
    }
    lines
}

/// A refused apply is reported honestly with the exact external blockers,
/// not as a success (§12.2, §10.4).
fn render_apply_refusal(blockers: &[&str]) -> Vec<String> {
    let mut lines = vec![
        "nawa: migration apply refused — the built-in boot handler is blocked by external dependencies".to_string(),
        "  No migration record, mutation gate or package change was created.".to_string(),
        "  Blockers and owners:".to_string(),
    ];
    for blocker in blockers {
        lines.push(format!("    - {blocker}"));
    }
    lines.push(
        "  Offline planning remains available (`nawa system migrate plan`).".to_string(),
    );
    lines
}

/// One recovery decision, rendered from typed data (no decisions here).
fn render_recovery_note(note: &recover::RecoveryNote) -> String {
    use recover::RecoveryAction as A;
    let action = match &note.action {
        A::Discarded => "discarded the abandoned planned record".to_string(),
        A::ReConcluded(state) => format!("re-concluded as {}", state.as_str()),
        A::HistoryFinalized(detail) => format!("finalized transaction history: {detail}"),
        A::NeedsRecovery(reason) => format!("needs recovery: {reason}"),
        A::Unchanged(reason) => format!("unchanged: {reason}"),
    };
    format!("  {}: {action}", note.id)
}

/// The post-reboot verification result, rendered from typed data.
fn render_resume_outcome(outcome: &recover::ResumeOutcome) -> Vec<String> {
    match outcome {
        recover::ResumeOutcome::Verified(record) => vec![
            format!(
                "Migration {} resumed: target init verified, state {} (mutation gate clear)",
                record.id,
                record.state.as_str()
            ),
            "  Old-provider cleanup remains a separate, explicitly authorized action".to_string(),
        ],
        recover::ResumeOutcome::NeedsRecovery(record, failures) => {
            let mut lines = vec![format!(
                "Migration {} verification failed: state {} (mutation gate stays active)",
                record.id,
                record.state.as_str()
            )];
            for failure in failures {
                lines.push(format!("  - {failure}"));
            }
            lines.push(
                "  Resolve the boot/init issue, then run `nawa system migrate resume` again"
                    .to_string(),
            );
            lines
        }
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
    fn system_plan_does_not_claim_a_transition_from_indicated_pid1_evidence() {
        let observed = detect::ObservedSystem {
            root: Path::new("/").to_path_buf(),
            context: detect::Context::LiveHost { evidence: Vec::new() },
            runtime: detect::RuntimeInit::Observed {
                kind: crate::domain::system::identity::InitKind::Runit,
                confidence: detect::Confidence::Indicated,
                evidence: vec![detect::Evidence {
                    source: "/proc/1/exe".to_string(),
                    value: "/usr/bin/runit".to_string(),
                }],
            },
            installed: detect::InstalledInit::default(),
            desired: detect::DesiredInit::Declared {
                identity: "systemd".to_string(),
                source: "/usr/share/nawa/system-profile.toml".to_string(),
            },
            effective_profile: None,
        };

        let text = joined(&render_system_plan(&observed));
        assert!(
            text.contains("unresolved — PID 1 identity is only indicated"),
            "the plan must expose the evidence limitation: {text}"
        );
        assert!(
            !text.contains("transition required"),
            "an indicated identity cannot authorize a transition decision: {text}"
        );
        assert!(
            !text.contains("Built-in candidate"),
            "a migration candidate must not be advertised without proven PID 1: {text}"
        );
    }

    #[test]
    fn system_plan_shows_a_candidate_only_for_proven_pid1_evidence() {
        let observed = detect::ObservedSystem {
            root: Path::new("/").to_path_buf(),
            context: detect::Context::LiveHost { evidence: Vec::new() },
            runtime: detect::RuntimeInit::Observed {
                kind: crate::domain::system::identity::InitKind::Runit,
                confidence: detect::Confidence::Proven,
                evidence: vec![
                    detect::Evidence {
                        source: "/proc/1/exe".to_string(),
                        value: "/usr/bin/runit".to_string(),
                    },
                    detect::Evidence {
                        source: "/proc/1/comm".to_string(),
                        value: "runit".to_string(),
                    },
                ],
            },
            installed: detect::InstalledInit::default(),
            desired: detect::DesiredInit::Declared {
                identity: "systemd".to_string(),
                source: "/usr/share/nawa/system-profile.toml".to_string(),
            },
            effective_profile: None,
        };

        let text = joined(&render_system_plan(&observed));
        assert!(text.contains("transition required from init-system=runit to init-system=systemd"), "{text}");
        assert!(text.contains("Built-in candidate: faseela-runit-to-systemd-v1"), "{text}");
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

    #[test]
    fn recovery_notes_render_every_action_kind_without_deciding() {
        use recover::RecoveryAction as A;
        let cases = [
            (A::Discarded, "discarded the abandoned planned record"),
            (A::ReConcluded(MigrationState::AwaitingReboot), "re-concluded as awaiting-reboot"),
            (A::HistoryFinalized("transaction 1 reconciled as partial".into()), "finalized transaction history"),
            (A::NeedsRecovery("ambiguous".into()), "needs recovery: ambiguous"),
            (A::Unchanged("terminal".into()), "unchanged: terminal"),
        ];
        for (action, expected) in cases {
            let line = render_recovery_note(&recover::RecoveryNote {
                id: "mig".to_string(),
                action,
            });
            assert!(line.starts_with("  mig: "), "{line}");
            assert!(line.contains(expected), "missing {expected:?} in {line}");
        }
    }

    #[test]
    fn a_verified_resume_is_rendered_with_the_gate_clear_result() {
        let outcome = recover::ResumeOutcome::Verified(record(
            "faseela-runit-to-systemd-v1",
            MigrationState::Completed,
        ));
        let text = joined(&render_resume_outcome(&outcome));
        assert!(text.contains("target init verified"), "{text}");
        assert!(text.contains("mutation gate clear"), "{text}");
        assert!(text.contains("Old-provider cleanup remains a separate"), "{text}");
    }

    #[test]
    fn a_failed_resume_renders_every_failure_and_the_gate_warning() {
        let outcome = recover::ResumeOutcome::NeedsRecovery(
            record("mig", MigrationState::NeedsRecovery),
            vec!["PID 1 is runit".to_string(), "boot entry missing".to_string()],
        );
        let text = joined(&render_resume_outcome(&outcome));
        assert!(text.contains("mutation gate stays active"), "{text}");
        assert!(text.contains("PID 1 is runit"), "{text}");
        assert!(text.contains("boot entry missing"), "{text}");
        assert!(text.contains("run `nawa system migrate resume` again"), "{text}");
    }
}
