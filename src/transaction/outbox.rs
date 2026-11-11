//! Producer side of the plugin observer outbox.
//!
//! A finalized transaction record is published here (atomically, 0o600 in a
//! 0o700 directory) while the database lock is still held. The *consumer* —
//! `plugins::events` — drains the outbox after the lock is released. Keeping
//! the write side in `transaction` means the transaction layer never has to
//! know that plugins exist: plugins depend on transactions, not the reverse.

use anyhow::{Context, Result};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use super::history::TransactionRecord;

pub(crate) fn outbox_dir(root: &Path) -> PathBuf {
    root.join("var/lib/nawa/plugins/outbox")
}

/// Queue one finalized transaction. The rename publishes a complete record.
pub(crate) fn enqueue_transaction(root: &Path, record: &TransactionRecord) -> Result<()> {
    let dir = outbox_dir(root);
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    let target = dir.join(format!("{:010}.toml", record.id));
    crate::atomic::write_atomic(
        &target,
        toml::to_string(record)?.as_bytes(),
        0o600,
    )
    .with_context(|| format!("publish transaction observer event {}", target.display()))
}
