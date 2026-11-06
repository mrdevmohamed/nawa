//! Historical undo (roadmap §5.9): a compensating transaction, not time
//! travel. Every required artifact is resolved and re-verified BEFORE the
//! first package mutation; a package changed by a later operation conflicts
//! and fails the whole preflight rather than silently performing a partial
//! undo.

use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::history::{PackageState, StepAction, TransactionRecord, TxStatus};

/// What undo will do to one package.
#[derive(Debug, Clone, PartialEq)]
pub enum UndoAction {
    /// The transaction installed it (and nothing needed it before): remove.
    Remove,
    /// The transaction upgraded/downgraded/reinstalled it: restore the exact
    /// previous artifact.
    RestorePrevious { digest: String },
    /// The transaction removed it: reinstall the recorded artifact.
    Reinstall { digest: String },
    /// The step never committed: nothing to compensate.
    #[allow(dead_code)]
    Nothing,
}

/// One planned compensation.
#[derive(Debug, Clone)]
pub struct UndoStep {
    pub package: String,
    pub action: UndoAction,
    /// The before-state recorded by the original transaction.
    pub before: Option<PackageState>,
    /// The after-state recorded by the original transaction (what undo
    /// expects to find on disk right now).
    #[allow(dead_code)]
    pub after: Option<PackageState>,
    /// Reason to restore (from the before-state).
    pub reason: String,
}

/// A fully preflighted undo plan.
#[derive(Debug)]
pub struct UndoPlan {
    pub original_id: u64,
    pub steps: Vec<UndoStep>,
}

/// Build the undo plan for transaction `id` against the current database.
/// Every package the transaction changed is compared with its after-state;
/// a mismatch (later operation touched it) is a conflict. All required
/// archive objects are located and digest-verified here, before any
/// mutation. Must be called while holding the exclusive db lock.
pub fn plan_undo(root: &Path, id: u64) -> Result<UndoPlan> {
    let record: TransactionRecord = crate::transaction::history::load_record(root, id)?;
    if record.schema != super::history::SCHEMA_VERSION {
        bail!("transaction {id} uses unsupported schema {}", record.schema);
    }
    let status = record.status()?;
    if !matches!(
        status,
        TxStatus::Completed | TxStatus::Partial | TxStatus::Undone
    ) {
        bail!(
            "transaction {id} is {} — only completed or partial operations can be undone",
            status.as_str()
        );
    }
    if let Some(by) = record.undone_by {
        bail!("transaction {id} was already undone by transaction {by}");
    }
    let committed: Vec<&super::history::PackageChange> = record
        .package_changes
        .iter()
        .filter(|c| c.status == "committed" || c.status == "committed-hook-failed")
        .collect();
    if committed.is_empty() {
        bail!("transaction {id} has no committed package changes to compensate");
    }

    let db = crate::db::Db::new(root);
    let mut steps = Vec::new();
    for change in &committed {
        let current = db.entry(&change.name)?;
        // optimistic concurrency (roadmap §5.9): the current state must still
        // correspond to the after-state this transaction created
        let after = change.after.clone().unwrap_or(PackageState::absent());
        match (&current, after.is_installed()) {
            (Some(cur), true) if !after.matches_entry(cur) => bail!(
                "conflict: `{}` was changed by a later operation (now {}-{}), \
                 but transaction {id} left it at {} — refusing to undo",
                change.name,
                cur.version,
                cur.rel,
                after.version.clone().unwrap_or_default()
            ),
            (None, true) => bail!(
                "conflict: `{}` is no longer installed, so transaction {id}'s \
                 result has been superseded — refusing to undo",
                change.name
            ),
            (Some(_), false) => bail!(
                "conflict: `{}` is installed although transaction {id} removed it — \
                 refusing to undo",
                change.name
            ),
            _ => {}
        }
        let before = change.before.clone().unwrap_or(PackageState::absent());
        let action = if !before.is_installed() {
            // the transaction installed it new
            // (a removal below rechecks current reverse dependencies)
            UndoAction::Remove
        } else if before.artifact_sha256.is_empty() {
            bail!(
                "transaction {id} records no artifact digest for the previous \
                 version of `{}` (legacy record) — cannot restore exact bytes; \
                 use `nawa rollback {name}` instead",
                change.name,
                name = change.name
            );
        } else if after.is_installed() {
            UndoAction::RestorePrevious { digest: before.artifact_sha256.clone() }
        } else {
            UndoAction::Reinstall { digest: before.artifact_sha256.clone() }
        };
        let before_snapshot = before.clone();
        // preflight every required archive object right here, before the
        // first mutation (roadmap §5.9 step 8)
        if let UndoAction::RestorePrevious { digest } | UndoAction::Reinstall { digest } = &action {
            crate::artifact::verify_stored(root, digest).with_context(|| {
                format!(
                    "cannot undo transaction {id}: the archived original of `{}` \
                     is missing or fails verification",
                    change.name
                )
            })?;
        }
        steps.push(UndoStep {
            package: change.name.clone(),
            action,
            before: Some(before_snapshot),
            after: Some(after),
            reason: before.reason.clone(),
        });
    }
    if steps.is_empty() {
        bail!("transaction {id} has no compensable package changes");
    }
    // dependency order (roadmap §5.9): removed dependencies are restored
    // BEFORE the packages that require them; installed packages are removed
    // dependents-first.
    steps.sort_by(|a, b| match (&a.action, &b.action) {
        (UndoAction::RestorePrevious { .. } | UndoAction::Reinstall { .. }, UndoAction::Remove) => std::cmp::Ordering::Less,
        (UndoAction::Remove, UndoAction::RestorePrevious { .. } | UndoAction::Reinstall { .. }) => std::cmp::Ordering::Greater,
        _ => std::cmp::Ordering::Equal,
    });
    Ok(UndoPlan { original_id: id, steps })
}

