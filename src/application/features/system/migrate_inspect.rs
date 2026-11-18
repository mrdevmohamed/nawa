//! Read-only migration-record inspection use cases (design §12.2): what
//! `nawa system migrate list/show/status` report. The first production
//! consumers of the durable record store (`migration_record`, top level).
//!
//! Strictly read-only per §10.3/§12.2: no record is created, modified or
//! transitioned, no handler runs, and nothing is written under the
//! selected root — the store's `save` has no caller here. Reads work on
//! any `--root`, including offline roots, because only files under
//! `<root>/var/lib/nawa/migrations` are touched.
//!
//! Honesty policy over corrupt records (§10.3: readers stay honest, and
//! unlike the gate's best-effort scan these commands exist to *report*):
//! an inspection that cannot read a record says so with the record's
//! actual validation cause instead of skipping it silently.
//!
//! * `list` returns every readable record plus every corrupt one; the
//!   caller decides how to present and exit.
//! * `show` fails loudly on an unknown id (naming the record directory)
//!   and on a corrupt record (naming the file and the validation failure)
//!   — it must render a specific record or nothing at all.
//! * `status` reports the mutation-gate answer, never fails: a record
//!   directory that cannot even be read is itself the answer, because the
//!   mutation gate refuses fail-closed in exactly that situation
//!   (`lock::check_migration_gate`).

// The migration coordinator (the next Phase 5 slice) will share this
// module's view of the store; until then `save` remains store-internal
// dead code, not an inspection concern.
use std::path::Path;

use anyhow::anyhow;

use crate::adapters::migration_record::{self, ActiveGate};
use crate::domain::system::migration::MigrationRecord;

/// One record under `root` that could not be read. `cause` is the full
/// error chain, which names the record file and the exact validation
/// failure (§12.2: report corrupt records with their actual errors).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UnreadableRecord {
    /// the migration id the record file name claims
    pub(crate) id: String,
    /// the full error chain naming the file and the failure
    pub(crate) cause: String,
}

/// The result of reading every record under `root`: every readable record
/// plus every one that could not be read. Deterministic order (by id).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Listing {
    pub(crate) records: Vec<MigrationRecord>,
    pub(crate) unreadable: Vec<UnreadableRecord>,
}

impl Listing {
    /// True when the store holds no record at all — neither a readable
    /// nor an unreadable one. The caller explains this explicitly (§12.2:
    /// an empty store is a stated result, not silence).
    pub(crate) fn is_empty(&self) -> bool {
        self.records.is_empty() && self.unreadable.is_empty()
    }
}

/// Read every migration record under `root`, sorted by id. A directory
/// that does not exist is an empty listing (no migration record was ever
/// written under this root); any other directory-read failure is an error.
pub(crate) fn list(root: &Path) -> anyhow::Result<Listing> {
    let mut records = Vec::new();
    let mut unreadable = Vec::new();
    for id in migration_record::record_ids(root)? {
        match migration_record::load(root, &id) {
            Ok(Some(record)) => records.push(record),
            // The directory listing saw the file, the read did not find
            // it: under the shared inspection lock nothing may remove
            // records, so this is reported instead of skipped.
            Ok(None) => unreadable.push(UnreadableRecord {
                id: id.clone(),
                cause: format!(
                    "system migration record {} is gone: it was removed while being read",
                    migration_record::migrations_dir(root)
                        .join(format!("{id}.toml"))
                        .display()
                ),
            }),
            Err(error) => unreadable.push(UnreadableRecord {
                id,
                cause: format!("{error:#}"),
            }),
        }
    }
    Ok(Listing {
        records,
        unreadable,
    })
}

/// Load one migration record for display. Unknown ids and unreadable
/// records are loud errors (§12.2): exit 1, naming the record directory
/// or the exact file and validation failure.
pub(crate) fn show(root: &Path, id: &str) -> anyhow::Result<MigrationRecord> {
    match migration_record::load(root, id) {
        Ok(Some(record)) => Ok(record),
        Ok(None) => Err(anyhow!(
            "no migration record named {id:?} exists under {} — `nawa system migrate list` shows what does",
            migration_record::migrations_dir(root).display()
        )),
        Err(error) => Err(error),
    }
}

/// The mutation-gate answer under `root` (§10.3): whether package-mutating
/// commands are currently blocked, and by what. Informational — this
/// never fails, because an unreadable record directory *is* the answer
/// (the gate refuses fail-closed when it cannot verify the record state).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Status {
    /// where the records live under this root
    pub(crate) dir: std::path::PathBuf,
    /// whether package-mutating commands are currently blocked
    pub(crate) blocked: bool,
    /// every migration holding the gate, sorted by id
    pub(crate) gates: Vec<ActiveGate>,
    /// records that could not be read — they block mutations fail-closed
    /// even without a gate-active state among them
    pub(crate) unreadable: Vec<UnreadableRecord>,
    /// set when the record directory itself could not be read: the gate
    /// state cannot be verified, so mutations are refused fail-closed
    pub(crate) scan_error: Option<String>,
}

