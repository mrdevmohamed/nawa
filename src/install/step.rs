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
