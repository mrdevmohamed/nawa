//! Operation context (roadmap §4.2 + §5): one identity passed through every
//! package-changing command, recording the transaction and its steps.
//!
//! Commands that change package state call [`begin_operation`] (which
//! allocates the id and writes the write-ahead state under the exclusive db
//! lock), wrap each package mutation in [`RecorderHandle::step`], and finish
//! with [`finish_operation`]. A no-history implementation exists for tests
//! and for commands that opt out.

use anyhow::Result;
use std::path::{Path, PathBuf};

use super::history::{
    PackageChange, PackageState, StepAction, StepStatus, TransactionRecord, TxStatus,
    SCHEMA_VERSION,
};
use super::journal;

/// Normalized command kinds — never a dump of the caller's arguments or
/// environment (roadmap §4.2).
#[allow(dead_code)] // GroupInstall/GroupRemove are wired by the Phase 5 group commands
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperationCommand {
    Install,
    Update,
    Upgrade,
    Remove,
    Autoremove,
    BuildInstall,
    ArchiveInstall,
    GroupInstall,
    GroupRemove,
    HistoryUndo,
}

impl OperationCommand {
    pub fn as_str(self) -> &'static str {
        match self {
            OperationCommand::Install => "install",
            OperationCommand::Update => "update",
            OperationCommand::Upgrade => "upgrade",
            OperationCommand::Remove => "remove",
            OperationCommand::Autoremove => "autoremove",
            OperationCommand::BuildInstall => "build-install",
            OperationCommand::ArchiveInstall => "archive-install",
            OperationCommand::GroupInstall => "group-install",
            OperationCommand::GroupRemove => "group-remove",
            OperationCommand::HistoryUndo => "history-undo",
        }
    }
}

/// Begin an operation: allocate the transaction id and write the write-ahead
/// state. MUST be called while the caller holds the exclusive database lock
/// (ID allocation is serialized by that lock — roadmap §5.6).
pub fn begin_operation(
    root: &Path,
    command: OperationCommand,
    requested: &[String],
    repositories: &[String],
) -> Result<OperationHandle> {
    let tx = super::history::allocate_id(root)?;
    let started_at = crate::artifact::now_secs();
    journal::write_tx_state(
        root,
        &journal::TxState {
            schema: SCHEMA_VERSION,
            tx,
            command: command.as_str().to_string(),
            started_at,
        },
    )?;
    let mut handle = OperationHandle {
        root: root.to_path_buf(),
        command,
        tx: Some(tx),
        started_at,
        requested: requested.to_vec(),
        repositories: repositories.to_vec(),
        changes: Vec::new(),
        disabled: false,
        groups: Vec::new(),
        undo_target: None,
    };
    // write the initial "preparing" record so `nawa history` shows it
    handle.persist_initial()?;
    Ok(handle)
}

/// A handle that records nothing (commands that opt out, unit tests).
#[allow(dead_code)]
pub fn no_history() -> OperationHandle {
    OperationHandle {
        root: PathBuf::new(),
        command: OperationCommand::Install,
        tx: None,
        started_at: 0,
        requested: Vec::new(),
        repositories: Vec::new(),
        changes: Vec::new(),
        disabled: true,
        groups: Vec::new(),
        undo_target: None,
    }
}

/// The live operation context handed to package steps.
pub struct OperationHandle {
    root: PathBuf,
    command: OperationCommand,
    tx: Option<u64>,
    started_at: u64,
    requested: Vec<String>,
    repositories: Vec<String>,
    changes: Vec<PackageChange>,
    disabled: bool,
    groups: Vec<String>,
    undo_target: Option<u64>,
}

impl OperationHandle {
    /// The allocated transaction id, when history recording is enabled.
    #[allow(dead_code)]
    pub fn tx(&self) -> Option<u64> {
        self.tx
    }

