//! Crash reconciliation (roadmap §5.5): decide what an interrupted journal
//! actually did, by comparing the write-ahead intents with the package
//! database.
//!
//! * db matches the intent's target state -> the step committed;
//! * db matches the precondition -> it never applied;
//! * neither -> `needs-recovery`: refuse to guess.
//!
//! Recovery never re-runs package hooks: they are external side effects with
//! their own recorded status, not something reconciliation can replay safely.

use anyhow::Result;
use std::path::Path;

use super::history::{PackageChange, TransactionRecord, TxStatus};
use super::journal;

/// Reconcile one transaction's journal against the live database and return
/// the finished record (status `interrupted` if the process died before
/// finishing, with per-step outcomes inferred where they are unambiguous).
/// Must run while holding the exclusive database lock.
pub(crate) fn reconcile(root: &Path, tx: u64) -> Result<Option<TransactionRecord>> {
    let Some(state) = read_state(root, tx)? else { return Ok(None) };
    let existing = super::history::load_record(root, tx).ok();
    let db = crate::db::Db::new(root);
    let mut changes = Vec::new();
    let mut any_committed = false;
    let mut ambiguous = false;

    for (intent, recorded_result) in journal::transaction_steps(root, tx)? {
        let current = db.entry(&intent.package)?;
        let (outcome, failure, hook) = match recorded_result {
            Some(result) => match result.outcome.as_str() {
                "committed" => ("committed", result.failure, result.hook),
                "committed-hook-failed" => ("committed-hook-failed", result.failure, result.hook),
                "failed" => {
                    let target_matches = intent.target.as_ref().is_some_and(|target| current.as_ref()
                        .is_some_and(|entry| target.matches_entry(entry)));
                    let before_matches = intent.precondition.as_ref().is_some_and(|before| current.as_ref()
                        .is_some_and(|entry| before.matches_entry(entry)));
                    if target_matches && !before_matches {
                        // Legacy callers could write "failed" after a package
                        // commit when only the post-install hook had failed.
                        ("committed-hook-failed", result.failure, Some("failed".to_string()))
                    } else if target_matches && before_matches {
                        ambiguous = true;
                        ("ambiguous", Some("legacy WAL says failed but both before- and target-state match the database".into()), None)
                    } else { ("failed", result.failure, result.hook) }
                }
                other => anyhow::bail!("transaction {tx} step {} has unknown outcome {other:?}", intent.step),
            },
            None => {
                let inferred = infer_pending_outcome(&intent, current.as_ref());
                if inferred == "ambiguous" {
                    ambiguous = true;
                    ("ambiguous", Some("the package database matches neither the intent's before- nor after-state".into()), None)
                } else {
                    journal::write_result(root, &journal::StepResult {
                        schema: super::history::SCHEMA_VERSION, tx, step: intent.step,
                        package: intent.package.clone(), outcome: inferred.to_string(),
                        failure: None, hook: None,
                    })?;
                    (inferred, None, None)
                }
            }
        };
        let committed = outcome == "committed" || outcome == "committed-hook-failed";
        any_committed |= committed;
        let after = if !committed || intent.action == "remove" { None }
            else { intent.target.clone().or_else(|| current.as_ref().map(super::history::PackageState::from_entry)) };
        changes.push(PackageChange {
            name: intent.package, action: intent.action,
            status: match outcome {
                "committed" => "committed",
                "committed-hook-failed" => "committed-hook-failed",
                "failed" => "failed",
                _ => "needs-recovery",
            }.to_string(),
            before: intent.precondition, after, source: String::new(), failure, hook,
        });
    }

    let status = if ambiguous { TxStatus::NeedsRecovery }
        else if any_committed { TxStatus::Partial } else { TxStatus::Failed };
    let now = crate::artifact::now_secs();
    Ok(Some(TransactionRecord {
        schema: super::history::SCHEMA_VERSION, id: tx, command: state.command,
        status: status.as_str().to_string(),
        started_at: existing.as_ref().map(|r| r.started_at).unwrap_or(state.started_at),
        finished_at: Some(now), duration_seconds: Some(now.saturating_sub(state.started_at)),
        initiated_by: existing.as_ref().map(|r| r.initiated_by.clone()).unwrap_or_default(),
        requested_packages: existing.as_ref().map(|r| r.requested_packages.clone()).unwrap_or_default(),
        repository_ids: existing.as_ref().map(|r| r.repository_ids.clone()).unwrap_or_default(),
        package_changes: changes, undone_by: existing.as_ref().and_then(|r| r.undone_by),
        undoes: existing.as_ref().and_then(|r| r.undoes),
        groups: existing.as_ref().map(|r| r.groups.clone()).unwrap_or_default(),
    }))
}

