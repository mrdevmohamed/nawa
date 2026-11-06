//! Transaction record schema and the history store (roadmap §5.2/§5.3).

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::db::{self, Entry};

pub const SCHEMA_VERSION: u32 = 1;

/// `<root>/var/lib/nawa/history`
pub fn history_dir(root: &Path) -> PathBuf {
    root.join("var/lib/nawa/history")
}

fn transactions_dir(root: &Path) -> PathBuf {
    history_dir(root).join("transactions")
}

fn transaction_path(root: &Path, id: u64) -> PathBuf {
    transactions_dir(root).join(format!("{id:010}.toml"))
}

/// The state machine of a transaction (roadmap §5.4). `interrupted` is NOT
/// `failed`: package state may have changed before the process died.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TxStatus {
    /// ID allocated, plan being assembled.
    Preparing,
    /// At least one step may execute.
    Running,
    /// Every planned step completed.
    Completed,
    /// Some steps committed, a later one failed.
    Partial,
    /// Failed before any package state changed.
    Failed,
    /// The process ended while the transaction was active.
    Interrupted,
    /// Reconciliation cannot safely infer the result.
    NeedsRecovery,
    /// A compensating transaction completed; this record is kept as-is.
    Undone,
}

impl TxStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            TxStatus::Preparing => "preparing",
            TxStatus::Running => "running",
            TxStatus::Completed => "completed",
            TxStatus::Partial => "partial",
            TxStatus::Failed => "failed",
            TxStatus::Interrupted => "interrupted",
            TxStatus::NeedsRecovery => "needs-recovery",
            TxStatus::Undone => "undone",
        }
    }
}

/// What one step did to one package.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StepAction {
    Install,
    Upgrade,
    Downgrade,
    Reinstall,
    Remove,
    Restore,
}

impl StepAction {
    /// Stable serialization of the step action.
    #[allow(dead_code)]
    pub fn as_str(self) -> &'static str {
        match self {
            StepAction::Install => "install",
            StepAction::Upgrade => "upgrade",
            StepAction::Downgrade => "downgrade",
            StepAction::Reinstall => "reinstall",
            StepAction::Remove => "remove",
            StepAction::Restore => "restore",
        }
    }
}

/// Per-step status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StepStatus {
    /// The package database commit for this step finished.
    Committed,
    /// The step was planned but never reached (later step failed first).
    Skipped,
    /// The step itself failed; the package is at its before-state.
    Failed,
    /// Committed, but the package hook failed afterwards. The package files
    /// were NOT rolled back — record that honestly.
    CommittedHookFailed,
}

impl StepStatus {
    /// Stable serialization of the step status.
    #[allow(dead_code)]
    pub fn as_str(self) -> &'static str {
        match self {
            StepStatus::Committed => "committed",
            StepStatus::Skipped => "skipped",
            StepStatus::Failed => "failed",
            StepStatus::CommittedHookFailed => "committed-hook-failed",
        }
    }

    #[allow(dead_code)]
    pub fn committed(self) -> bool {
        matches!(self, StepStatus::Committed | StepStatus::CommittedHookFailed)
    }
}

/// Snapshot of one package's identity at a point in time.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PackageState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rel: Option<u32>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub arch: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reason: String,
    /// The exact archive bytes (content-addressed reference).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub artifact_sha256: String,
}

impl PackageState {
    pub fn from_entry(e: &Entry) -> Self {
        PackageState {
            version: Some(e.version.clone()),
            rel: Some(e.rel),
            arch: e.arch.clone(),
            reason: e.reason.clone(),
            artifact_sha256: e.artifact_sha256.clone(),
        }
    }

    pub fn absent() -> Self {
        PackageState::default()
    }

    pub fn is_installed(&self) -> bool {
        self.version.is_some()
    }

