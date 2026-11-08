//! Durable post-transaction observer events.
//!
//! OperationHandle publishes a compact outbox record while the DB lock is
//! held. Nawa drains it after run() returns and the lock has been released.
//! Observer errors are stored as diagnostics and never reverse package state.

use anyhow::{Context, Result};
use serde::Serialize;
use rustix::fd::AsFd;
use rustix::fs::{flock, FlockOperation};
use std::fs::{File, OpenOptions};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::transaction::history::TransactionRecord;
use super::discovery::{self, PluginState};
use super::manifest::Capability;
use super::runner::{PluginProcess, DEFAULT_TIMEOUT};

fn outbox_dir(root: &Path) -> PathBuf {
    root.join("var/lib/nawa/plugins/outbox")
}

fn failures_dir(root: &Path) -> PathBuf {
    root.join("var/lib/nawa/plugins/failures")
}

static FAILURE_SEQUENCE: AtomicU64 = AtomicU64::new(1);

/// Serialize event dispatchers without holding the package database lock.
/// A non-blocking lock also avoids deadlocking a plugin that invokes Nawa.
fn acquire_dispatch_lock(dir: &Path) -> Result<Option<File>> {
    let path = dir.join(".dispatch.lock");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
.create(true)
        .truncate(false)
        .mode(0o600)
        .open(&path)
        .with_context(|| format!("open plugin dispatch lock {}", path.display()))?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    loop {
        match flock(file.as_fd(), FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => return Ok(Some(file)),
            Err(error) => {
                let error = std::io::Error::from(error);
                match error.kind() {
                    std::io::ErrorKind::Interrupted => continue,
                    std::io::ErrorKind::WouldBlock => return Ok(None),
                    _ => return Err(error)
                        .with_context(|| format!("lock plugin event outbox {}", dir.display())),
                }
            }
        }
    }
}

/// Queue one finalized transaction. The rename publishes a complete record.
pub fn enqueue_transaction(root: &Path, record: &TransactionRecord) -> Result<()> {
    let dir = outbox_dir(root);
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    let target = dir.join(format!("{:010}.toml", record.id));
    let temporary = dir.join(format!(
        ".{:010}.{}.{}.tmp",
        record.id,
        std::process::id(),
        crate::artifact::now_secs()
    ));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)?;
    use std::io::Write as _;
    file.write_all(toml::to_string(record)?.as_bytes())?;
    file.sync_all()?;
    std::fs::rename(&temporary, &target)
        .with_context(|| format!("publish transaction observer event {}", target.display()))?;
    Ok(())
}

#[derive(Debug, Serialize)]
struct ObserverFailure {
    schema: u32,
    transaction_id: u64,
    plugin_id: String,
    event_type: String,
    error: String,
    recorded_at: u64,
}

/// Dispatch pending events. Malformed records remain in the outbox for review.
pub fn dispatch_pending(
    root: &Path,
    project_config: Option<&Path>,
    system_config: Option<&Path>,
) {
    let dir = outbox_dir(root);
    if !dir.is_dir() || std::env::var_os(crate::lock::HELD_ENV).is_some() {
        return;
    }
    let _dispatch_lock = match acquire_dispatch_lock(&dir) {
        Ok(Some(lock)) => lock,
        Ok(None) => return,
        Err(error) => {
            // The outbox is root-owned. An unprivileged run (`nawa update`,
            // queries) cannot lock it and has nothing to deliver anyway: the
            // pending events go out with the next privileged command, so
            // this is not worth a warning.
            if crate::diag::has_io_kind(&error, std::io::ErrorKind::PermissionDenied)
                && !crate::diag::is_root()
            {
                return;
            }
            crate::nawa_warn!("could not lock plugin event outbox: {error:#}");
            return;
        }
    };
    let Ok(entries) = std::fs::read_dir(&dir) else { return };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok().map(|item| item.path()))
        .filter(|path| path.extension().is_some_and(|extension| extension == "toml"))
        .collect();
    paths.sort();

    for path in paths {
        let record = match load_event(&path) {
            Ok(record) => record,
            Err(error) => {
                crate::nawa_warn!("cannot load observer event {}: {error:#}", path.display());
                continue;
            }
        };
        let mut diagnostics_complete = true;
        for failure in dispatch_record(root, project_config, system_config, &record) {
            crate::nawa_warn!(
                "observer plugin {} failed for {}: {}",
                failure.plugin_id, failure.event_type, failure.error
            );
            if let Err(error) = store_failure(root, &failure) {
                diagnostics_complete = false;
                crate::nawa_warn!("could not store observer failure: {error:#}");
            }
        }
        // Retain the event if its failure diagnostics could not be persisted.
        if !diagnostics_complete {
            continue;
        }
        if let Err(error) = std::fs::remove_file(&path) {
            crate::nawa_warn!("could not remove dispatched event {}: {error}", path.display());
        }
    }
}

