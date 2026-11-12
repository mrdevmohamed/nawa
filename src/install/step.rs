//! Shared transaction-history recording for package mutations.
//!
//! Every install-like and remove-like command must classify a failed step the
//! same way: did the package still get committed (with only its hook left to
//! retry), or did the mutation fail before changing the database? Keeping that
//! decision here prevents the status and hook fields from drifting between
//! install, build, upgrade, rollback, remove, autoremove, and group commands.

use anyhow::Result;
use std::path::Path;

use crate::db::{Db, Entry};
use crate::transaction::context::RecorderHandle;
use crate::transaction::history::{committed_hook_failed, PackageState, StepAction, StepStatus};

/// Record an install/restore step from the installer's result.
#[allow(clippy::too_many_arguments)]
pub(crate) fn record_install_outcome(
    recorder: &mut RecorderHandle<'_>,
    root: &Path,
    step: u32,
    package: &str,
    action: StepAction,
    before: Option<&Entry>,
    target: &PackageState,
    source: &str,
    result: &Result<Entry>,
) -> Result<()> {
    match result {
        Ok(entry) => recorder.step_result(
            step,
            package,
            action,
            StepStatus::Committed,
            before,
            Some(entry),
            source,
            None,
            None,
        ),
        Err(error) => {
            let current = Db::new(root).entry(package)?;
            let retry_hook = crate::install::hook_needs_retry(root, package).unwrap_or(false);
            let hook_committed =
                committed_hook_failed(current.as_ref(), before, target, retry_hook);
            recorder.step_result(
                step,
                package,
                action,
                if hook_committed {
                    StepStatus::CommittedHookFailed
                } else {
                    StepStatus::Failed
                },
                before,
                if hook_committed {
                    current.as_ref()
                } else {
                    None
                },
                source,
                Some(format!("{error:#}")),
                hook_committed.then(|| "failed".to_string()),
            )
        }
    }
}