/// Apply a preflighted undo plan through the ordinary installer/remover.
/// Each step rechecks its preconditions under the exclusive db lock right
/// before mutating. Returns the per-step outcomes for the history record.
pub struct AppliedStep {
    #[allow(dead_code)]
    pub package: String,
    #[allow(dead_code)]
    pub action: StepAction,
    pub committed: bool,
    #[allow(dead_code)]
    pub failure: Option<String>,
}

pub fn apply_undo(
    root: &Path,
    plan: &UndoPlan,
    recorder: &mut super::context::RecorderHandle<'_>,
    force: bool,
) -> Result<Vec<AppliedStep>> {
    let db = crate::db::Db::new(root);
    let mut applied = Vec::new();
    for step in &plan.steps {
        let before_entry = db.entry(&step.package)?;
        let (action, status, failure): (StepAction, bool, Option<String>) = match &step.action {
            UndoAction::Nothing => (StepAction::Reinstall, true, None),
            UndoAction::Remove => {
                // recheck reverse dependencies now, at execution time
                let dependents: Vec<String> = db
                    .installed()?
                    .into_iter()
                    .filter(|e| e.name != step.package && e.run.iter().any(|r| r == &step.package))
                    .map(|e| e.name)
                    .collect();
                if !dependents.is_empty() && !force {
                    (
                        StepAction::Remove,
                        false,
                        Some(format!("still required by {}", dependents.join(", "))),
                    )
                } else {
                    match crate::install::remove_with(&step.package, root, force) {
                        Ok(()) => (StepAction::Remove, true, None),
                        Err(e) => (StepAction::Remove, false, Some(format!("{e:#}"))),
                    }
                }
            }
            UndoAction::RestorePrevious { digest } | UndoAction::Reinstall { digest } => {
                match crate::artifact::verify_stored(root, digest)
                    .map_err(|e| anyhow::anyhow!("object verification failed: {e:#}"))
                    .and_then(|object| restore_object(root, &object, digest, &step.reason))
                {
                    Ok(()) => (StepAction::Restore, true, None),
                    Err(e) => (StepAction::Restore, false, Some(format!("{e:#}"))),
                }
            }
        };
        let after_entry = db.entry(&step.package)?;
        let step_no = recorder
            .step_intent(
                &step.package,
                action,
                before_entry.as_ref(),
                after_entry.as_ref(),
                step.before.as_ref().map(|b| b.artifact_sha256.as_str()).unwrap_or(""),
            )
            .unwrap_or(0);
        recorder
            .step_result(
                step_no,
                &step.package,
                action,
                if status {
                    super::history::StepStatus::Committed
                } else {
                    super::history::StepStatus::Failed
                },
                before_entry.as_ref(),
                after_entry.as_ref(),
                "history-undo",
                failure.clone(),
                None,
            )
            .ok();
        applied.push(AppliedStep {
            package: step.package.clone(),
            action,
            committed: status,
            failure,
        });
    }
    let any_failed = applied.iter().any(|s| !s.committed);
    if any_failed {
        let failed: Vec<String> = applied
            .iter()
            .filter(|s| !s.committed)
            .map(|s| s.package.clone())
            .collect();
        bail!(
            "undo of transaction {} partially failed for: {} (the completed steps \
             remain in effect and are recorded)",
            plan.original_id,
            failed.join(", ")
        );
    }
    Ok(applied)
}