    /// The recorder view used by individual package steps. `db_before` is
    /// the package's current db entry (None = not installed).
    pub fn recorder(&mut self) -> RecorderHandle<'_> {
        RecorderHandle { op: self }
    }

    /// Persist the preparing/running record (id, command, requested names).
    fn persist_initial(&mut self) -> Result<()> {
        let Some(tx) = self.tx else { return Ok(()) };
        let record = TransactionRecord {
            schema: SCHEMA_VERSION,
            id: tx,
            command: self.command.as_str().to_string(),
            status: TxStatus::Preparing.as_str().to_string(),
            started_at: self.started_at,
            finished_at: None,
            duration_seconds: None,
            initiated_by: format!("uid:{}", rustix_uid()),
            requested_packages: self.requested.clone(),
            repository_ids: self.repositories.clone(),
            package_changes: Vec::new(),
            undone_by: None,
            undoes: None,
            groups: self.groups.clone(),
        };
        crate::transaction::history::write_record(&self.root, &record)
    }

    /// Record a group membership change in this transaction (Phase 5).
    #[allow(dead_code)]
    pub fn record_group(&mut self, group_id: &str) {
        if !self.groups.iter().any(|g| g == group_id) {
            self.groups.push(group_id.to_string());
        }
    }

    /// Finish the operation: decide completed/partial/failed from the step
    /// outcomes and persist the final record. Must be called while the
    /// caller still holds the exclusive db lock (or immediately after the
    /// last step).
    pub fn finish(self, error: Option<&anyhow::Error>) -> Result<()> {
        let Some(tx) = self.tx else { return Ok(()) };
        let failed = error.is_some();
        let any_committed = self
            .changes
            .iter()
            .any(|c| c.status == "committed" || c.status == "committed-hook-failed");
        let any_hook_failed = self
            .changes
            .iter()
            .any(|c| c.status == "committed-hook-failed");
        let status = if !failed && !any_hook_failed {
            TxStatus::Completed
        } else if any_committed || any_hook_failed {
            TxStatus::Partial
        } else {
            TxStatus::Failed
        };
        let finished_at = crate::artifact::now_secs();
        let record = TransactionRecord {
            schema: SCHEMA_VERSION,
            id: tx,
            command: self.command.as_str().to_string(),
            status: status.as_str().to_string(),
            started_at: self.started_at,
            finished_at: Some(finished_at),
            duration_seconds: Some(finished_at.saturating_sub(self.started_at)),
            initiated_by: format!("uid:{}", rustix_uid()),
            requested_packages: self.requested.clone(),
            repository_ids: self.repositories.clone(),
            package_changes: self.changes.clone(),
            undone_by: None,
            undoes: self.undo_target,
            groups: self.groups.clone(),
        };
        crate::transaction::history::write_record(&self.root, &record)?;
        // Publish a durable observer outbox record while the transaction id
        // and result are known. Actual plugin execution happens after the DB
        // lock is released by main(); observer errors never undo the commit.
        if let Err(error) = crate::plugins::events::enqueue_transaction(&self.root, &record) {
            eprintln!("nawa: warning: could not queue plugin observer events: {error:#}");
        }
        // the journal has served its purpose once the record is complete
        let _ = journal::clear_journal(&self.root, tx);
        Ok(())
    }

    /// Link this transaction as the compensating undo of `original`, and
    /// annotate the original with `undone_by` (an allowed rewrite).
    pub fn link_undo(&mut self, original: u64) -> Result<()> {
        let Some(tx) = self.tx else { return Ok(()) };
        let mut orig = crate::transaction::history::load_record(&self.root, original)?;
        orig.undone_by = Some(tx);
        super::history::rewrite_record(&self.root, &orig)?;
        // note it in memory so the final record carries `undoes`
        self.undo_target = Some(original);
        Ok(())
    }
}

/// Per-step recording surface handed to the package installer wrapper.
pub struct RecorderHandle<'a> {
    op: &'a mut OperationHandle,
}

impl RecorderHandle<'_> {
    /// Write the write-ahead intent for one package step (before the
    /// mutation runs).
    pub fn step_intent(
        &mut self,
        package: &str,
        action: StepAction,
        before: Option<&crate::db::Entry>,
        target: Option<&crate::db::Entry>,
        artifact_sha256: &str,
    ) -> Result<u32> {
        if self.op.disabled {
            return Ok(0);
        }
        let Some(tx) = self.op.tx else { return Ok(0) };
        let step = (self.op.changes.len() + 1) as u32;
        let intent = journal::StepIntent {
            schema: SCHEMA_VERSION,
            tx,
            step,
            package: package.to_string(),
            action: action.as_str().to_string(),
            precondition: before.map(PackageState::from_entry),
            target: target.map(PackageState::from_entry),
            artifact_sha256: artifact_sha256.to_string(),
        };
        journal::write_intent(&self.op.root, &intent)?;
        Ok(step)
    }

    /// Record the outcome of one package step (after the db commit or the
    /// failure), replacing the write-ahead intent.
    #[allow(clippy::too_many_arguments)]
    pub fn step_result(
        &mut self,
        step: u32,
        package: &str,
        action: StepAction,
        status: StepStatus,
        before: Option<&crate::db::Entry>,
        after: Option<&crate::db::Entry>,
        source: &str,
        failure: Option<String>,
        hook: Option<String>,
    ) -> Result<()> {
        if self.op.disabled {
            return Ok(());
        }
        let Some(tx) = self.op.tx else { return Ok(()) };
        journal::write_result(
            &self.op.root,
            &journal::StepResult {
                schema: SCHEMA_VERSION,
                tx,
                step,
                package: package.to_string(),
                outcome: match status {
                    StepStatus::Committed => "committed".into(),
                    StepStatus::CommittedHookFailed => "committed-hook-failed".into(),
                    StepStatus::Failed => "failed".into(),
                    StepStatus::Skipped => "failed".into(),
                },
                failure: failure.clone(),
                hook: hook.clone(),
            },
        )?;
        self.op.changes.push(PackageChange {
            name: package.to_string(),
            action: action.as_str().to_string(),
            status: match status {
                StepStatus::Committed => "committed".to_string(),
                StepStatus::CommittedHookFailed => "committed-hook-failed".to_string(),
                StepStatus::Failed => "failed".to_string(),
                StepStatus::Skipped => "skipped".to_string(),
            },
            before: before.map(PackageState::from_entry),
            after: after.map(PackageState::from_entry),
            source: source.to_string(),
            failure,
            hook,
        });
        Ok(())
    }
}

