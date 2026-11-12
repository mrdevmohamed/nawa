//! Resolve the version `nawa rollback` should restore.
//!
//! Transaction history is the canonical logical source. The legacy
//! `undo/<name>/desc` slot stays as a compatibility mirror, so the resolver
//! deliberately keeps the old slot authoritative wherever history cannot
//! reproduce it exactly. The selection rules are documented in
//! `docs/design/undo-wal-consolidation.md` §6.3; changing this order changes
//! user-visible rollback behavior.

use anyhow::{bail, Context, Result};
use std::path::Path;

use crate::db::{Db, Entry};
use crate::transaction::history::{self, PackageChange, PackageState, TransactionRecord, TxStatus};

/// How much later the legacy slot may claim to be before we treat it as
/// "written by an old binary after the newest history record". The legacy
/// format has no transaction id, so filesystem mtime is the only signal.
const CLOCK_SKEW_SECS: u64 = 60;

/// Where the selected previous version came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PreviousSource {
    /// A committed `PackageChange.before` in transaction history. History
    /// records package changes, not WAL step numbers, so the transaction is
    /// the finest identity available here.
    History { tx: u64 },
    /// The legacy `undo/<name>/desc` compatibility mirror.
    LegacySlot,
}

/// The version `nawa rollback` should restore.
#[derive(Debug, Clone)]
pub(crate) struct PreviousVersion {
    pub state: PackageState,
    /// Kept for diagnostics and resolver tests; the rollback path itself only
    /// consumes the target state.
    #[allow(dead_code)]
    pub source: PreviousSource,
}

/// A before-state candidate from one transaction record.
#[derive(Debug, Clone)]
struct Candidate {
    before: Option<PackageState>,
    tx: u64,
}

impl Candidate {
    /// The installed before-state, if this candidate can be a rollback target.
    fn installed(&self) -> Option<&PackageState> {
        self.before.as_ref().filter(|state| is_installed(state))
    }
}

struct Analysis {
    /// Before-state of the newest committed change for the package.
    committed: Option<Candidate>,
    /// Before-state of the newest attempted change (`failed` included).
    attempted: Option<Candidate>,
    /// Newest transaction timestamp on disk, for old-binary detection.
    newest_timestamp: Option<u64>,
}

fn is_installed(state: &PackageState) -> bool {
    state.version.is_some() && state.rel.is_some()
}

fn is_committed(change: &PackageChange) -> bool {
    matches!(
        change.status.as_str(),
        "committed" | "committed-hook-failed"
    )
}

fn is_attempted(change: &PackageChange) -> bool {
    matches!(
        change.status.as_str(),
        "committed" | "committed-hook-failed" | "failed"
    )
}

fn is_active(status: TxStatus) -> bool {
    matches!(
        status,
        TxStatus::Preparing | TxStatus::Running | TxStatus::NeedsRecovery | TxStatus::Interrupted
    )
}

fn describe_state(name: &str, state: &PackageState) -> String {
    match (&state.version, state.rel) {
        (Some(version), Some(rel)) => format!("{name}-{version}-{rel}"),
        (Some(version), None) => format!("{name}-{version}"),
        _ => format!("{name} (absent)"),
    }
}

