//! Write-ahead package-step journal (roadmap §5.5).
//!
//! The package database and the history records are separate files, so no
//! single rename can make both atomic. Before every package mutation the
//! intent is persisted here; after the db commit the result is appended and
//! the step marked committed. A later `reconcile` compares the intent with
//! the actual db state and records what really happened.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use super::atomic;
use super::history::{PackageState, SCHEMA_VERSION};

/// `<root>/var/lib/nawa/history/journal/<id>/`
fn journal_dir(root: &Path, tx: u64) -> PathBuf {
    super::history::history_dir(root).join("journal").join(format!("{tx:010}"))
}

/// The intent of one not-yet-applied package step.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct StepIntent {
    pub schema: u32,
    pub tx: u64,
    pub step: u32,
    pub package: String,
    /// "install" | "upgrade" | "downgrade" | "reinstall" | "remove" | "restore"
    pub action: String,
    /// Expected package identity before the mutation (absent = not installed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub precondition: Option<PackageState>,
    /// Desired identity after the mutation (absent = removed).
    #[allow(dead_code)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<PackageState>,
    /// Content-addressed archive reference used by the step, when there is one.
    #[allow(dead_code)]
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub artifact_sha256: String,
}

/// The observed outcome of one step.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct StepResult {
    pub schema: u32,
    pub tx: u64,
    pub step: u32,
    pub package: String,
    /// "committed" | "failed" | "committed-hook-failed"
    pub outcome: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hook: Option<String>,
}

fn intent_path(root: &Path, tx: u64, step: u32) -> PathBuf {
    journal_dir(root, tx).join("steps").join(format!("intent-{step:04}.toml"))
}

fn result_path(root: &Path, tx: u64, step: u32) -> PathBuf {
    journal_dir(root, tx).join("steps").join(format!("result-{step:04}.toml"))
}

/// Transaction-level marker: state.toml with the running status.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct TxState {
    pub schema: u32,
    pub tx: u64,
    pub command: String,
    pub started_at: u64,
}

/// Persist the write-ahead intent for one step (atomic temp+rename).
pub(crate) fn write_intent(root: &Path, intent: &StepIntent) -> Result<()> {
    if intent.schema != SCHEMA_VERSION {
        anyhow::bail!("unsupported step-intent schema {}", intent.schema);
    }
    crate::db::check_name(&intent.package)?;
    let path = intent_path(root, intent.tx, intent.step);
    atomic::write_atomic(&path, toml::to_string_pretty(intent)?.as_bytes(), 0o600)
}

/// Record the observed outcome of one step.
pub(crate) fn write_result(root: &Path, result: &StepResult) -> Result<()> {
    if result.schema != SCHEMA_VERSION {
        anyhow::bail!("unsupported step-result schema {}", result.schema);
    }
    crate::db::check_name(&result.package)?;
    let path = result_path(root, result.tx, result.step);
    atomic::write_atomic(&path, toml::to_string_pretty(result)?.as_bytes(), 0o600)
}

/// Write the transaction-level running marker.
pub(crate) fn write_tx_state(root: &Path, state: &TxState) -> Result<()> {
    let dir = journal_dir(root, state.tx);
    let path = dir.join("state.toml");
    atomic::write_atomic(&path, toml::to_string_pretty(state)?.as_bytes(), 0o600)
}

/// Read one step's intent, if it was written.
pub(crate) fn read_intent(root: &Path, tx: u64, step: u32) -> Result<Option<StepIntent>> {
    let path = intent_path(root, tx, step);
    if !path.exists() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(&path)?;
    Ok(Some(toml::from_str(&raw).context("corrupt step intent")?))
}

/// Read one step's result, if it was written.
pub(crate) fn read_result(root: &Path, tx: u64, step: u32) -> Result<Option<StepResult>> {
    let path = result_path(root, tx, step);
    if !path.exists() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(&path)?;
    Ok(Some(toml::from_str(&raw).context("corrupt step result")?))
}