fn rustix_uid() -> u32 {
    rustix::process::getuid().as_raw()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{Db, Entry};

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("nawa-opctx-{}-{}", tag, std::process::id()));
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

    #[test]
    fn full_operation_lifecycle_records_steps() {
        let base = tmpdir("lifecycle");
        let root = base.join("root");
        let mut op = begin_operation(
            &root,
            OperationCommand::Install,
            &["demo".to_string()],
            &["faseela".to_string()],
        )
        .unwrap();
        let tx = op.tx().unwrap();
        assert_eq!(tx, 1);

        // step: install demo 1.0
        let mut rec = op.recorder();
        let step = rec
            .step_intent("demo", StepAction::Install, None, Some(&entry("demo", "1.0", &"a".repeat(64))), &"a".repeat(64))
            .unwrap();
        // simulate the db commit
        Db::new(&root).add(&entry("demo", "1.0", &"a".repeat(64)), &[], &Default::default()).unwrap();
        rec.step_result(
            step,
            "demo",
            StepAction::Install,
            StepStatus::Committed,
            None,
            Some(&entry("demo", "1.0", &"a".repeat(64))),
            "faseela",
            None,
            None,
        )
        .unwrap();

        op.finish(None).unwrap();
        let record = crate::transaction::history::load_record(&root, tx).unwrap();
        assert_eq!(record.status().unwrap(), TxStatus::Completed);
        assert_eq!(record.package_changes.len(), 1);
        assert_eq!(record.package_changes[0].name, "demo");
        assert_eq!(record.requested_packages, vec!["demo".to_string()]);
        assert!(record.finished_at.is_some());
        // the journal is cleaned up after a complete record
        assert!(journal::pending_steps(&root, tx).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn failing_operation_without_commits_is_failed() {
        let base = tmpdir("failed");
        let root = base.join("root");
        let op = begin_operation(&root, OperationCommand::Install, &["demo".to_string()], &[]).unwrap();
        let err = anyhow::anyhow!("fetch failed");
        op.finish(Some(&err)).unwrap();
        let record = crate::transaction::history::load_record(&root, 1).unwrap();
        assert_eq!(record.status().unwrap(), TxStatus::Failed);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn partial_operation_after_committed_step() {
        let base = tmpdir("partial");
        let root = base.join("root");
        let mut op = begin_operation(&root, OperationCommand::Install, &["a".to_string(), "b".to_string()], &[]).unwrap();
        {
            let mut rec = op.recorder();
            let step = rec.step_intent("a", StepAction::Install, None, Some(&entry("a", "1", &"a".repeat(64))), "").unwrap();
            Db::new(&root).add(&entry("a", "1", &"a".repeat(64)), &[], &Default::default()).unwrap();
            rec.step_result(step, "a", StepAction::Install, StepStatus::Committed, None,
                Some(&entry("a", "1", &"a".repeat(64))), "local-build", None, None).unwrap();
        }
        let err = anyhow::anyhow!("second fetch failed");
        op.finish(Some(&err)).unwrap();
        let record = crate::transaction::history::load_record(&root, 1).unwrap();
        assert_eq!(record.status().unwrap(), TxStatus::Partial);
        assert_eq!(record.package_changes.len(), 1);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn no_history_mode_records_nothing() {
        let base = tmpdir("nohist");
        let root = base.join("root");
        let mut op = no_history();
        assert!(op.tx().is_none());
        {
            let mut rec = op.recorder();
            let step = rec.step_intent("x", StepAction::Install, None, None, "").unwrap();
            rec.step_result(step, "x", StepAction::Install, StepStatus::Committed, None, None, "", None, None).unwrap();
        }
        op.finish(None).unwrap();
        assert!(crate::transaction::history::list_records(&root).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn undo_linking_annotates_both_records() {
        let base = tmpdir("undolink");
        let root = base.join("root");
        let mut op1 = begin_operation(&root, OperationCommand::Install, &["demo".to_string()], &[]).unwrap();
        {
            let mut rec = op1.recorder();
            let step = rec.step_intent("demo", StepAction::Install, None, Some(&entry("demo", "1.0", &"a".repeat(64))), "").unwrap();
            Db::new(&root).add(&entry("demo", "1.0", &"a".repeat(64)), &[], &Default::default()).unwrap();
            rec.step_result(step, "demo", StepAction::Install, StepStatus::Committed, None,
                Some(&entry("demo", "1.0", &"a".repeat(64))), "local-build", None, None).unwrap();
        }
        op1.finish(None).unwrap();

        let mut op2 = begin_operation(&root, OperationCommand::HistoryUndo, &[], &[]).unwrap();
        op2.link_undo(1).unwrap();
        op2.finish(None).unwrap();
        let original = crate::transaction::history::load_record(&root, 1).unwrap();
        assert_eq!(original.undone_by, Some(2));
        let _ = std::fs::remove_dir_all(&base);
    }
}