fn load_event(path: &Path) -> Result<TransactionRecord> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("read observer event {}", path.display()))?;
    let record: TransactionRecord = toml::from_str(&raw)
        .with_context(|| format!("parse observer event {}", path.display()))?;
    record.validate()?;
    Ok(record)
}

fn dispatch_record(
    root: &Path,
    project_config: Option<&Path>,
    system_config: Option<&Path>,
    record: &TransactionRecord,
) -> Vec<ObserverFailure> {
    let state = PluginState::load_effective(project_config, system_config);
    let discovered = match discovery::discover(rustix::process::geteuid().is_root()) {
        Ok(plugins) => plugins,
        Err(error) => {
            return vec![failure(record.id, "discovery", "transaction.observe", format!("{error:#}"))];
        }
    };
    let events = transaction_events(record);
    let protected = protected_paths(root, project_config, system_config);
    let mut failures = Vec::new();

    for plugin in discovered {
        if plugin.error.is_some() || !state.is_enabled(&plugin.manifest.id) {
            continue;
        }
        if !state.has_grant(&plugin.manifest.id, Capability::TransactionObserve) {
            continue;
        }
        let wanted: Vec<_> = events.iter()
            .filter(|(event_type, _)| plugin.manifest.events.iter().any(|name| name == event_type))
            .collect();
        if wanted.is_empty() {
            continue;
        }

        let granted = state.granted_capabilities(
            &plugin.manifest.id,
            &plugin.manifest.requested_capabilities,
        );
        let mut process = match PluginProcess::start_with_policy(
            &plugin.manifest,
            &granted,
            &protected,
            DEFAULT_TIMEOUT,
        ) {
            Ok(process) => process,
            Err(error) => {
                failures.push(failure(record.id, &plugin.manifest.id, "startup", format!("{error:#}")));
                continue;
            }
        };
        for (event_type, change_index) in wanted {
            let payload = event_payload(record, event_type, *change_index, &granted);
            if let Err(error) = process.notify_event(event_type, &payload, DEFAULT_TIMEOUT) {
                failures.push(failure(record.id, &plugin.manifest.id, event_type, format!("{error:#}")));
                break;
            }
        }
    }
    failures
}

fn protected_paths(
    root: &Path,
    project_config: Option<&Path>,
    system_config: Option<&Path>,
) -> Vec<PathBuf> {
    let mut paths = vec![root.to_path_buf()];
    for config in [project_config, system_config].into_iter().flatten() {
        if let Some(parent) = config.parent() {
            paths.push(parent.to_path_buf());
        }
    }
    paths
}

fn transaction_events(record: &TransactionRecord) -> Vec<(String, Option<usize>)> {
    let mut events = Vec::new();
    let transaction_event = match record.status.as_str() {
        "completed" => Some("transaction.completed"),
        "failed" => Some("transaction.failed"),
        "partial" => Some("transaction.partial"),
        _ => None,
    };
    if let Some(event_type) = transaction_event {
        events.push((event_type.to_string(), None));
    }
    for (index, change) in record.package_changes.iter().enumerate() {
        if change.status != "committed" && change.status != "committed-hook-failed" {
            continue;
        }
        let event_type = match change.action.as_str() {
            "install" => "package.installed",
            "upgrade" | "reinstall" => "package.upgraded",
            "restore" if change.before.is_none() => "package.installed",
            "restore" => "package.upgraded",
            "remove" => "package.removed",
            _ => continue,
        };
        events.push((event_type.to_string(), Some(index)));
    }
    events
}