/// Every step intent paired with its durable result when one exists.
pub(crate) fn transaction_steps(root: &Path, tx: u64) -> Result<Vec<(StepIntent, Option<StepResult>)>> {
    let dir = journal_dir(root, tx).join("steps");
    let rd = match std::fs::read_dir(&dir) {
        Ok(rd) => rd,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).with_context(|| format!("read journal {}", dir.display())),
    };
    let mut steps: Vec<u32> = rd.filter_map(|entry| entry.ok()).filter_map(|entry| {
        let name = entry.file_name().to_string_lossy().to_string();
        name.strip_prefix("intent-").and_then(|value| value.strip_suffix(".toml"))
            .and_then(|value| value.parse::<u32>().ok())
    }).collect();
    steps.sort_unstable();
    steps.dedup();
    let mut out = Vec::with_capacity(steps.len());
    for step in steps {
        let intent = read_intent(root, tx, step)?
            .ok_or_else(|| anyhow::anyhow!("journal step {tx}/{step} disappeared while reading"))?;
        if intent.schema != SCHEMA_VERSION || intent.tx != tx || intent.step != step {
            anyhow::bail!("journal intent identity mismatch for transaction {tx}, step {step}");
        }
        let result = read_result(root, tx, step)?;
        if let Some(result) = &result {
            if result.schema != SCHEMA_VERSION || result.tx != tx
                || result.step != step || result.package != intent.package {
                anyhow::bail!("journal result identity mismatch for transaction {tx}, step {step}");
            }
        }
        out.push((intent, result));
    }
    Ok(out)
}

/// Every intent without a durable result.
pub(crate) fn pending_steps(root: &Path, tx: u64) -> Result<Vec<(u32, StepIntent)>> {
    Ok(transaction_steps(root, tx)?.into_iter()
        .filter_map(|(intent, result)| result.is_none().then_some((intent.step, intent)))
        .collect())
}

/// Every transaction id that has a journal directory.
pub(crate) fn journaled_transactions(root: &Path) -> Result<Vec<u64>> {
    let dir = super::history::history_dir(root).join("journal");
    let mut out = Vec::new();
    let rd = match std::fs::read_dir(&dir) {
        Ok(rd) => rd,
        Err(_) => return Ok(out),
    };
    for e in rd.filter_map(|e| e.ok()) {
        let n = e.file_name().to_string_lossy().to_string();
        if let Some(id) = e
            .path()
            .is_dir()
            .then(|| n.parse::<u64>().ok())
            .flatten()
        {
            out.push(id);
        }
    }
    out.sort_unstable();
    Ok(out)
}

/// Remove the journal of a finished transaction (after its record is fully
/// written). Interrupted/needs-recovery journals are kept.
pub(crate) fn clear_journal(root: &Path, tx: u64) -> Result<()> {
    let dir = journal_dir(root, tx);
    if dir.exists() {
        std::fs::remove_dir_all(&dir).with_context(|| format!("remove journal {}", dir.display()))?;
        if let Some(parent) = dir.parent() { atomic::sync_directory(parent)?; }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transaction::history::PackageState;

    use crate::testutil::tmpdir;

    fn intent(tx: u64, step: u32, pkg: &str) -> StepIntent {
        StepIntent {
            schema: SCHEMA_VERSION,
            tx,
            step,
            package: pkg.into(),
            action: "install".into(),
            precondition: None,
            target: Some(PackageState {
                version: Some("1.0".into()),
                rel: Some(1),
                arch: String::new(),
                reason: "explicit".to_string(),
                artifact_sha256: String::new(),
            }),
            artifact_sha256: String::new(),
        }
    }

    #[test]
    fn intent_result_and_pending_steps() {
        let base = tmpdir("wal");
        let root = base.join("root");
        write_tx_state(&root, &TxState { schema: 1, tx: 7, command: "install".into(), started_at: 5 }).unwrap();
        write_intent(&root, &intent(7, 1, "zlib")).unwrap();
        write_intent(&root, &intent(7, 2, "curl")).unwrap();

        let pending = pending_steps(&root, 7).unwrap();
        assert_eq!(pending.len(), 2);
        assert_eq!(pending[0].1.package, "zlib");

        write_result(&root, &StepResult {
            schema: 1, tx: 7, step: 1, package: "zlib".into(),
            outcome: "committed".into(), failure: None, hook: None,
        }).unwrap();
        let pending = pending_steps(&root, 7).unwrap();
        assert_eq!(pending.len(), 1, "a step with a result is no longer pending");
        assert_eq!(pending[0].1.package, "curl");
        let all = transaction_steps(&root, 7).unwrap();
        assert_eq!(all.len(), 2);
        assert!(all[0].1.is_some());
        assert!(all[1].1.is_none());

        assert_eq!(journaled_transactions(&root).unwrap(), vec![7]);
        clear_journal(&root, 7).unwrap();
        assert!(pending_steps(&root, 7).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn path_like_package_names_are_rejected() {
        let base = tmpdir("evil");
        let root = base.join("root");
        let mut i = intent(1, 1, "../evil");
        i.package = "../evil".into();
        assert!(write_intent(&root, &i).is_err());
        let _ = std::fs::remove_dir_all(&base);
    }
}