/// Record a remove step from the remover's result. A removal that already
/// dropped the database entry commits even when its hook reports an error.
#[allow(clippy::too_many_arguments)]
pub(crate) fn record_remove_outcome(
    recorder: &mut RecorderHandle<'_>,
    root: &Path,
    step: u32,
    package: &str,
    action: StepAction,
    before: Option<&Entry>,
    source: &str,
    result: &Result<()>,
) -> Result<()> {
    match result {
        Ok(()) => recorder.step_result(
            step,
            package,
            action,
            StepStatus::Committed,
            before,
            None,
            source,
            None,
            None,
        ),
        Err(error) => {
            let after = Db::new(root).entry(package)?;
            let hook_committed = before.is_some() && after.is_none();
            recorder.step_result(
                step,
                package,
                action,
                if hook_committed {
                    StepStatus::CommittedHookFailed
                } else {
                    StepStatus::Failed
                },
                before,
                after.as_ref(),
                source,
                Some(format!("{error:#}")),
                hook_committed.then(|| "failed".to_string()),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use crate::testutil::{entry, tmpdir};
    use crate::transaction::context::{begin_operation, OperationCommand};
    use crate::transaction::history::{load_record, PackageState, TransactionRecord};

    fn install_record(
        root: &Path,
        result: &Result<Entry>,
        before: Option<&Entry>,
        target: &Entry,
    ) -> TransactionRecord {
        let mut op =
            begin_operation(root, OperationCommand::Install, &["demo".to_string()], &[]).unwrap();
        let tx = op.tx().unwrap();
        let mut recorder = op.recorder();
        let step = recorder
            .step_intent(
                "demo",
                StepAction::Install,
                before,
                Some(target),
                &target.artifact_sha256,
            )
            .unwrap();
        record_install_outcome(
            &mut recorder,
            root,
            step,
            "demo",
            StepAction::Install,
            before,
            &PackageState::from_entry(target),
            "archive",
            result,
        )
        .unwrap();
        op.finish(result.as_ref().err()).unwrap();
        load_record(root, tx).unwrap()
    }

    fn remove_record(
        root: &Path,
        result: &Result<()>,
        before: Option<&Entry>,
    ) -> TransactionRecord {
        let mut op =
            begin_operation(root, OperationCommand::Remove, &["demo".to_string()], &[]).unwrap();
        let tx = op.tx().unwrap();
        let mut recorder = op.recorder();
        let step = recorder
            .step_intent("demo", StepAction::Remove, before, None, "")
            .unwrap();
        record_remove_outcome(
            &mut recorder,
            root,
            step,
            "demo",
            StepAction::Remove,
            before,
            "remove",
            result,
        )
        .unwrap();
        op.finish(result.as_ref().err()).unwrap();
        load_record(root, tx).unwrap()
    }

    #[test]
    fn successful_install_records_the_committed_entry() {
        let base = tmpdir("step-install-ok");
        let root = base.join("root");
        let package = entry("demo", "1.0", &"a".repeat(64));
        let record = install_record(&root, &Ok(package.clone()), None, &package);
        let change = &record.package_changes[0];
        assert_eq!(change.status, "committed");
        assert_eq!(
            change.after.as_ref().unwrap().version.as_deref(),
            Some("1.0")
        );
        assert!(change.failure.is_none());
        assert!(change.hook.is_none());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn failed_install_that_reached_target_records_hook_failure() {
        let base = tmpdir("step-install-committed");
        let root = base.join("root");
        let package = entry("demo", "1.0", &"a".repeat(64));
        Db::new(&root)
            .add(&package, &[], &Default::default())
            .unwrap();
        let record = install_record(
            &root,
            &Err(anyhow::anyhow!("post-install hook failed")),
            None,
            &package,
        );
        let change = &record.package_changes[0];
        assert_eq!(change.status, "committed-hook-failed");
        assert_eq!(
            change.after.as_ref().unwrap().version.as_deref(),
            Some("1.0")
        );
        assert!(change
            .failure
            .as_deref()
            .unwrap()
            .contains("post-install hook failed"));
        assert_eq!(change.hook.as_deref(), Some("failed"));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn failed_install_before_commit_records_failure() {
        let base = tmpdir("step-install-failed");
        let root = base.join("root");
        let package = entry("demo", "1.0", &"a".repeat(64));
        let record = install_record(
            &root,
            &Err(anyhow::anyhow!("archive is corrupt")),
            None,
            &package,
        );
        let change = &record.package_changes[0];
        assert_eq!(change.status, "failed");
        assert!(change.after.is_none());
        assert!(change
            .failure
            .as_deref()
            .unwrap()
            .contains("archive is corrupt"));
        assert!(change.hook.is_none());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn pending_hook_marks_an_already_matching_reinstall_as_hook_failure() {
        let base = tmpdir("step-install-retry-hook");
        let root = base.join("root");
        let package = entry("demo", "1.0", &"a".repeat(64));
        let db = Db::new(&root);
        db.add(&package, &[], &Default::default()).unwrap();
        let hooks = crate::archive::Hooks {
            post_install: "true".to_string(),
            ..Default::default()
        };
        crate::install::hooks::save_hooks(&db, "demo", &hooks).unwrap();
        // No hook-status file: the hook is pending for this version.
        let record = install_record(
            &root,
            &Err(anyhow::anyhow!("hook did not finish")),
            Some(&package),
            &package,
        );
        let change = &record.package_changes[0];
        assert_eq!(change.status, "committed-hook-failed");
        assert_eq!(change.hook.as_deref(), Some("failed"));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn successful_remove_records_committed() {
        let base = tmpdir("step-remove-ok");
        let root = base.join("root");
        let package = entry("demo", "1.0", &"a".repeat(64));
        let record = remove_record(&root, &Ok(()), Some(&package));
        let change = &record.package_changes[0];
        assert_eq!(change.status, "committed");
        assert!(change.after.is_none());
        assert!(change.failure.is_none());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn failed_remove_after_db_removal_records_hook_failure() {
        let base = tmpdir("step-remove-committed");
        let root = base.join("root");
        let package = entry("demo", "1.0", &"a".repeat(64));
        let db = Db::new(&root);
        db.add(&package, &[], &Default::default()).unwrap();
        db.remove("demo").unwrap();
        let record = remove_record(
            &root,
            &Err(anyhow::anyhow!("post-remove hook failed")),
            Some(&package),
        );
        let change = &record.package_changes[0];
        assert_eq!(change.status, "committed-hook-failed");
        assert!(change.after.is_none());
        assert!(change
            .failure
            .as_deref()
            .unwrap()
            .contains("post-remove hook failed"));
        assert_eq!(change.hook.as_deref(), Some("failed"));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn failed_remove_before_db_removal_records_failure() {
        let base = tmpdir("step-remove-failed");
        let root = base.join("root");
        let package = entry("demo", "1.0", &"a".repeat(64));
        Db::new(&root)
            .add(&package, &[], &Default::default())
            .unwrap();
        let record = remove_record(&root, &Err(anyhow::anyhow!("file is busy")), Some(&package));
        let change = &record.package_changes[0];
        assert_eq!(change.status, "failed");
        assert_eq!(
            change.after.as_ref().unwrap().version.as_deref(),
            Some("1.0")
        );
        assert!(change.failure.as_deref().unwrap().contains("file is busy"));
        assert!(change.hook.is_none());
        let _ = std::fs::remove_dir_all(&base);
    }
}