/// Resolve the one-slot-back rollback target for `name`.
///
/// Returns `None` when neither the legacy slot nor history records a previous
/// version. Must be called while no other nawa process can mutate the root
/// (the caller holds the exclusive database lock).
pub(crate) fn resolve_previous(root: &Path, name: &str) -> Result<Option<PreviousVersion>> {
    crate::db::check_name(name)?;
    let current = Db::new(root).entry(name)?;
    let desc_path = root.join("var/lib/nawa/undo").join(name).join("desc");
    let legacy = read_legacy(&desc_path)?;
    // A corrupt/unsupported record is surfaced, never skipped: choosing a
    // rollback target from half-valid history would be worse than refusing.
    let records = history::list_records(root)
        .context("cannot read transaction history while resolving the rollback target")?;
    let analysis = analyze(root, name, &records)?;

    let committed = analysis.committed.as_ref();
    match (legacy, committed) {
        (None, None) => Ok(None),
        (None, Some(candidate)) => match candidate.installed() {
            Some(_) => Ok(Some(from_history(candidate))),
            // A fresh install's before-state is absent: no previous slot.
            None => Ok(None),
        },
        // Rule 2: old root or old binary. This also covers rule 7 (a fresh
        // install whose newest committed change has no before-state), because
        // such a change has no installed candidate.
        (Some(legacy), None) => Ok(Some(from_legacy(legacy))),
        (Some(legacy), Some(candidate)) => {
            let Some(history_state) = candidate.installed() else {
                return Ok(Some(from_legacy(legacy)));
            };
            // Rule 6 first: preserve the historical "already at" message even
            // when the two records disagree about bytes. The caller performs
            // the actual current-version check.
            if current
                .as_ref()
                .is_some_and(|installed| same_identity(&legacy, installed))
            {
                return Ok(Some(from_legacy(legacy)));
            }
            // Rules 3 and 4: the slot and the newest committed before-state
            // agree. Use the slot as the target, but keep a non-empty history
            // digest when the legacy record has none (stronger verification).
            if state_agrees_with_entry(history_state, &legacy) {
                return Ok(Some(merge_legacy_with_history(legacy, candidate)));
            }
            // Rule 5: a failed attempt after `save_undo` consumed the slot.
            // The slot now names the current state; the caller's existing
            // "already at" check preserves the old behavior.
            if analysis
                .attempted
                .as_ref()
                .and_then(Candidate::installed)
                .is_some_and(|attempted| state_agrees_with_entry(attempted, &legacy))
            {
                return Ok(Some(from_legacy(legacy)));
            }
            // Rule 8: the legacy slot is clearly newer than every history
            // record, which is what an old binary writing after history looks
            // like. The legacy format has no transaction id, so mtime is the
            // only available signal; require a clear margin.
            if legacy_is_clearly_newer(&desc_path, analysis.newest_timestamp) {
                return Ok(Some(from_legacy(legacy)));
            }
            // Rule 9: fail closed rather than silently choose one of two
            // contradictory targets.
            let history_target = describe_state(name, history_state);
            bail!(
                "cannot choose a rollback target for `{name}`: the undo record says {}, \
                 but transaction {} records {}; inspect `nawa history` before rolling back",
                legacy.version_full(),
                candidate.tx,
                history_target
            );
        }
    }
}

/// Synthesize the entry consumed by the existing restore path. Only the
/// package identity and artifact digest are used there: the installer
/// re-reads `run`, `install_date`, and `size` from the verified archive.
/// Never pass this synthetic entry to `db.add` or package hooks.
pub(crate) fn synthesize_entry(name: &str, state: &PackageState) -> Entry {
    Entry {
        name: name.to_string(),
        version: state.version.clone().unwrap_or_default(),
        rel: state.rel.unwrap_or(0),
        arch: state.arch.clone(),
        reason: state.reason.clone(),
        run: Vec::new(),
        install_date: 0,
        size: 0,
        artifact_sha256: state.artifact_sha256.clone(),
    }
}

fn read_legacy(path: &Path) -> Result<Option<Entry>> {
    if !path.exists() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("read undo record {}", path.display()))?;
    let entry = toml::from_str(&raw).context("corrupt undo record")?;
    Ok(Some(entry))
}

fn from_legacy(entry: Entry) -> PreviousVersion {
    PreviousVersion {
        state: PackageState::from_entry(&entry),
        source: PreviousSource::LegacySlot,
    }
}

fn from_history(candidate: &Candidate) -> PreviousVersion {
    PreviousVersion {
        // `Candidate::installed` guaranteed a before-state.
        state: candidate.before.clone().unwrap_or_default(),
        source: PreviousSource::History { tx: candidate.tx },
    }
}

/// Same package identity. Digests are compared separately because a legacy
/// record may have no digest at all.
fn same_identity(entry: &Entry, other: &Entry) -> bool {
    entry.version == other.version && entry.rel == other.rel
}

fn state_identity_matches(state: &PackageState, entry: &Entry) -> bool {
    state.version.as_deref() == Some(entry.version.as_str()) && state.rel == Some(entry.rel)
}

/// Rules 3/4: same identity, and the digests do not contradict each other.
/// One empty digest is compatible (the other side has stronger information);
/// two different non-empty digests are a disagreement.
fn state_agrees_with_entry(state: &PackageState, entry: &Entry) -> bool {
    if !state_identity_matches(state, entry) {
        return false;
    }
    state.artifact_sha256.is_empty()
        || entry.artifact_sha256.is_empty()
        || state.artifact_sha256 == entry.artifact_sha256
}