impl Status {
    /// Whether the block is the ordinary gate (some record keeps it) or
    /// the fail-closed refusal (record state cannot be verified).
    pub(crate) fn blocked_by_gate(&self) -> bool {
        !self.gates.is_empty()
    }
}

pub(crate) fn status(root: &Path) -> Status {
    let dir = migration_record::migrations_dir(root);
    let ids = match migration_record::record_ids(root) {
        Ok(ids) => ids,
        Err(error) => {
            return Status {
                dir,
                blocked: true,
                gates: Vec::new(),
                unreadable: Vec::new(),
                scan_error: Some(format!("{error:#}")),
            }
        }
    };
    let mut gates = Vec::new();
    let mut unreadable = Vec::new();
    for id in ids {
        match migration_record::load(root, &id) {
            Ok(Some(record)) => {
                if record.state.keeps_gate() {
                    gates.push(ActiveGate {
                        id: record.id,
                        state: record.state,
                    });
                }
            }
            Ok(None) => unreadable.push(UnreadableRecord {
                id: id.clone(),
                cause: format!(
                    "system migration record {} is gone: it was removed while being read",
                    dir.join(format!("{id}.toml")).display()
                ),
            }),
            Err(error) => unreadable.push(UnreadableRecord {
                id,
                cause: format!("{error:#}"),
            }),
        }
    }
    Status {
        dir,
        blocked: !gates.is_empty() || !unreadable.is_empty(),
        gates,
        unreadable,
        scan_error: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::system::capability::Capability;
    use crate::domain::system::migration::{
        HandlerId, MigrationDefinition, MigrationState, StepRecord,
    };
    use crate::testutil::tmpdir;

    fn cap(key: &str) -> Capability {
        Capability::parse(key).unwrap()
    }

    fn definition(id: &str) -> MigrationDefinition {
        MigrationDefinition::new(
            id,
            1,
            cap("init-system=runit"),
            cap("init-system=systemd"),
            HandlerId::FaseelaInitRunitToSystemd,
            true,
        )
        .unwrap()
    }

    fn record(id: &str, state: MigrationState) -> MigrationRecord {
        let mut record = MigrationRecord::new(&definition(id));
        record.state = state;
        record
    }

    fn interrupted_record() -> MigrationRecord {
        let mut record = record("faseela-runit-to-systemd-v1", MigrationState::Preparing);
        record.steps = vec![StepRecord {
            id: "install-packages".to_string(),
            intent: Some("install the systemd packages".to_string()),
            outcome: None,
        }];
        record
    }

    #[test]
    fn list_on_a_root_without_a_record_directory_is_empty() {
        let root = tmpdir("migrate-inspect-missing");
        let listing = list(&root).unwrap();
        assert!(listing.is_empty());
        assert!(listing.records.is_empty());
        assert!(listing.unreadable.is_empty());
    }

    #[test]
    fn list_reads_every_record_sorted_by_id() {
        let root = tmpdir("migrate-inspect-list");
        migration_record::save(&root, &record("zzz-migration", MigrationState::Completed)).unwrap();
        migration_record::save(&root, &record("aaa-migration", MigrationState::Planned)).unwrap();
        let listing = list(&root).unwrap();
        assert!(listing.unreadable.is_empty());
        let ids: Vec<&str> = listing.records.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(
            ids,
            ["aaa-migration", "zzz-migration"],
            "deterministic order"
        );
    }

    #[test]
    fn list_reports_a_corrupt_record_with_its_cause_and_keeps_the_rest() {
        let root = tmpdir("migrate-inspect-corrupt");
        std::fs::create_dir_all(migration_record::migrations_dir(&root)).unwrap();
        std::fs::write(
            migration_record::migrations_dir(&root).join("broken-migration.toml"),
            "not [ valid toml",
        )
        .unwrap();
        migration_record::save(&root, &record("good-migration", MigrationState::Planned)).unwrap();

        let listing = list(&root).unwrap();
        assert_eq!(listing.records.len(), 1);
        assert_eq!(listing.records[0].id, "good-migration");
        assert_eq!(listing.unreadable.len(), 1);
        let broken = &listing.unreadable[0];
        assert_eq!(broken.id, "broken-migration");
        assert!(
            broken.cause.contains("broken-migration.toml"),
            "the cause names the exact file: {}",
            broken.cause
        );
        assert!(
            broken.cause.contains("not valid TOML"),
            "the cause is the actual validation failure: {}",
            broken.cause
        );
        assert!(!listing.is_empty());
    }

    #[test]
    fn show_returns_the_record_and_names_the_directory_for_unknown_ids() {
        let root = tmpdir("migrate-inspect-show");
        migration_record::save(&root, &interrupted_record()).unwrap();

        let shown = show(&root, "faseela-runit-to-systemd-v1").unwrap();
        assert_eq!(shown.state, MigrationState::Preparing);
        assert_eq!(shown.steps.len(), 1);
        assert_eq!(
            shown.steps[0].outcome, None,
            "interrupted stays interrupted"
        );

        let error = show(&root, "no-such-migration").unwrap_err();
        let msg = format!("{error:#}");
        assert!(msg.contains("no-such-migration"), "{msg}");
        assert!(
            msg.contains("var/lib/nawa/migrations"),
            "the error names the record directory: {msg}"
        );
    }

    #[test]
    fn show_propagates_a_corrupt_record_with_its_exact_failure() {
        let root = tmpdir("migrate-inspect-show-corrupt");
        let dir = migration_record::migrations_dir(&root);
        std::fs::create_dir_all(&dir).unwrap();
        // every required field present except the state: the unknown
        // state name is the exact validation failure this record carries
        std::fs::write(
            dir.join("faseela-runit-to-systemd-v1.toml"),
            "schema = 1\nid = \"m\"\ndefinition_version = 1\n\
             from = { domain = \"init-system\", identity = \"runit\" }\n\
             to = { domain = \"init-system\", identity = \"systemd\" }\n\
             handler = \"faseela.init.runit_to_systemd\"\nstate = \"prepped\"\n",
        )
        .unwrap();
        let error = show(&root, "faseela-runit-to-systemd-v1").unwrap_err();
        let msg = format!("{error:#}");
        assert!(msg.contains("faseela-runit-to-systemd-v1.toml"), "{msg}");
        assert!(msg.contains("unknown migration state"), "{msg}");
    }

    #[test]
    fn status_reports_the_gate_and_its_records() {
        let root = tmpdir("migrate-inspect-status-gate");
        migration_record::save(&root, &record("aaa-planned", MigrationState::Planned)).unwrap();
        migration_record::save(
            &root,
            &record("zzz-awaiting", MigrationState::AwaitingReboot),
        )
        .unwrap();

        let report = status(&root);
        assert!(report.blocked);
        assert!(report.blocked_by_gate());
        assert!(report.unreadable.is_empty());
        assert_eq!(
            report.gates,
            vec![ActiveGate {
                id: "zzz-awaiting".to_string(),
                state: MigrationState::AwaitingReboot,
            }],
            "only gate-keeping states are reported, sorted by id"
        );
    }

    #[test]
    fn status_reports_no_gate_when_nothing_keeps_it() {
        let root = tmpdir("migrate-inspect-status-open");
        migration_record::save(&root, &record("aaa-planned", MigrationState::Planned)).unwrap();
        let report = status(&root);
        assert!(!report.blocked);
        assert!(!report.blocked_by_gate());
        assert!(report.scan_error.is_none());

        let fresh = status(&tmpdir("migrate-inspect-status-fresh"));
        assert!(!fresh.blocked);
    }

    #[test]
    fn status_reports_unreadable_records_as_a_fail_closed_block() {
        // a corrupt record makes the mutation gate refuse fail-closed, so
        // status must report the block even without a gate-active state
        let root = tmpdir("migrate-inspect-status-corrupt");
        let dir = migration_record::migrations_dir(&root);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("broken-migration.toml"), "not [ valid toml").unwrap();

        let report = status(&root);
        assert!(report.blocked);
        assert!(!report.blocked_by_gate(), "no gate-keeping state was read");
        assert_eq!(report.unreadable.len(), 1);
        assert!(report.unreadable[0].cause.contains("broken-migration.toml"));
        assert!(report.scan_error.is_none());
    }

    #[test]
    fn an_unreadable_record_directory_is_itself_the_status_answer() {
        // the gate refuses fail-closed when the record state cannot be
        // read; status reports exactly that instead of failing
        let root = tmpdir("migrate-inspect-status-dir-error");
        let dir = migration_record::migrations_dir(&root);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("blocked-migration.toml");
        std::fs::create_dir(&path).unwrap(); // a directory in the file's place

        let report = status(&root);
        assert!(report.blocked);
        assert!(!report.blocked_by_gate());
        assert_eq!(report.unreadable.len(), 1);
        assert!(report.unreadable[0]
            .cause
            .contains("blocked-migration.toml"));
    }
}