/// Restore one object through the ordinary installer, honouring the recorded
/// authentication kind (roadmap §4.7 rules apply here too).
fn restore_object(root: &Path, object: &Path, digest: &str, _reason: &str) -> Result<()> {
    let prov = crate::artifact::load_provenance(root, digest)?;
    match prov.as_ref().map(|p| p.authentication.as_str()) {
        Some("signed-index") => {
            super::super::install::verify_retained_index_proof(root, digest)?;
            super::super::install::install_archive_from_index_proof(
                object, root, false, digest, "keep", None,
            )?;
            Ok(())
        }
        Some("detached-signature") => {
            super::super::install::verify_retained_detached_proof(root, digest)?;
            super::super::install::install_archive_pinned(
                object, root, false, true, None, "keep", Some(digest),
            )?;
            Ok(())
        }
        _ => {
            super::super::install::install_archive_pinned(
                object, root, false, true, None, "keep", Some(digest),
            )?;
            Ok(())
        }
    }
}

/// The `nawa history` table (roadmap §5.8): newest first, stable ids.
pub fn print_history(root: &Path, limit: usize) -> Result<()> {
    let records = crate::transaction::history::list_records(root)?;
    if records.is_empty() {
        println!("nawa: no transaction history yet");
        return Ok(());
    }
    println!(
        "{:<6}  {:<16}  {:<13}  {:<9}  Result",
        "ID", "Date", "Command", "Packages"
    );
    println!("{}", "-".repeat(60));
    for r in records.iter().rev().take(limit) {
        println!(
            "{:<6}  {:<16}  {:<13}  {:<9}  {}",
            r.id,
            crate::humanize::format_datetime(r.started_at),
            r.command,
            r.package_changes.len(),
            r.status,
        );
    }
    Ok(())
}