    /// Identity + artifact digest comparison (roadmap §5.9): the same version
    /// string can hide different bytes, so the digest decides.
    pub fn matches_entry(&self, e: &Entry) -> bool {
        let identity = match (&self.version, self.rel) {
            (Some(v), Some(r)) => *v == e.version && r == e.rel,
            (None, None) => true,
            _ => false,
        };
        if !identity {
            return false;
        }
        if self.artifact_sha256.is_empty() || e.artifact_sha256.is_empty() {
            // legacy records without a digest fall back to identity only
            return true;
        }
        self.artifact_sha256 == e.artifact_sha256
    }
}

/// One package change inside a transaction.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackageChange {
    pub name: String,
    pub action: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<PackageState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<PackageState>,
    /// Where the archive came from: repository id, "local-build", "archive".
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub source: String,
    /// Safe failure summary when the step failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<String>,
    /// Hook outcome for this step, when one ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hook: Option<String>,
}

/// An immutable record of one completed (or interrupted) transaction.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransactionRecord {
    pub schema: u32,
    pub id: u64,
    /// Normalized command kind ("install", "update", "history-undo", ...).
    pub command: String,
    pub status: String,
    /// Unix seconds.
    pub started_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_seconds: Option<u64>,
    /// "uid:<n>" — no usernames, no environment, no arguments dump.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub initiated_by: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requested_packages: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub repository_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub package_changes: Vec<PackageChange>,
    /// Set on the original when a compensating undo completed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub undone_by: Option<u64>,
    /// Set on the compensating transaction: which id it undoes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub undoes: Option<u64>,
    /// Group ids this transaction installed/removed (Phase 5).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<String>,
}

impl TransactionRecord {
    pub fn status(&self) -> Result<TxStatus> {
        parse_status(&self.status)
    }

    /// Validate names and references that could escape their roots
    /// (roadmap §5.3: reject path-like or malformed values on load).
    pub fn validate(&self) -> Result<()> {
        if self.schema != SCHEMA_VERSION {
            bail!("unsupported transaction schema {} (expected {SCHEMA_VERSION})", self.schema);
        }
        if self.id == 0 {
            bail!("transaction id 0 is invalid");
        }
        parse_status(&self.status)?;
        for c in &self.package_changes {
            db::check_name(&c.name)?;
            if !c.before.as_ref().is_some_and(|b| b.version.is_none() || db::valid_version(b.version.as_deref().unwrap_or("")))
                && c.before.is_some()
            {
                let v = c.before.as_ref().and_then(|b| b.version.clone()).unwrap_or_default();
                if !v.is_empty() && !db::valid_version(&v) {
                    bail!("transaction {} has an invalid before-version for {}", self.id, c.name);
                }
            }
            if let Some(a) = &c.after {
                if let Some(v) = &a.version {
                    if !v.is_empty() && !db::valid_version(v) {
                        bail!("transaction {} has an invalid after-version for {}", self.id, c.name);
                    }
                }
            }
            if !a_artifact_ok(c.before.as_ref()) || !a_artifact_ok(c.after.as_ref()) {
                bail!("transaction {} has an invalid artifact reference", self.id);
            }
        }
        for g in &self.groups {
            crate::groups::check_group_id(g)?;
        }
        Ok(())
    }

    pub fn to_toml(&self) -> Result<String> {
        Ok(toml::to_string_pretty(self)?)
    }

    pub fn from_toml(raw: &str) -> Result<Self> {
        let r: TransactionRecord =
            toml::from_str(raw).context("corrupt transaction record")?;
        r.validate()?;
        Ok(r)
    }
}

fn a_artifact_ok(s: Option<&PackageState>) -> bool {
    match s {
        None => true,
        Some(st) if st.artifact_sha256.is_empty() => true,
        Some(st) => crate::artifact::validate_digest(&st.artifact_sha256).is_ok(),
    }
}