fn event_payload(
    record: &TransactionRecord,
    event_type: &str,
    change_index: Option<usize>,
    granted: &[Capability],
) -> serde_json::Value {
    let mut payload = serde_json::json!({
        "event_type": event_type,
        "transaction_id": record.id,
        "command": record.command,
        "status": record.status,
    });
    let Some(object) = payload.as_object_mut() else { return payload };

    if granted.contains(&Capability::HistoryRead) {
        object.insert("started_at".into(), record.started_at.into());
        object.insert("finished_at".into(), serde_json::to_value(record.finished_at).unwrap_or(serde_json::Value::Null));
        object.insert("duration_seconds".into(), serde_json::to_value(record.duration_seconds).unwrap_or(serde_json::Value::Null));
        object.insert("initiated_by".into(), record.initiated_by.clone().into());
        object.insert("undoes".into(), serde_json::to_value(record.undoes).unwrap_or(serde_json::Value::Null));
        object.insert("undone_by".into(), serde_json::to_value(record.undone_by).unwrap_or(serde_json::Value::Null));
    }
    if granted.contains(&Capability::RepositoryMetadataRead) {
        object.insert("repository_ids".into(), serde_json::to_value(&record.repository_ids).unwrap_or(serde_json::Value::Null));
    }
    if granted.contains(&Capability::PackagePlanInspect) {
        let selected: Vec<_> = match change_index {
            Some(index) => record.package_changes.get(index).into_iter().collect(),
            None => record.package_changes.iter().collect(),
        };
        let changes: Vec<serde_json::Value> = selected
        .into_iter()
        .map(|change| serde_json::json!({
            "name": change.name,
            "action": change.action,
            "status": change.status,
            "source": change.source,
            "failure": change.failure,
            "hook": change.hook,
        }))
        .collect();
        object.insert("package_changes".into(), serde_json::Value::Array(changes));
        object.insert("requested_packages".into(), serde_json::to_value(&record.requested_packages).unwrap_or(serde_json::Value::Null));
    }
    payload
}

fn failure(tx: u64, plugin: &str, event: &str, error: String) -> ObserverFailure {
    ObserverFailure {
        schema: 1,
        transaction_id: tx,
        plugin_id: plugin.to_string(),
        event_type: event.to_string(),
        error,
        recorded_at: crate::artifact::now_secs(),
    }
}

fn store_failure(root: &Path, failure: &ObserverFailure) -> Result<()> {
    let dir = failures_dir(root);
    std::fs::create_dir_all(&dir)?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    let sequence = FAILURE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let target = dir.join(format!(
        "{:010}-{}-{}-{}-{}-{sequence}.toml",
        failure.transaction_id,
        failure.plugin_id,
        failure.event_type,
        failure.recorded_at,
        std::process::id()
    ));
    let temporary = dir.join(format!(".{}.{}.tmp", failure.transaction_id, std::process::id()));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)?;
    use std::io::Write as _;
    file.write_all(toml::to_string(failure)?.as_bytes())?;
    file.sync_all()?;
    std::fs::rename(temporary, target)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transaction::history::PackageChange;

    fn sample_record(status: &str) -> TransactionRecord {
        TransactionRecord {
            schema: 1,
            id: 7,
            command: "install".into(),
            status: status.into(),
            started_at: 100,
            finished_at: Some(112),
            duration_seconds: Some(12),
            initiated_by: "uid:1000".into(),
            requested_packages: vec!["curl".into()],
            repository_ids: vec!["main".into()],
            package_changes: vec![PackageChange {
                name: "curl".into(),
                action: "install".into(),
                status: "committed".into(),
                before: None,
                after: None,
                source: "main".into(),
                failure: None,
                hook: None,
            }],
            undone_by: None,
            undoes: None,
            groups: Vec::new(),
        }
    }

    #[test]
    fn completed_install_emits_transaction_and_package_events() {
        let events = transaction_events(&sample_record("completed"));
        assert!(events.contains(&("transaction.completed".into(), None)));
        assert!(events.contains(&("package.installed".into(), Some(0))));
    }

    #[test]
    fn observer_payload_is_redacted_until_capabilities_are_granted() {
        let record = sample_record("completed");
        let minimal = event_payload(&record, "transaction.completed", None, &[]);
        assert_eq!(minimal["transaction_id"], 7);
        assert!(minimal.get("started_at").is_none());
        assert!(minimal.get("repository_ids").is_none());
        assert!(minimal.get("package_changes").is_none());

        let granted = event_payload(
            &record,
            "transaction.completed",
            None,
            &[
                Capability::HistoryRead,
                Capability::RepositoryMetadataRead,
                Capability::PackagePlanInspect,
            ],
        );
        assert_eq!(granted["started_at"], 100);
        assert_eq!(granted["repository_ids"][0], "main");
        assert_eq!(granted["package_changes"][0]["name"], "curl");
    }

    #[test]
    fn failed_and_partial_transactions_map_to_distinct_events() {
        assert!(transaction_events(&sample_record("failed"))
            .contains(&("transaction.failed".into(), None)));
        assert!(transaction_events(&sample_record("partial"))
            .contains(&("transaction.partial".into(), None)));
    }
}