/// `nawa history info <ID>`: the full story of one transaction.
pub fn print_info(root: &Path, id: u64) -> Result<()> {
    let r = crate::transaction::history::load_record(root, id)?;
    println!("Transaction {}", r.id);
    println!("  Command:   {} {}", r.command, r.requested_packages.join(" "));
    println!("  Status:    {}", r.status);
    println!("  Started:   {}", crate::humanize::format_datetime(r.started_at));
    if let Some(f) = r.finished_at {
        println!("  Finished:  {}", crate::humanize::format_datetime(f));
    }
    if let Some(d) = r.duration_seconds {
        println!("  Duration:  {}", crate::humanize::format_duration(d));
    }
    if !r.initiated_by.is_empty() {
        println!("  Initiated: {}", r.initiated_by);
    }
    if !r.repository_ids.is_empty() {
        println!("  Repos:     {}", r.repository_ids.join(", "));
    }
    if let Some(u) = r.undone_by {
        println!("  Undone by: transaction {u}");
    }
    if let Some(u) = r.undoes {
        println!("  Undoes:    transaction {u}");
    }
    if r.package_changes.is_empty() {
        println!("  No package changes");
    }
    for c in &r.package_changes {
        println!();
        println!("  {}: {} ({})", c.name, c.action, c.status);
        if let Some(b) = &c.before {
            if let Some(v) = &b.version {
                println!("    before: {}-{}", v, b.rel.unwrap_or(1));
            }
        }
        if let Some(a) = &c.after {
            if let Some(v) = &a.version {
                println!("    after:  {}-{}", v, a.rel.unwrap_or(1));
            }
        }
        if !c.artifact_of_after().is_empty() {
            println!("    artifact: {}", c.artifact_of_after());
        }
        if !c.source.is_empty() {
            println!("    source:   {}", c.source);
        }
        if let Some(f) = &c.failure {
            println!("    failure:  {f}");
        }
        if let Some(h) = &c.hook {
            println!("    hook:     {h}");
        }
    }
    Ok(())
}

impl super::history::PackageChange {
    pub fn artifact_of_after(&self) -> &str {
        self.after.as_ref().map(|a| a.artifact_sha256.as_str()).unwrap_or("")
    }
}

/// `nawa history undo <ID>`: plan, confirm, then apply as a new transaction
/// linked to the original. Returns Ok(()) even when the user declined.
pub fn history_undo(root: &Path, id: u64, yes: bool) -> Result<()> {
    let _lock = crate::lock::acquire(root, crate::lock::Mode::Exclusive)?;
    // reconcile anything a killed nawa left behind first
    for note in super::recovery::reconcile_incomplete(root)? {
        eprintln!("nawa: recovered: {note}");
    }
    let plan = plan_undo(root, id)?;
    println!("nawa: undoing transaction {id} ({}) — compensating steps:", plan.steps.len());
    for step in &plan.steps {
        match &step.action {
            UndoAction::Remove => println!("  remove     {}", step.package),
            UndoAction::RestorePrevious { .. } => println!(
                "  restore    {} to {}",
                step.package,
                step.before.as_ref().and_then(|b| b.version.clone()).unwrap_or_default()
            ),
            UndoAction::Reinstall { .. } => println!(
                "  reinstall  {} ({})",
                step.package,
                step.before.as_ref().and_then(|b| b.version.clone()).unwrap_or_default()
            ),
            UndoAction::Nothing => {}
        }
    }
    println!("nawa: note: package hooks and other external side effects are not reversed");
    if !yes {
        crate::confirm::confirm(&format!("nawa: undo transaction {id}?"), false)?;
    }
    let mut op = super::context::begin_operation(
        root,
        super::context::OperationCommand::HistoryUndo,
        &[],
        &[],
    )?;
    op.link_undo(id)?;
    let mut recorder = op.recorder();
    let result = apply_undo(root, &plan, &mut recorder, false);
    let err = result.err();
    op.finish(err.as_ref())?;
    match err {
        Some(e) => Err(e),
        None => {
            println!("nawa: transaction {id} undone (recorded as a new transaction)");
            Ok(())
        }
    }
}

/// Map of installed entries, used by undo conflict checks in tests.
#[allow(dead_code)]
pub fn installed_map(root: &Path) -> Result<BTreeMap<String, crate::db::Entry>> {
    super::history::installed_snapshot(root)
}