/// Keep the legacy target, but borrow the history digest when the slot has
/// none. This preserves legacy-cache fallback only when history also has no
/// digest; otherwise the stronger retained-proof path is used.
fn merge_legacy_with_history(entry: Entry, candidate: &Candidate) -> PreviousVersion {
    let mut state = PackageState::from_entry(&entry);
    if state.artifact_sha256.is_empty() {
        if let Some(before) = &candidate.before {
            state.artifact_sha256 = before.artifact_sha256.clone();
        }
    }
    PreviousVersion {
        state,
        source: PreviousSource::LegacySlot,
    }
}

fn analyze(root: &Path, name: &str, records: &[TransactionRecord]) -> Result<Analysis> {
    refuse_untrustworthy_transactions(root, name, records)?;

    let mut committed = None;
    let mut attempted = None;
    // Newest id first. Undone records are superseded by their compensating
    // transaction, which supplies its own committed before-state.
    for record in records.iter().rev() {
        if record.undone_by.is_some() {
            continue;
        }
        if committed.is_none() {
            if let Some(change) = record
                .package_changes
                .iter()
                .rev()
                .find(|change| change.name == name && is_committed(change))
            {
                committed = Some(Candidate {
                    before: change.before.clone(),
                    tx: record.id,
                });
            }
        }
        if attempted.is_none() {
            if let Some(change) = record
                .package_changes
                .iter()
                .rev()
                .find(|change| change.name == name && is_attempted(change))
            {
                attempted = Some(Candidate {
                    before: change.before.clone(),
                    tx: record.id,
                });
            }
        }
        if committed.is_some() && attempted.is_some() {
            break;
        }
    }

    let newest_timestamp = records
        .iter()
        .map(|record| record.finished_at.unwrap_or(record.started_at))
        .max();

    Ok(Analysis {
        committed,
        attempted,
        newest_timestamp,
    })
}

/// Refuse when a transaction touching this package is still active or
/// unresolved. The on-disk state is not trustworthy enough to pick a target.
fn refuse_untrustworthy_transactions(
    root: &Path,
    name: &str,
    records: &[TransactionRecord],
) -> Result<()> {
    for record in records {
        let status = record.status()?;
        if is_active(status)
            && record
                .package_changes
                .iter()
                .any(|change| change.name == name)
        {
            bail!(
                "transaction {} is {} and touches `{name}` — \
                 inspect `nawa history` before rolling back",
                record.id,
                status.as_str()
            );
        }
    }
    // A journal without a terminal record, or with an active record, still
    // has an unfinished intent. `main` reconciles these before an exclusive
    // command, but resolve_previous must also be safe for direct callers.
    for tx in crate::transaction::journal::journaled_transactions(root)? {
        let status = records
            .iter()
            .find(|record| record.id == tx)
            .map(|record| record.status())
            .transpose()?;
        let active = status.is_none_or(is_active);
        for (intent, result) in crate::transaction::journal::transaction_steps(root, tx)? {
            // An intent without a result is unfinished even when a terminal
            // record exists: journal cleanup or reconciliation was interrupted.
            if intent.package == name && (active || result.is_none()) {
                bail!(
                    "transaction {tx} has an unfinished journal step for `{name}` — \
                     run `nawa history` to reconcile it before rolling back"
                );
            }
        }
    }
    Ok(())
}