/// A missing target means removal only for an explicit remove action.
fn infer_pending_outcome(intent: &journal::StepIntent, current: Option<&crate::db::Entry>) -> &'static str {
    if intent.action == "remove" {
        return match current {
            None => "committed",
            Some(entry) if intent.precondition.as_ref().is_some_and(|before| before.matches_entry(entry)) => "failed",
            _ => "ambiguous",
        };
    }
    let Some(target) = intent.target.as_ref() else {
        return match current {
            Some(entry) if intent.precondition.as_ref().is_some_and(|before| before.matches_entry(entry)) => "failed",
            None if intent.precondition.is_none() => "failed",
            _ => "ambiguous",
        };
    };
    match current {
        Some(entry) if target.matches_entry(entry) => "committed",
        Some(entry) if intent.precondition.as_ref().is_some_and(|before| before.matches_entry(entry)) => "failed",
        None if intent.precondition.is_none() => "failed",
        _ => "ambiguous",
    }
}

/// The journal's transaction-level marker, when it exists.
fn read_state(root: &Path, tx: u64) -> Result<Option<journal::TxState>> {
    let path = super::history::history_dir(root)
        .join("journal")
        .join(format!("{tx:010}"))
        .join("state.toml");
    if !path.exists() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(&path)?;
    Ok(Some(toml::from_str(&raw)?))
}