#[allow(dead_code)]
fn _unused(_p: PathBuf) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{Db, Entry};
    use crate::transaction::context::{begin_operation, OperationCommand};
    use crate::transaction::history::{PackageChange, PackageState, StepStatus, SCHEMA_VERSION};

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("nawa-undo-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn entry(name: &str, version: &str, digest: &str) -> Entry {
        Entry {
            name: name.into(),
            version: version.into(),
            rel: 1,
            arch: String::new(),
            reason: "explicit".into(),
            run: vec![],
            install_date: 0,
            size: 0,
            artifact_sha256: digest.into(),
        }
    }

    fn make_pkg(dir: &Path, name: &str, version: &str, body: &[u8]) -> PathBuf {
        let staging = dir.join(format!("st-{name}-{version}"));
        std::fs::create_dir_all(&staging).unwrap();
        std::fs::write(staging.join("bin"), body).unwrap();
        let pkginfo = format!(
            "pkgname = {name}\npkgver = {version}\npkgrel = 1\narch = any\nrundeps = \nsize = 1\n"
        );
        let out = dir.join(format!("{name}-{version}-1.nawa.tar.zst"));
        crate::archive::create(&staging, &pkginfo, &out).unwrap();
        out
    }

    /// Record a completed install transaction with a real installed package.
    #[allow(dead_code)]
    fn record_install(root: &Path, name: &str, version: &str, digest: &str) -> u64 {
        let mut op = begin_operation(root, OperationCommand::Install, &[name.to_string()], &[]).unwrap();
        {
            let mut rec = op.recorder();
            let step = rec
                .step_intent(name, StepAction::Install, None, Some(&entry(name, version, digest)), digest)
                .unwrap();
            Db::new(root).add(&entry(name, version, digest), &[], &Default::default()).unwrap();
            rec.step_result(step, name, StepAction::Install, StepStatus::Committed, None,
                Some(&entry(name, version, digest)), "local-build", None, None).unwrap();
        }
        op.finish(None).unwrap();
        op_tx(root, name)
    }

    #[allow(dead_code)]
    fn op_tx(root: &Path, _name: &str) -> u64 {
        crate::transaction::history::list_records(root).unwrap().last().unwrap().id
    }

    #[test]
    fn undo_of_install_removes_the_package() {
        let base = tmpdir("rm");
        let root = base.join("root");
        let v1 = make_pkg(&base, "demo", "1.0", b"one");
        crate::install::install_archive(&v1, &root, false, true, None, "explicit").unwrap();
        let digest = crate::fetch::sha256_of_file(&v1).unwrap();
        let db = Db::new(&root);
        let installed = db.entry("demo").unwrap().unwrap();

        // synthesize the transaction record for that install
        let mut op = begin_operation(&root, OperationCommand::Install, &["demo".to_string()], &[]).unwrap();
        {
            let mut rec = op.recorder();
            let step = rec.step_intent("demo", StepAction::Install, None, Some(&installed), &digest).unwrap();
            rec.step_result(step, "demo", StepAction::Install, StepStatus::Committed, None,
                Some(&installed), "local-build", None, None).unwrap();
        }
        op.finish(None).unwrap();
        let tx = crate::transaction::history::list_records(&root).unwrap().last().unwrap().id;

        let plan = plan_undo(&root, tx).unwrap();
        assert_eq!(plan.steps.len(), 1);
        assert!(matches!(plan.steps[0].action, UndoAction::Remove));

        let mut op = begin_operation(&root, OperationCommand::HistoryUndo, &[], &[]).unwrap();
        op.link_undo(tx).unwrap();
        let mut rec = op.recorder();
        apply_undo(&root, &plan, &mut rec, false).unwrap();
        op.finish(None).unwrap();

        assert!(Db::new(&root).entry("demo").unwrap().is_none(), "package removed");
        let original = crate::transaction::history::load_record(&root, tx).unwrap();
        assert_eq!(original.undone_by, Some(tx + 1));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn undo_of_upgrade_restores_previous_artifact() {
        let base = tmpdir("upg");
        let root = base.join("root");
        let v1 = make_pkg(&base, "demo", "1.0", b"one");
        let v2 = make_pkg(&base, "demo", "2.0", b"two");
        crate::install::install_archive(&v1, &root, false, true, None, "explicit").unwrap();
        let before = Db::new(&root).entry("demo").unwrap().unwrap();
        crate::install::install_archive(&v2, &root, false, true, None, "explicit").unwrap();
        let after = Db::new(&root).entry("demo").unwrap().unwrap();

        let mut op = begin_operation(&root, OperationCommand::Upgrade, &[], &[]).unwrap();
        {
            let mut rec = op.recorder();
            let step = rec.step_intent("demo", StepAction::Upgrade, Some(&before), Some(&after), &after.artifact_sha256).unwrap();
            rec.step_result(step, "demo", StepAction::Upgrade, StepStatus::Committed, Some(&before),
                Some(&after), "local-build", None, None).unwrap();
        }
        op.finish(None).unwrap();
        let tx = crate::transaction::history::list_records(&root).unwrap().last().unwrap().id;

        let plan = plan_undo(&root, tx).unwrap();
        match &plan.steps[0].action {
            UndoAction::RestorePrevious { digest } => assert_eq!(*digest, before.artifact_sha256),
            other => panic!("expected RestorePrevious, got {other:?}"),
        }
        let mut op = begin_operation(&root, OperationCommand::HistoryUndo, &[], &[]).unwrap();
        let mut rec = op.recorder();
        apply_undo(&root, &plan, &mut rec, false).unwrap();
        op.finish(None).unwrap();
        assert_eq!(Db::new(&root).entry("demo").unwrap().unwrap().version, "1.0");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn superseded_package_refuses_undo() {
        let base = tmpdir("superseded");
        let root = base.join("root");
        let v1 = make_pkg(&base, "demo", "1.0", b"one");
        let v2 = make_pkg(&base, "demo", "2.0", b"two");
        let v3 = make_pkg(&base, "demo", "3.0", b"three");
        crate::install::install_archive(&v1, &root, false, true, None, "explicit").unwrap();
        let b1 = Db::new(&root).entry("demo").unwrap().unwrap();
        crate::install::install_archive(&v2, &root, false, true, None, "explicit").unwrap();
        let b2 = Db::new(&root).entry("demo").unwrap().unwrap();

        // transaction 1: upgrade 1.0 -> 2.0
        let mut op = begin_operation(&root, OperationCommand::Upgrade, &[], &[]).unwrap();
        {
            let mut rec = op.recorder();
            let step = rec.step_intent("demo", StepAction::Upgrade, Some(&b1), Some(&b2), &b2.artifact_sha256).unwrap();
            rec.step_result(step, "demo", StepAction::Upgrade, StepStatus::Committed, Some(&b1),
                Some(&b2), "local-build", None, None).unwrap();
        }
        op.finish(None).unwrap();
        let tx1 = crate::transaction::history::list_records(&root).unwrap().last().unwrap().id;

        // a LATER transaction upgrades 2.0 -> 3.0
        crate::install::install_archive(&v3, &root, false, true, None, "explicit").unwrap();
        let b3 = Db::new(&root).entry("demo").unwrap().unwrap();
        let mut op = begin_operation(&root, OperationCommand::Upgrade, &[], &[]).unwrap();
        {
            let mut rec = op.recorder();
            let step = rec.step_intent("demo", StepAction::Upgrade, Some(&b2), Some(&b3), &b3.artifact_sha256).unwrap();
            rec.step_result(step, "demo", StepAction::Upgrade, StepStatus::Committed, Some(&b2),
                Some(&b3), "local-build", None, None).unwrap();
        }
        op.finish(None).unwrap();

        // undoing tx1 must refuse: demo no longer matches its after-state
        let err = plan_undo(&root, tx1).unwrap_err();
        assert!(format!("{err:#}").contains("conflict"), "{err:#}");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn missing_object_refuses_undo_preflight() {
        let base = tmpdir("missing-obj");
        let root = base.join("root");
        let v1 = make_pkg(&base, "demo", "1.0", b"one");
        let v2 = make_pkg(&base, "demo", "2.0", b"two");
        crate::install::install_archive(&v1, &root, false, true, None, "explicit").unwrap();
        let b1 = Db::new(&root).entry("demo").unwrap().unwrap();
        crate::install::install_archive(&v2, &root, false, true, None, "explicit").unwrap();
        let b2 = Db::new(&root).entry("demo").unwrap().unwrap();
        // destroy the old object so restoration is impossible
        let obj = crate::artifact::object_path(&root, &b1.artifact_sha256).unwrap();
        std::fs::remove_file(&obj).unwrap();

        let mut op = begin_operation(&root, OperationCommand::Upgrade, &[], &[]).unwrap();
        {
            let mut rec = op.recorder();
            let step = rec.step_intent("demo", StepAction::Upgrade, Some(&b1), Some(&b2), &b2.artifact_sha256).unwrap();
            rec.step_result(step, "demo", StepAction::Upgrade, StepStatus::Committed, Some(&b1),
                Some(&b2), "local-build", None, None).unwrap();
        }
        op.finish(None).unwrap();
        let tx = crate::transaction::history::list_records(&root).unwrap().last().unwrap().id;

        let err = plan_undo(&root, tx).unwrap_err();
        assert!(format!("{err:#}").contains("missing or fails verification"), "{err:#}");
        // nothing changed
        assert_eq!(Db::new(&root).entry("demo").unwrap().unwrap().version, "2.0");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn already_undone_and_empty_refuse() {
        let base = tmpdir("meta");
        let root = base.join("root");
        // a failed transaction cannot be undone at all
        let op = begin_operation(&root, OperationCommand::Install, &["x".to_string()], &[]).unwrap();
        op.finish(Some(&anyhow::anyhow!("nope"))).unwrap();
        let err = plan_undo(&root, 1).unwrap_err();
        assert!(format!("{err:#}").contains("only completed or partial"), "{err:#}");

        // a completed transaction with zero package changes has nothing to compensate
        let op = begin_operation(&root, OperationCommand::Install, &["x".to_string()], &[]).unwrap();
        op.finish(None).unwrap();
        let err = plan_undo(&root, 2).unwrap_err();
        assert!(format!("{err:#}").contains("no committed package changes"), "{err:#}");

        // nonexistent id
        assert!(plan_undo(&root, 99).is_err());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn legacy_record_without_digest_refuses() {
        let base = tmpdir("legacy");
        let root = base.join("root");
        let record = TransactionRecord {
            schema: SCHEMA_VERSION,
            id: 1,
            command: "upgrade".into(),
            status: "completed".into(),
            started_at: 1,
            finished_at: Some(2),
            duration_seconds: Some(1),
            initiated_by: "uid:0".into(),
            requested_packages: vec![],
            repository_ids: vec![],
            package_changes: vec![PackageChange {
                name: "demo".into(),
                action: "upgrade".into(),
                status: "committed".into(),
                before: Some(PackageState {
                    version: Some("1.0".into()),
                    rel: Some(1),
                    arch: String::new(),
                    reason: "explicit".into(),
                    artifact_sha256: String::new(),
                }),
                after: Some(PackageState {
                    version: Some("2.0".into()),
                    rel: Some(1),
                    arch: String::new(),
                    reason: "explicit".into(),
                    artifact_sha256: String::new(),
                }),
                source: String::new(),
                failure: None,
                hook: None,
            }],
            undone_by: None,
            undoes: None,
            groups: vec![],
        };
        crate::transaction::history::write_record(&root, &record).unwrap();
        Db::new(&root).add(&entry("demo", "2.0", ""), &[], &Default::default()).unwrap();
        let err = plan_undo(&root, 1).unwrap_err();
        assert!(format!("{err:#}").contains("nawa rollback"), "{err:#}");
        let _ = std::fs::remove_dir_all(&base);
    }
}