fn legacy_is_clearly_newer(desc_path: &Path, newest_history: Option<u64>) -> bool {
    let Some(newest_history) = newest_history else {
        return false;
    };
    let Ok(modified) = std::fs::metadata(desc_path).and_then(|metadata| metadata.modified()) else {
        return false;
    };
    let Ok(modified) = modified.duration_since(std::time::UNIX_EPOCH) else {
        return false;
    };
    modified.as_secs() > newest_history.saturating_add(CLOCK_SKEW_SECS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::tmpdir;
    use crate::transaction::history::{PackageChange, SCHEMA_VERSION};

    fn state(version: &str, digest: &str) -> PackageState {
        PackageState {
            version: Some(version.into()),
            rel: Some(1),
            arch: "x86_64".into(),
            reason: "explicit".into(),
            artifact_sha256: digest.into(),
        }
    }

    fn entry(version: &str, digest: &str) -> Entry {
        synthesize_entry("demo", &state(version, digest))
    }

    fn change(
        status: &str,
        before: Option<PackageState>,
        after: Option<PackageState>,
    ) -> PackageChange {
        PackageChange {
            name: "demo".into(),
            action: "upgrade".into(),
            status: status.into(),
            before,
            after,
            source: "test".into(),
            failure: None,
            hook: None,
        }
    }

    fn record(id: u64, status: &str, changes: Vec<PackageChange>) -> TransactionRecord {
        let now = crate::artifact::now_secs();
        TransactionRecord {
            schema: SCHEMA_VERSION,
            id,
            command: "upgrade".into(),
            status: status.into(),
            started_at: now.saturating_sub(1),
            finished_at: Some(now),
            duration_seconds: Some(1),
            initiated_by: "uid:0".into(),
            requested_packages: vec!["demo".into()],
            repository_ids: Vec::new(),
            package_changes: changes,
            undone_by: None,
            undoes: None,
            groups: Vec::new(),
        }
    }

    fn write_desc(root: &Path, entry: &Entry) {
        let dir = root.join("var/lib/nawa/undo/demo");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("desc"), toml::to_string_pretty(entry).unwrap()).unwrap();
    }

    fn write_history(root: &Path, record: &TransactionRecord) {
        history::write_record(root, record).unwrap();
    }

    fn add_current(root: &Path, entry: &Entry) {
        Db::new(root).add(entry, &[], &Default::default()).unwrap();
    }

    #[test]
    fn no_slot_and_no_history_has_no_target() {
        let base = tmpdir("previous-none");
        assert!(resolve_previous(&base, "demo").unwrap().is_none());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn history_install_without_before_has_no_target() {
        let base = tmpdir("previous-fresh");
        let after = entry("1.0", &"a".repeat(64));
        add_current(&base, &after);
        write_history(
            &base,
            &record(
                1,
                "completed",
                vec![change(
                    "committed",
                    None,
                    Some(state("1.0", &"a".repeat(64))),
                )],
            ),
        );
        assert!(resolve_previous(&base, "demo").unwrap().is_none());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn history_upgrade_returns_the_previous_version() {
        let base = tmpdir("previous-history");
        add_current(&base, &entry("2.0", &"b".repeat(64)));
        write_history(
            &base,
            &record(
                1,
                "completed",
                vec![change(
                    "committed",
                    Some(state("1.0", &"a".repeat(64))),
                    Some(state("2.0", &"b".repeat(64))),
                )],
            ),
        );
        let previous = resolve_previous(&base, "demo").unwrap().unwrap();
        assert_eq!(previous.state.version.as_deref(), Some("1.0"));
        assert_eq!(previous.source, PreviousSource::History { tx: 1 });
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn legacy_slot_only_is_used_on_an_old_root() {
        let base = tmpdir("previous-legacy");
        write_desc(&base, &entry("1.0", &"a".repeat(64)));
        let previous = resolve_previous(&base, "demo").unwrap().unwrap();
        assert_eq!(previous.state.version.as_deref(), Some("1.0"));
        assert_eq!(previous.source, PreviousSource::LegacySlot);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn agreement_prefers_a_non_empty_history_digest() {
        let base = tmpdir("previous-merge");
        add_current(&base, &entry("2.0", &"b".repeat(64)));
        write_desc(&base, &entry("1.0", ""));
        write_history(
            &base,
            &record(
                1,
                "completed",
                vec![change(
                    "committed",
                    Some(state("1.0", &"a".repeat(64))),
                    Some(state("2.0", &"b".repeat(64))),
                )],
            ),
        );
        let previous = resolve_previous(&base, "demo").unwrap().unwrap();
        assert_eq!(previous.state.version.as_deref(), Some("1.0"));
        assert_eq!(previous.state.artifact_sha256, "a".repeat(64));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn failed_attempt_before_save_undo_keeps_the_older_slot() {
        let base = tmpdir("previous-before-save");
        add_current(&base, &entry("2.0", &"b".repeat(64)));
        // The last committed operation upgraded 1.0 -> 2.0, so the slot is 1.0.
        write_desc(&base, &entry("1.0", &"a".repeat(64)));
        write_history(
            &base,
            &record(
                1,
                "completed",
                vec![change(
                    "committed",
                    Some(state("1.0", &"a".repeat(64))),
                    Some(state("2.0", &"b".repeat(64))),
                )],
            ),
        );
        // A later upgrade 2.0 -> 3.0 failed before save_undo.
        write_history(
            &base,
            &record(
                2,
                "failed",
                vec![change(
                    "failed",
                    Some(state("2.0", &"b".repeat(64))),
                    Some(state("3.0", &"c".repeat(64))),
                )],
            ),
        );
        let previous = resolve_previous(&base, "demo").unwrap().unwrap();
        assert_eq!(previous.state.version.as_deref(), Some("1.0"));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn failed_attempt_after_save_undo_uses_the_current_slot() {
        let base = tmpdir("previous-after-save");
        add_current(&base, &entry("2.0", &"b".repeat(64)));
        write_desc(&base, &entry("2.0", &"b".repeat(64)));
        write_history(
            &base,
            &record(
                1,
                "completed",
                vec![change(
                    "committed",
                    Some(state("1.0", &"a".repeat(64))),
                    Some(state("2.0", &"b".repeat(64))),
                )],
            ),
        );
        write_history(
            &base,
            &record(
                2,
                "failed",
                vec![change(
                    "failed",
                    Some(state("2.0", &"b".repeat(64))),
                    Some(state("3.0", &"c".repeat(64))),
                )],
            ),
        );
        let previous = resolve_previous(&base, "demo").unwrap().unwrap();
        assert_eq!(previous.state.version.as_deref(), Some("2.0"));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn slot_equal_to_current_is_still_returned_for_the_already_at_check() {
        let base = tmpdir("previous-already-at");
        add_current(&base, &entry("1.0", &"a".repeat(64)));
        write_desc(&base, &entry("1.0", &"a".repeat(64)));
        write_history(
            &base,
            &record(
                1,
                "completed",
                vec![change(
                    "committed",
                    Some(state("1.0", &"a".repeat(64))),
                    Some(state("2.0", &"b".repeat(64))),
                )],
            ),
        );
        let previous = resolve_previous(&base, "demo").unwrap().unwrap();
        assert_eq!(previous.state.version.as_deref(), Some("1.0"));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn fresh_install_keeps_a_stale_slot() {
        let base = tmpdir("previous-stale-slot");
        add_current(&base, &entry("3.0", &"c".repeat(64)));
        write_desc(&base, &entry("1.0", &"a".repeat(64)));
        write_history(
            &base,
            &record(
                1,
                "completed",
                vec![change(
                    "committed",
                    None,
                    Some(state("3.0", &"c".repeat(64))),
                )],
            ),
        );
        let previous = resolve_previous(&base, "demo").unwrap().unwrap();
        assert_eq!(previous.state.version.as_deref(), Some("1.0"));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn clearly_newer_legacy_slot_wins_over_history() {
        let base = tmpdir("previous-old-binary");
        add_current(&base, &entry("3.0", &"c".repeat(64)));
        // The slot and history disagree; only the old-binary mtime signal
        // explains why the slot is newer.
        write_desc(&base, &entry("1.0", &"a".repeat(64)));
        let mut old = record(
            1,
            "completed",
            vec![change(
                "committed",
                Some(state("2.0", &"b".repeat(64))),
                Some(state("3.0", &"c".repeat(64))),
            )],
        );
        let past = crate::artifact::now_secs().saturating_sub(3600);
        old.started_at = past;
        old.finished_at = Some(past);
        write_history(&base, &old);
        let previous = resolve_previous(&base, "demo").unwrap().unwrap();
        assert_eq!(previous.state.version.as_deref(), Some("1.0"));
        assert_eq!(previous.source, PreviousSource::LegacySlot);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn contradictory_digests_fail_closed() {
        let base = tmpdir("previous-contradiction");
        add_current(&base, &entry("3.0", &"c".repeat(64)));
        write_desc(&base, &entry("1.0", &"a".repeat(64)));
        write_history(
            &base,
            &record(
                1,
                "completed",
                vec![change(
                    "committed",
                    Some(state("1.0", &"d".repeat(64))),
                    Some(state("3.0", &"c".repeat(64))),
                )],
            ),
        );
        let error = resolve_previous(&base, "demo").unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains("cannot choose a rollback target"),
            "{message}"
        );
        assert!(message.contains("nawa history"), "{message}");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn corrupt_legacy_slot_is_an_error() {
        let base = tmpdir("previous-corrupt-desc");
        let dir = base.join("var/lib/nawa/undo/demo");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("desc"), "not toml [[[").unwrap();
        let error = resolve_previous(&base, "demo").unwrap_err();
        assert!(format!("{error:#}").contains("corrupt undo record"));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn corrupt_history_blocks_resolution_too() {
        let base = tmpdir("previous-corrupt-history");
        write_desc(&base, &entry("1.0", &"a".repeat(64)));
        let dir = base.join("var/lib/nawa/history/transactions");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("0000000001.toml"), "not toml [[[").unwrap();
        let error = resolve_previous(&base, "demo").unwrap_err();
        assert!(
            format!("{error:#}").contains("cannot read transaction history"),
            "{error:#}"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn active_transaction_touching_the_package_refuses() {
        for status in ["preparing", "running", "needs-recovery", "interrupted"] {
            let base = tmpdir(&format!("previous-active-{status}"));
            write_desc(&base, &entry("1.0", &"a".repeat(64)));
            write_history(
                &base,
                &record(
                    1,
                    status,
                    vec![change(
                        "committed",
                        Some(state("1.0", &"a".repeat(64))),
                        None,
                    )],
                ),
            );
            let error = resolve_previous(&base, "demo").unwrap_err();
            assert!(
                format!("{error:#}").contains("inspect `nawa history`"),
                "{status}: {error:#}"
            );
            let _ = std::fs::remove_dir_all(&base);
        }
    }

    #[test]
    fn pending_journal_step_refuses() {
        use crate::transaction::journal::{write_intent, write_tx_state, StepIntent, TxState};

        let base = tmpdir("previous-pending-journal");
        write_desc(&base, &entry("1.0", &"a".repeat(64)));
        write_tx_state(
            &base,
            &TxState {
                schema: 1,
                tx: 7,
                command: "install".into(),
                started_at: crate::artifact::now_secs(),
            },
        )
        .unwrap();
        write_intent(
            &base,
            &StepIntent {
                schema: SCHEMA_VERSION,
                tx: 7,
                step: 1,
                package: "demo".into(),
                action: "upgrade".into(),
                precondition: Some(state("1.0", &"a".repeat(64))),
                target: Some(state("2.0", &"b".repeat(64))),
                artifact_sha256: "b".repeat(64),
            },
        )
        .unwrap();
        let error = resolve_previous(&base, "demo").unwrap_err();
        assert!(
            format!("{error:#}").contains("unfinished journal step"),
            "{error:#}"
        );

        // A terminal record does not make a leftover pending intent safe:
        // journal cleanup or reconciliation itself was interrupted.
        write_history(&base, &record(7, "completed", Vec::new()));
        let error = resolve_previous(&base, "demo").unwrap_err();
        assert!(
            format!("{error:#}").contains("unfinished journal step"),
            "{error:#}"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn skipped_changes_are_ignored_and_failed_changes_are_attempted() {
        let base = tmpdir("previous-skipped");
        add_current(&base, &entry("2.0", &"b".repeat(64)));
        write_desc(&base, &entry("2.0", &"b".repeat(64)));
        write_history(
            &base,
            &record(
                1,
                "completed",
                vec![change(
                    "committed",
                    Some(state("1.0", &"a".repeat(64))),
                    Some(state("2.0", &"b".repeat(64))),
                )],
            ),
        );
        // A newer transaction skipped this package and must not become H_a.
        write_history(
            &base,
            &record(
                2,
                "failed",
                vec![change("skipped", Some(state("9.0", &"9".repeat(64))), None)],
            ),
        );
        // A still newer failed attempt is H_a; it matches the current slot.
        write_history(
            &base,
            &record(
                3,
                "failed",
                vec![change(
                    "failed",
                    Some(state("2.0", &"b".repeat(64))),
                    Some(state("4.0", &"d".repeat(64))),
                )],
            ),
        );
        let previous = resolve_previous(&base, "demo").unwrap().unwrap();
        assert_eq!(previous.state.version.as_deref(), Some("2.0"));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn compensating_transaction_supplies_the_newest_before_state() {
        let base = tmpdir("previous-undone");
        add_current(&base, &entry("1.0", &"a".repeat(64)));
        let mut original = record(
            1,
            "completed",
            vec![change(
                "committed",
                Some(state("1.0", &"a".repeat(64))),
                Some(state("2.0", &"b".repeat(64))),
            )],
        );
        original.undone_by = Some(2);
        write_history(&base, &original);
        let mut undo_change = change(
            "committed",
            Some(state("2.0", &"b".repeat(64))),
            Some(state("1.0", &"a".repeat(64))),
        );
        undo_change.action = "restore".into();
        let mut undo = record(2, "completed", vec![undo_change]);
        undo.command = "history-undo".into();
        undo.undoes = Some(1);
        write_history(&base, &undo);
        let previous = resolve_previous(&base, "demo").unwrap().unwrap();
        assert_eq!(previous.state.version.as_deref(), Some("2.0"));
        assert_eq!(previous.source, PreviousSource::History { tx: 2 });
        let _ = std::fs::remove_dir_all(&base);
    }
}
