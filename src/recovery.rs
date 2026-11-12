//! Combined crash recovery: physical file journals first, then the logical
//! transaction history/WAL.
//!
//! The physical pass restores files and package-database entries. Only then
//! can the logical pass compare write-ahead step intents with the database
//! and decide whether a step committed, failed, or needs manual recovery.
//! Running them in the other order would let a half-restored database look
//! like an ambiguous package state.
//!
//! Callers must hold the exclusive database lock: both passes assume no
//! other nawa process is mid-transaction.

use anyhow::Result;
use std::path::Path;

/// Run both recovery passes and return one note per repair.
pub(crate) fn recover(root: &Path) -> Result<Vec<String>> {
    let mut notes = crate::install::journal::recover(root)?;
    notes.extend(crate::transaction::recovery::reconcile_incomplete(root)?);
    Ok(notes)
}

/// [`recover`], reporting on stderr. Failures to repair are warnings: the
/// command at hand should still be able to run, and the next exclusive
/// command will retry the same idempotent passes.
pub(crate) fn recover_and_report(root: &Path) {
    match recover(root) {
        Ok(notes) => {
            for note in notes {
                crate::nawa_info_stderr!("recovered: {note}");
            }
        }
        Err(error) => {
            crate::nawa_warn!("could not check for an interrupted transaction: {error:#}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{Db, Entry};
    use crate::install::journal::{FileJournalId, Tx};
    use crate::transaction::history::{PackageState, TxStatus, SCHEMA_VERSION};
    use crate::transaction::journal::{self, StepIntent, TxState};

    use crate::testutil::tmpdir;

    fn entry(name: &str, version: &str, digest: &str) -> Entry {
        Entry {
            name: name.into(),
            version: version.into(),
            rel: 1,
            arch: "x86_64".into(),
            reason: "explicit".into(),
            run: vec![],
            install_date: 1,
            size: 1,
            artifact_sha256: digest.into(),
        }
    }

    fn state(entry: &Entry) -> PackageState {
        PackageState {
            version: Some(entry.version.clone()),
            rel: Some(entry.rel),
            arch: entry.arch.clone(),
            reason: entry.reason.clone(),
            artifact_sha256: entry.artifact_sha256.clone(),
        }
    }

    #[test]
    fn physical_repair_runs_before_logical_reconciliation() {
        let base = tmpdir("combined-recovery");
        let root = base.join("root");
        let db = Db::new(&root);
        let before = entry("demo", "1.0", &"b".repeat(64));
        let target = entry("demo", "2.0", &"a".repeat(64));
        db.add(&before, &[], &Default::default()).unwrap();
        std::fs::create_dir_all(root.join("usr/bin")).unwrap();
        std::fs::write(root.join("usr/bin/demo"), b"v1").unwrap();
        let stage = base.join("stage");
        std::fs::create_dir_all(&stage).unwrap();
        std::fs::write(stage.join("demo"), b"v2").unwrap();

        let tx = 1;
        let step = 1;
        // The logical WAL says "install v2 over v1".
        journal::write_tx_state(
            &root,
            &TxState {
                schema: 1,
                tx,
                command: "install".into(),
                started_at: 10,
            },
        )
        .unwrap();
        journal::write_intent(
            &root,
            &StepIntent {
                schema: SCHEMA_VERSION,
                tx,
                step,
                package: "demo".into(),
                action: "install".into(),
                precondition: Some(state(&before)),
                target: Some(state(&target)),
                artifact_sha256: target.artifact_sha256.clone(),
            },
        )
        .unwrap();

        // The physical journal already moved v2 into place and replaced the
        // db entry, then the process died before COMMIT.
        let mut physical = Tx::new_linked(
            &root,
            root.join(".nawa/bak-combined"),
            FileJournalId { tx, step },
        );
        physical
            .place(&stage.join("demo"), &root.join("usr/bin/demo"))
            .unwrap();
        physical
            .snapshot_db(&db.entry_dir("demo").unwrap())
            .unwrap();
        db.add(&target, &[], &Default::default()).unwrap();
        std::mem::forget(physical); // SIGKILL: journal and backup stay behind

        assert_eq!(std::fs::read(root.join("usr/bin/demo")).unwrap(), b"v2");
        assert_eq!(db.entry("demo").unwrap().unwrap().version, "2.0");

        let notes = recover(&root).unwrap();
        // Physical pass restored both the file and the db entry; only then
        // could the logical pass see that the step never committed.
        assert_eq!(std::fs::read(root.join("usr/bin/demo")).unwrap(), b"v1");
        assert_eq!(db.entry("demo").unwrap().unwrap().version, "1.0");
        let record = crate::transaction::history::load_record(&root, tx).unwrap();
        assert_eq!(record.status().unwrap(), TxStatus::Failed);
        assert!(
            notes.iter().any(|note| note.contains("tx 1 step 1")),
            "{notes:?}"
        );
        // A second pass is a no-op.
        assert!(recover(&root).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&base);
    }
}
