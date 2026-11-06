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
pub fn reconcile(root: &Path, tx: u64) -> Result<Option<TransactionRecord>> {
    let Some(state) = read_state(root, tx)? else {
        return Ok(None);
    };
    let db = crate::db::Db::new(root);
    let mut record = TransactionRecord {
        schema: super::history::SCHEMA_VERSION,
        id: tx,
        command: state.command.clone(),
        status: TxStatus::Interrupted.as_str().to_string(),
        started_at: state.started_at,
        finished_at: None,
        duration_seconds: None,
        initiated_by: String::new(),
        requested_packages: Vec::new(),
        repository_ids: Vec::new(),
        package_changes: Vec::new(),
        undone_by: None,
        undoes: None,
        groups: Vec::new(),
    };
    let mut any_committed = false;
    let mut ambiguous = false;
    for (_step, intent) in journal::pending_steps(root, tx)? {
        let current = db.entry(&intent.package)?;
        let outcome = match (&intent.target, &current) {
            // wanted installed: db shows exactly the target identity+bytes
            (Some(target), Some(entry)) if target.matches_entry(entry) => "committed",
            // wanted installed but it is not there at all
            (Some(_), None) => "failed",
            // wanted removed: db no longer has it
            (None, None) => "committed",
            // wanted removed but still installed with the precondition bytes
            (None, Some(entry)) => match &intent.precondition {
                Some(pre) if pre.matches_entry(entry) => "failed",
                // installed to something else entirely: cannot tell
                _ => "ambiguous",
            },
            // wanted installed, db has something else: cannot tell
            (Some(_), Some(_)) => "ambiguous",
        };
        match outcome {
            "ambiguous" => ambiguous = true,
            "committed" => any_committed = true,
            _ => {}
        }
        record.package_changes.push(PackageChange {
            name: intent.package.clone(),
            action: intent.action.clone(),
            status: if outcome == "ambiguous" {
                "needs-recovery".to_string()
            } else if outcome == "committed" {
                "committed".to_string()
            } else {
                "failed".to_string()
            },
            before: intent.precondition.clone(),
            after: if outcome == "committed" { intent.target.clone() } else { None },
            source: String::new(),
            failure: if outcome == "ambiguous" {
                Some("the package database matches neither the intent's before- nor after-state".into())
            } else {
                None
            },
            hook: None,
        });
    }
    if ambiguous {
        record.status = TxStatus::NeedsRecovery.as_str().to_string();
    } else if any_committed {
        record.status = TxStatus::Partial.as_str().to_string();
    } else if record.package_changes.is_empty() {
        record.status = TxStatus::Failed.as_str().to_string();
    } else {
        // every pending step failed to apply: nothing changed
        record.status = TxStatus::Failed.as_str().to_string();
    }
    Ok(Some(record))
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
pub fn reconcile_incomplete(root: &Path) -> Result<Vec<String>> {
    let mut notes = Vec::new();
    for tx in journal::journaled_transactions(root)? {
        // a finished record plus an empty pending set is just cleanup
        let pending = journal::pending_steps(root, tx)?;
        let existing = super::history::load_record(root, tx).ok();
        match (existing, pending.is_empty()) {
            (Some(_), true) => {
                journal::clear_journal(root, tx)?;
            }
            (Some(_), false) => {
                // record exists but steps are pending: reconcile into a
                // rewritten record (the old one is archived under .old/)
                if let Some(record) = reconcile(root, tx)? {
                    super::history::rewrite_record(root, &record)?;
                    notes.push(format!(
                        "transaction {tx} was interrupted; reconciled as {}",
                        record.status
                    ));
                }
            }
            (None, _) => {
                if let Some(record) = reconcile(root, tx)? {
                    crate::transaction::history::write_record(root, &record)?;
                    notes.push(format!(
                        "transaction {tx} was interrupted; recorded as {}",
                        record.status
                    ));
                }
            }
        }
    }
    Ok(notes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{Db, Entry};
    use crate::transaction::history::{PackageState, SCHEMA_VERSION};

    fn tmpdir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("nawa-recov-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn add_entry(root: &std::path::Path, name: &str, version: &str, digest: &str) {
        let db = Db::new(root);
        db.add(
            &Entry {
                name: name.into(),
                version: version.into(),
                rel: 1,
                arch: String::new(),
                reason: "explicit".into(),
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
                reason: "explicit".into(),
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
                reason: "explicit".into(),
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
                reason: "explicit".into(),
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
                reason: "explicit".into(),
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