pub fn parse_status(s: &str) -> Result<TxStatus> {
    match s {
        "preparing" => Ok(TxStatus::Preparing),
        "running" => Ok(TxStatus::Running),
        "completed" => Ok(TxStatus::Completed),
        "partial" => Ok(TxStatus::Partial),
        "failed" => Ok(TxStatus::Failed),
        "interrupted" => Ok(TxStatus::Interrupted),
        "needs-recovery" => Ok(TxStatus::NeedsRecovery),
        "undone" => Ok(TxStatus::Undone),
        other => bail!("unknown transaction status {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// store operations (caller holds the exclusive db lock)
// ---------------------------------------------------------------------------

/// Allocate the next monotonic transaction id by updating `last-id`.
/// Gaps after crashes are acceptable; reusing an id is not.
pub fn allocate_id(root: &Path) -> Result<u64> {
    let dir = history_dir(root);
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("create {}", dir.display()))?;
    let last_path = dir.join("last-id");
    let next = match std::fs::read_to_string(&last_path) {
        Ok(raw) => raw.trim().parse::<u64>().context("corrupt history last-id")? + 1,
        Err(_) => 1,
    };
    let tmp = dir.join(".last-id.tmp");
    std::fs::write(&tmp, next.to_string())?;
    std::fs::rename(&tmp, &last_path)?;
    Ok(next)
}

/// Write a transaction record atomically (temp file + rename). Records are
/// immutable once written EXCEPT the narrowly defined administrative
/// annotations (`undone_by`, status transitions out of interrupted states).
pub fn write_record(root: &Path, record: &TransactionRecord) -> Result<()> {
    record.validate()?;
    let dir = transactions_dir(root);
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("create {}", dir.display()))?;
    let path = transaction_path(root, record.id);
    let tmp = dir.join(format!(".{}.{}.tmp", record.id, std::process::id()));
    let write = (|| -> Result<()> {
        std::fs::write(&tmp, record.to_toml()?)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    })();
    if write.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    write
}

/// Replace an existing record (only for the allowed annotations: status
/// reconciliation, `undone_by`). The old record is kept in `.old/`.
pub fn rewrite_record(root: &Path, record: &TransactionRecord) -> Result<()> {
    record.validate()?;
    let path = transaction_path(root, record.id);
    if !path.exists() {
        bail!("cannot rewrite transaction {}: no such record", record.id);
    }
    let old_dir = transactions_dir(root).join(".old");
    std::fs::create_dir_all(&old_dir)?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let _ = std::fs::copy(&path, old_dir.join(format!("{}-{stamp}", record.id)));
    write_record(root, record)
}

/// Return whether a durable compensating transaction restored every
/// committed package in the original transaction to its recorded before-state.
/// A Partial undo can still be complete at the package level when a post-operation
/// hook failed after the package mutation was committed.
pub fn undo_restored_original(root: &Path, original_id: u64, undo_id: u64) -> Result<bool> {
    let undo = load_record(root, undo_id)?;
    if undo.undoes != Some(original_id)
        || !matches!(undo.status()?, TxStatus::Completed | TxStatus::Partial)
    {
        return Ok(false);
    }
    let original = load_record(root, original_id)?;
    let db = crate::db::Db::new(root);
    for change in original.package_changes.iter().filter(|change| {
        change.status == "committed" || change.status == "committed-hook-failed"
    }) {
        let current = db.entry(&change.name)?;
        let desired = change.before.clone().unwrap_or_else(PackageState::absent);
        let restored = if desired.is_installed() {
            current.as_ref().is_some_and(|entry| desired.matches_entry(entry))
        } else {
            current.is_none()
        };
        if !restored { return Ok(false); }
    }
    Ok(true)
}

/// Mark the original transaction undone only after its compensating transaction
/// is durable and the live package database matches the original before-state.
pub fn mark_undone_by(root: &Path, original_id: u64, undo_id: u64) -> Result<()> {
    if !undo_restored_original(root, original_id, undo_id)? {
        bail!("transaction {undo_id} has not fully restored transaction {original_id}");
    }
    let mut original = load_record(root, original_id)?;
    match original.undone_by {
        Some(existing) if existing == undo_id => return Ok(()),
        Some(existing) => bail!("transaction {original_id} was already undone by transaction {existing}"),
        None => {}
    }
    original.undone_by = Some(undo_id);
    rewrite_record(root, &original)
}

/// Load one record by id.
pub fn load_record(root: &Path, id: u64) -> Result<TransactionRecord> {
    let path = transaction_path(root, id);
    if !path.exists() {
        bail!("no transaction {id} in the history");
    }
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("read transaction {id}"))?;
    TransactionRecord::from_toml(&raw).with_context(|| format!("transaction {id} is corrupt"))
}