/// Reconcile every journaled transaction without a finished record, write
/// the results, and clean up the unambiguous journals. Returns human notes.
/// Must run while holding the exclusive database lock.
pub(crate) fn reconcile_incomplete(root: &Path) -> Result<Vec<String>> {
    let mut notes = Vec::new();
    for tx in journal::journaled_transactions(root)? {
        let pending = journal::pending_steps(root, tx)?;
        let existing = super::history::load_record(root, tx).ok();
        let final_record = existing.as_ref().is_some_and(|record| matches!(
            record.status().ok(),
            Some(TxStatus::Completed | TxStatus::Partial | TxStatus::Failed
                | TxStatus::Interrupted | TxStatus::NeedsRecovery | TxStatus::Undone)
        ));
        // A preparing/running record still needs reconstruction even with no
        // pending intents: its step results may exist, but finish() did not run.
        if final_record && pending.is_empty() {
            let needs_manual_recovery = existing.as_ref().is_some_and(|record| {
                record.status().ok() == Some(TxStatus::NeedsRecovery)
            });
            if !needs_manual_recovery {
                journal::clear_journal(root, tx)?;
            }
            continue;
        }
        if let Some(record) = reconcile(root, tx)? {
            if existing.is_some() { super::history::rewrite_record(root, &record)?; }
            else { super::history::write_record(root, &record)?; }
            if record.status != TxStatus::NeedsRecovery.as_str() {
                journal::clear_journal(root, tx)?;
            }
            notes.push(format!("transaction {tx} was interrupted; reconciled as {}", record.status));
        }
    }
    Ok(notes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{Db, Entry};
    use crate::transaction::history::{PackageState, SCHEMA_VERSION};

    use crate::testutil::tmpdir;

    fn add_entry(root: &std::path::Path, name: &str, version: &str, digest: &str) {
        let db = Db::new(root);
        db.add(
            &Entry {
                name: name.into(),
                version: version.into(),
                rel: 1,
                arch: String::new(),
                reason: "explicit".to_string(),
                run: vec![],
                install_date: 0,
                size: 0,
                artifact_sha256: digest.into(),
            },
            &[],
            &Default::default(),
        )
        .unwrap();
    }

    #[test]
    fn result_written_before_crash_is_not_discarded() {
        let base = tmpdir("result-before-finish");
        let root = base.join("root");
        let entry = crate::db::Entry {
            name: "demo".into(), version: "1.0".into(), rel: 1, arch: "any".into(),
            reason: "explicit".to_string(), run: vec![], install_date: 0, size: 0,
            artifact_sha256: "a".repeat(64),
        };
        crate::transaction::history::write_record(&root, &TransactionRecord {
            schema: SCHEMA_VERSION, id: 5, command: "install".into(), status: "preparing".into(),
            started_at: 10, finished_at: None, duration_seconds: None,
            initiated_by: "uid:1000".into(), requested_packages: vec!["demo".into()],
            repository_ids: vec!["test".into()], package_changes: vec![],
            undone_by: None, undoes: None, groups: vec![],
        }).unwrap();
        journal::write_tx_state(&root, &journal::TxState {
            schema: 1, tx: 5, command: "install".into(), started_at: 10,
        }).unwrap();
        journal::write_intent(&root, &journal::StepIntent {
            schema: SCHEMA_VERSION, tx: 5, step: 1, package: "demo".into(),
            action: "install".into(), precondition: None,
            target: Some(PackageState::from_entry(&entry)), artifact_sha256: "a".repeat(64),
        }).unwrap();
        add_entry(&root, "demo", "1.0", &"a".repeat(64));
        journal::write_result(&root, &journal::StepResult {
            schema: SCHEMA_VERSION, tx: 5, step: 1, package: "demo".into(),
            outcome: "committed".into(), failure: None, hook: None,
        }).unwrap();
        reconcile_incomplete(&root).unwrap();
        let record = crate::transaction::history::load_record(&root, 5).unwrap();
        assert_eq!(record.status, "partial");
        assert_eq!(record.requested_packages, vec!["demo"]);
        assert_eq!(record.repository_ids, vec!["test"]);
        assert_eq!(record.package_changes.len(), 1);
        assert_eq!(record.package_changes[0].status, "committed");
        assert!(!journal::journaled_transactions(&root).unwrap().contains(&5));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn legacy_install_without_target_is_not_misread_as_removal() {
        let base = tmpdir("legacy-null-target");
        let root = base.join("root");
        journal::write_tx_state(&root, &journal::TxState {
            schema: 1, tx: 6, command: "install".into(), started_at: 10,
        }).unwrap();
        journal::write_intent(&root, &journal::StepIntent {
            schema: SCHEMA_VERSION, tx: 6, step: 1, package: "demo".into(),
            action: "install".into(), precondition: None, target: None,
            artifact_sha256: String::new(),
        }).unwrap();
        reconcile_incomplete(&root).unwrap();
        let record = crate::transaction::history::load_record(&root, 6).unwrap();
        assert_eq!(record.status, "failed");
        assert_eq!(record.package_changes[0].status, "failed");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn crash_after_intent_before_install_is_failed() {
        let base = tmpdir("intent-only");
        let root = base.join("root");
        journal::write_tx_state(&root, &journal::TxState {
            schema: 1, tx: 1, command: "install".into(), started_at: 10,
        }).unwrap();
        journal::write_intent(&root, &journal::StepIntent {
            schema: SCHEMA_VERSION,
            tx: 1,
            step: 1,
            package: "demo".into(),
            action: "install".into(),
            precondition: None,
            target: Some(PackageState {
                version: Some("1.0".into()),
                rel: Some(1),
                arch: String::new(),
                reason: "explicit".to_string(),
                artifact_sha256: "a".repeat(64),
            }),
            artifact_sha256: "a".repeat(64),
        }).unwrap();
        // no db entry: the mutation never committed
        let notes = reconcile_incomplete(&root).unwrap();
        assert_eq!(notes.len(), 1);
        let record = crate::transaction::history::load_record(&root, 1).unwrap();
        assert_eq!(record.status().unwrap(), TxStatus::Failed);
        assert_eq!(record.package_changes[0].status, "failed");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn crash_after_db_commit_is_committed() {
        let base = tmpdir("after-commit");
        let root = base.join("root");
        journal::write_tx_state(&root, &journal::TxState {
            schema: 1, tx: 2, command: "install".into(), started_at: 10,
        }).unwrap();
        journal::write_intent(&root, &journal::StepIntent {
            schema: SCHEMA_VERSION,
            tx: 2,
            step: 1,
            package: "demo".into(),
            action: "install".into(),
            precondition: None,
            target: Some(PackageState {
                version: Some("1.0".into()),
                rel: Some(1),
                arch: String::new(),
                reason: "explicit".to_string(),
                artifact_sha256: "a".repeat(64),
            }),
            artifact_sha256: "a".repeat(64),
        }).unwrap();
        // the db commit DID happen before the crash
        add_entry(&root, "demo", "1.0", &"a".repeat(64));
        let _ = reconcile_incomplete(&root).unwrap();
        let record = crate::transaction::history::load_record(&root, 2).unwrap();
        assert_eq!(record.status().unwrap(), TxStatus::Partial);
        assert_eq!(record.package_changes[0].status, "committed");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn final_record_written_before_journal_cleanup_is_preserved() {
        let base = tmpdir("record-before-cleanup");
        let root = base.join("root");
        let mut record = TransactionRecord {
            schema: SCHEMA_VERSION, id: 8, command: "install".into(),
            status: "completed".into(), started_at: 10, finished_at: Some(11),
            duration_seconds: Some(1), initiated_by: "uid:1000".into(),
            requested_packages: vec!["demo".into()], repository_ids: vec![],
            package_changes: vec![], undone_by: None, undoes: None, groups: vec![],
        };
        journal::write_tx_state(&root, &journal::TxState {
            schema: 1, tx: 8, command: "install".into(), started_at: 10,
        }).unwrap();
        journal::write_intent(&root, &journal::StepIntent {
            schema: SCHEMA_VERSION, tx: 8, step: 1, package: "demo".into(),
            action: "install".into(), precondition: None, target: None,
            artifact_sha256: String::new(),
        }).unwrap();
        journal::write_result(&root, &journal::StepResult {
            schema: SCHEMA_VERSION, tx: 8, step: 1, package: "demo".into(),
            outcome: "failed".into(), failure: None, hook: None,
        }).unwrap();
        // Crash after publishing the final record but before journal cleanup.
        record.package_changes.push(PackageChange {
            name: "demo".into(), action: "install".into(), status: "failed".into(),
            before: None, after: None, source: "test".into(), failure: None, hook: None,
        });
        super::super::history::write_record(&root, &record).unwrap();
        assert!(reconcile_incomplete(&root).unwrap().is_empty());
        assert!(!journal::journaled_transactions(&root).unwrap().contains(&8));
        assert_eq!(super::super::history::load_record(&root, 8).unwrap().status, "completed");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn needs_recovery_journal_is_retained_after_repeated_reconciliation() {
        let base = tmpdir("retain-needs-recovery");
        let root = base.join("root");
        let digest = "a".repeat(64);
        let before = PackageState {
            version: Some("1.0".into()),
            rel: Some(1),
            arch: String::new(),
            reason: "explicit".to_string(),
            artifact_sha256: digest.clone(),
        };
        add_entry(&root, "demo", "1.0", &digest);
        journal::write_tx_state(&root, &journal::TxState {
            schema: 1, tx: 7, command: "install".into(), started_at: 10,
        }).unwrap();
        journal::write_intent(&root, &journal::StepIntent {
            schema: SCHEMA_VERSION,
            tx: 7,
            step: 1,
            package: "demo".into(),
            action: "install".into(),
            precondition: Some(before.clone()),
            target: Some(before),
            artifact_sha256: digest,
        }).unwrap();
        // This legacy result matches both before and target states. There are
        // no pending intents, but the transaction still needs manual recovery.
        journal::write_result(&root, &journal::StepResult {
            schema: SCHEMA_VERSION,
            tx: 7,
            step: 1,
            package: "demo".into(),
            outcome: "failed".into(),
            failure: Some("legacy result".into()),
            hook: None,
        }).unwrap();

        reconcile_incomplete(&root).unwrap();
        let record = crate::transaction::history::load_record(&root, 7).unwrap();
        assert_eq!(record.status().unwrap(), TxStatus::NeedsRecovery);
        let journal_dir = super::super::history::history_dir(&root)
            .join("journal")
            .join("0000000007");
        assert!(journal_dir.is_dir(), "manual-recovery journal must remain");

        reconcile_incomplete(&root).unwrap();
        assert!(journal_dir.is_dir(), "subsequent recovery must not delete it");
        let record = crate::transaction::history::load_record(&root, 7).unwrap();
        assert_eq!(record.status().unwrap(), TxStatus::NeedsRecovery);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn ambiguous_state_is_needs_recovery() {
        let base = tmpdir("ambiguous");
        let root = base.join("root");
        journal::write_tx_state(&root, &journal::TxState {
            schema: 1, tx: 3, command: "remove".into(), started_at: 10,
        }).unwrap();
        journal::write_intent(&root, &journal::StepIntent {
            schema: SCHEMA_VERSION,
            tx: 3,
            step: 1,
            package: "demo".into(),
            action: "remove".into(),
            precondition: Some(PackageState {
                version: Some("1.0".into()),
                rel: Some(1),
                arch: String::new(),
                reason: "explicit".to_string(),
                artifact_sha256: "a".repeat(64),
            }),
            target: None,
            artifact_sha256: String::new(),
        }).unwrap();
        // some OTHER version is installed: neither before nor after
        add_entry(&root, "demo", "2.0", &"b".repeat(64));
        let _ = reconcile_incomplete(&root).unwrap();
        let record = crate::transaction::history::load_record(&root, 3).unwrap();
        assert_eq!(record.status().unwrap(), TxStatus::NeedsRecovery);
        assert_eq!(record.package_changes[0].status, "needs-recovery");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn remove_that_did_not_apply_is_failed() {
        let base = tmpdir("rm-failed");
        let root = base.join("root");
        journal::write_tx_state(&root, &journal::TxState {
            schema: 1, tx: 4, command: "remove".into(), started_at: 10,
        }).unwrap();
        journal::write_intent(&root, &journal::StepIntent {
            schema: SCHEMA_VERSION,
            tx: 4,
            step: 1,
            package: "demo".into(),
            action: "remove".into(),
            precondition: Some(PackageState {
                version: Some("1.0".into()),
                rel: Some(1),
                arch: String::new(),
                reason: "explicit".to_string(),
                artifact_sha256: "a".repeat(64),
            }),
            target: None,
            artifact_sha256: String::new(),
        }).unwrap();
        // still installed at exactly the precondition bytes
        add_entry(&root, "demo", "1.0", &"a".repeat(64));
        let _ = reconcile_incomplete(&root).unwrap();
        let record = crate::transaction::history::load_record(&root, 4).unwrap();
        assert_eq!(record.status().unwrap(), TxStatus::Failed);
        assert_eq!(record.package_changes[0].status, "failed");
        let _ = std::fs::remove_dir_all(&base);
    }
}