/// List every record, oldest first (by numeric id, not by filename alone).
pub fn list_records(root: &Path) -> Result<Vec<TransactionRecord>> {
    let dir = transactions_dir(root);
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut ids: Vec<u64> = std::fs::read_dir(&dir)?
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            name.strip_suffix(".toml").and_then(|s| s.parse::<u64>().ok())
        })
        .collect();
    ids.sort_unstable();
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        // a record that fails validation is surfaced as an error, not skipped
        out.push(load_record(root, id)?);
    }
    Ok(out)
}

/// Collect every artifact digest referenced by any record (mark phase of
/// the clean/retention model, roadmap §5.10 / §8.11).
#[allow(dead_code)]
pub fn referenced_artifacts(root: &Path) -> Result<std::collections::BTreeSet<String>> {
    let mut out = std::collections::BTreeSet::new();
    for r in list_records(root)? {
        for c in &r.package_changes {
            for s in c.before.iter().chain(c.after.iter()) {
                if !s.artifact_sha256.is_empty() {
                    out.insert(s.artifact_sha256.clone());
                }
            }
        }
    }
    Ok(out)
}

/// Snapshots of the current db, keyed by name (used by the operation context).
#[allow(dead_code)]
pub fn installed_snapshot(root: &Path) -> Result<BTreeMap<String, Entry>> {
    let db = db::Db::new(root);
    let mut out = BTreeMap::new();
    for e in db.installed()? {
        out.insert(e.name.clone(), e);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("nawa-hist-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn sample(id: u64) -> TransactionRecord {
        TransactionRecord {
            schema: SCHEMA_VERSION,
            id,
            command: "update".into(),
            status: "completed".into(),
            started_at: 1_793_910_000,
            finished_at: Some(1_793_910_320),
            duration_seconds: Some(320),
            initiated_by: "uid:0".into(),
            requested_packages: vec!["curl".into(), "openssl".into()],
            repository_ids: vec!["faseela".into()],
            package_changes: vec![PackageChange {
                name: "openssl".into(),
                action: "upgrade".into(),
                status: "committed".into(),
                before: Some(PackageState {
                    version: Some("3.4.0".into()),
                    rel: Some(1),
                    arch: "x86_64".into(),
                    reason: "dependency".into(),
                    artifact_sha256: "a".repeat(64),
                }),
                after: Some(PackageState {
                    version: Some("3.5.0".into()),
                    rel: Some(1),
                    arch: "x86_64".into(),
                    reason: "dependency".into(),
                    artifact_sha256: "b".repeat(64),
                }),
                source: "faseela".into(),
                failure: None,
                hook: None,
            }],
            undone_by: None,
            undoes: None,
            groups: Vec::new(),
        }
    }

    #[test]
    fn record_roundtrip_and_validation() {
        let r = sample(15);
        let raw = r.to_toml().unwrap();
        let back = TransactionRecord::from_toml(&raw).unwrap();
        assert_eq!(back.id, 15);
        assert_eq!(back.status().unwrap(), TxStatus::Completed);
        assert_eq!(back.package_changes[0].name, "openssl");
        assert_eq!(back.package_changes[0].before.as_ref().unwrap().version.as_deref(), Some("3.4.0"));

        // unsupported schema is refused
        let bad = raw.replace("schema = 1", "schema = 2");
        assert!(TransactionRecord::from_toml(&bad).is_err());
        // unknown status is refused
        let bad = raw.replace("completed", "almost-done");
        assert!(TransactionRecord::from_toml(&bad).is_err());
        // path-like package names are refused
        let mut evil = sample(2);
        evil.package_changes[0].name = "../escape".into();
        assert!(evil.validate().is_err());
        // invalid artifact digests are refused
        let mut badref = sample(3);
        badref.package_changes[0].after.as_mut().unwrap().artifact_sha256 = "nothex".into();
        assert!(badref.validate().is_err());
    }

    #[test]
    fn ids_are_monotonic_and_persisted() {
        let base = tmpdir("ids");
        let root = base.join("root");
        assert_eq!(allocate_id(&root).unwrap(), 1);
        assert_eq!(allocate_id(&root).unwrap(), 2);
        assert_eq!(allocate_id(&root).unwrap(), 3);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn write_list_and_rewrite_work() {
        let base = tmpdir("store");
        let root = base.join("root");
        let mut r = sample(1);
        write_record(&root, &r).unwrap();
        r = sample(2);
        write_record(&root, &r).unwrap();
        let all = list_records(&root).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].id, 1);

        // rewrite with an annotation keeps the old copy
        let mut annotated = sample(1);
        annotated.undone_by = Some(3);
        rewrite_record(&root, &annotated).unwrap();
        let loaded = load_record(&root, 1).unwrap();
        assert_eq!(loaded.undone_by, Some(3));
        assert!(transactions_dir(&root).join(".old").exists());

        // referenced artifacts across before/after states
        let refs = referenced_artifacts(&root).unwrap();
        assert!(refs.contains(&"a".repeat(64)) && refs.contains(&"b".repeat(64)));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn partial_undo_marks_original_only_after_all_packages_are_restored() {
        let base = tmpdir("partial-undo-mark");
        let root = base.join("root");
        let original = sample(1);
        write_record(&root, &original).unwrap();
        let before = original.package_changes[0].before.clone().unwrap();
        let mut undo = sample(2);
        undo.command = "history-undo".into();
        undo.status = "partial".into();
        undo.undoes = Some(1);
        undo.package_changes = vec![PackageChange {
            name: "openssl".into(),
            action: "restore".into(),
            status: "committed-hook-failed".into(),
            before: original.package_changes[0].after.clone(),
            after: Some(before.clone()),
            source: "history-undo".into(),
            failure: Some("post-install hook failed".into()),
            hook: Some("failed".into()),
        }];
        write_record(&root, &undo).unwrap();
        let current = Entry {
            name: "openssl".into(),
            version: before.version.clone().unwrap(),
            rel: before.rel.unwrap(),
            arch: before.arch.clone(),
            reason: before.reason.clone(),
            run: vec![],
            install_date: 0,
            size: 0,
            artifact_sha256: before.artifact_sha256.clone(),
        };
        db::Db::new(&root).add(&current, &[], &Default::default()).unwrap();
        assert!(undo_restored_original(&root, 1, 2).unwrap());
        mark_undone_by(&root, 1, 2).unwrap();
        assert_eq!(load_record(&root, 1).unwrap().undone_by, Some(2));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn corrupt_record_surfaces_as_list_error() {
        let base = tmpdir("corrupt");
        let root = base.join("root");
        write_record(&root, &sample(1)).unwrap();
        let p = transaction_path(&root, 1);
        std::fs::write(&p, "not toml at all [[[").unwrap();
        assert!(list_records(&root).is_err(), "a truncated record must not be skipped silently");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn state_comparison_uses_artifact_digest() {
        let mut e = crate::db::Entry {
            name: "x".into(),
            version: "1.0".into(),
            rel: 1,
            arch: String::new(),
            reason: "explicit".into(),
            run: vec![],
            install_date: 0,
            size: 0,
            artifact_sha256: "c".repeat(64),
        };
        let mut st = PackageState::from_entry(&e);
        assert!(st.matches_entry(&e));
        // same version, different bytes -> superseded
        e.artifact_sha256 = "d".repeat(64);
        assert!(!st.matches_entry(&e), "identity alone must not win over bytes");
        e.artifact_sha256 = "c".repeat(64);
        // digest-less legacy state falls back to identity
        st.artifact_sha256.clear();
        e.artifact_sha256.clear();
        assert!(st.matches_entry(&e));
    }
}
