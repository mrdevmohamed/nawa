//! The durable migration record store (design §10.3).
//!
//! One versioned TOML record per migration under the selected target
//! root's state directory —
//! `<root>/var/lib/nawa/migrations/<migration-id>.toml` — holding the
//! durable state and the ordered write-ahead step intents/outcomes (§11).
//! The state machine itself is pure domain
//! (`domain/system/migration.rs`); this module owns only the text format,
//! the directory layout and durable publication.
//!
//! Placement: a top-level module, not under `application/features/`,
//! because two separate consumers need it — the lock mechanism checks the
//! mutation gate before every exclusive acquisition (§10.3), and the
//! migration coordinator use case will drive records through the state
//! machine. The lock module must not depend on the application layer
//! (application owns the transaction-apply path, which itself locks), so
//! the shared store sits beside the mechanism like `transaction::journal`:
//! a cohesive I/O subsystem the design's module organization (§4.3)
//! deliberately keeps top-level instead of forcing it under one layer.
//!
//! Publication semantics (§10.3): records are written with the shared
//! atomic temp+fsync+rename publication at mode 0600 (restrictive
//! permissions like the transaction journal), so a crash never leaves a
//! torn or half-updated record.
//!
//! Creation semantics live with the migration use case, not here: the
//! durable `planned` record is written only after the live-host apply
//! preflight and explicit authorization succeed, and planning against an
//! offline root is read-only — it must never create or modify a record
//! under the target root (§10.3). Reading is always allowed.
//!
//! Gate wiring (this slice): the lock mechanism consumes
//! [`active_gates`]/[`migrations_dir`] before every exclusive acquisition.
//! Record creation and state transitions (`save`) have no production
//! caller until the coordinator slice, so `dead_code` stays allowed at
//! module scope with that one outstanding consumer named.


use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::adapters::atomic;
use crate::domain::system::capability::Capability;
use crate::domain::system::migration::{
    check_id, HandlerId, MigrationRecord, MigrationState, StepOutcome, StepRecord, SCHEMA_VERSION,
};

/// `<root>/var/lib/nawa/migrations`
pub(crate) fn migrations_dir(root: &Path) -> PathBuf {
    root.join("var/lib/nawa/migrations")
}

/// Create the record directory (and its state-dir parents) with restrictive
/// 0700 permissions before publication. `atomic::write_atomic` creates missing
/// parents with the process umask, which may be permissive; the migration
/// journal must be root-only like the transaction journal (§10.3, §13).
fn ensure_records_dir(root: &Path) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    let dir = migrations_dir(root);
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true).mode(0o700);
    builder
        .create(&dir)
        .with_context(|| format!("create migration records directory {}", dir.display()))?;
    // A symlinked record directory would move authoritative state outside the
    // selected root; refuse it (no-follow, §13).
    let meta = std::fs::symlink_metadata(&dir)
        .with_context(|| format!("inspect migration records directory {}", dir.display()))?;
    if !meta.file_type().is_dir() {
        bail!(
            "migration records directory {} is not a real directory (symlinks are refused)",
            dir.display()
        );
    }
    Ok(())
}

/// The (validated) record file of `id`. The id is validated through the
/// domain model, so it can never traverse out of the record directory.
fn record_path(root: &Path, id: &str) -> Result<PathBuf> {
    check_id(id).map_err(anyhow::Error::msg)?;
    Ok(migrations_dir(root).join(format!("{id}.toml")))
}

/// Refuse a symlinked record file: a symlink under the state directory could
/// redirect authoritative reads to a file outside the selected root (§13:
/// no-follow handling). Atomic publication already replaces the symlink
/// itself on write; reads must not follow it either.
fn check_no_follow(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => bail!(
            "migration record {} is a symlink — refusing to follow it (§13)",
            path.display()
        ),
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("inspect {}", path.display())),
    }
}

/// The record as persisted: the text-format twin of the domain model. The
/// domain types stay free of serialization concerns; every read validates
/// fail closed (unknown schema/state/handler/capability is an error
/// naming the file, never silently misread).
#[derive(Debug, Serialize, Deserialize)]
struct RecordDoc {
    schema: u32,
    id: String,
    definition_version: u32,
    from: CapabilityDoc,
    to: CapabilityDoc,
    handler: String,
    state: String,
    #[serde(default)]
    steps: Vec<StepDoc>,
}

#[derive(Debug, Serialize, Deserialize)]
struct CapabilityDoc {
    domain: String,
    identity: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct StepDoc {
    id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    intent: Option<String>,
    /// "succeeded" | "failed" (absent = interrupted: intent without
    /// outcome, reconciled on restart, §10.3)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    outcome: Option<String>,
    /// required when `outcome` is `"failed"`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    failure: Option<String>,
}

impl RecordDoc {
    fn from_record(record: &MigrationRecord) -> RecordDoc {
        RecordDoc {
            schema: record.schema,
            id: record.id.clone(),
            definition_version: record.definition_version,
            from: CapabilityDoc::from(&record.from),
            to: CapabilityDoc::from(&record.to),
            handler: record.handler.as_str().to_string(),
            state: record.state.as_str().to_string(),
            steps: record
                .steps
                .iter()
                .map(|step| StepDoc {
                    id: step.id.clone(),
                    intent: step.intent.clone(),
                    outcome: step.outcome.as_ref().map(|outcome| match outcome {
                        StepOutcome::Succeeded => "succeeded".to_string(),
                        StepOutcome::Failed { .. } => "failed".to_string(),
                    }),
                    failure: step.outcome.as_ref().and_then(|outcome| match outcome {
                        StepOutcome::Failed { reason } => Some(reason.clone()),
                        StepOutcome::Succeeded => None,
                    }),
                })
                .collect(),
        }
    }

    /// Validate the parsed document. This never names the file: the
    /// single `.with_context` at the `load` boundary names it exactly
    /// once for every failure below (§10.3 fail closed).
    fn into_record(self) -> Result<MigrationRecord> {
        if self.schema != SCHEMA_VERSION {
            bail!(
                "unsupported migration record schema {} (this nawa understands schema {SCHEMA_VERSION})",
                self.schema
            );
        }
        if self.id.is_empty() {
            bail!("migration record has no id");
        }
        if self.definition_version == 0 {
            bail!("migration record has definition_version 0");
        }
        let state = MigrationState::parse(&self.state).map_err(anyhow::Error::msg)?;
        let handler = HandlerId::parse(&self.handler).map_err(anyhow::Error::msg)?;
        let steps = self
            .steps
            .into_iter()
            .map(|step| {
                if step.id.is_empty() {
                    bail!("migration record step has no id");
                }
                let outcome = match (step.outcome.as_deref(), step.failure.as_deref()) {
                    (None, None) => None,
                    (None, Some(_)) => {
                        bail!("migration record step {} has a failure without an outcome", step.id)
                    }
                    (Some("succeeded"), None) => Some(StepOutcome::Succeeded),
                    (Some("succeeded"), Some(_)) => {
                        bail!("migration record step {} succeeded but records a failure", step.id)
                    }
                    (Some("failed"), Some(reason)) => {
                        Some(StepOutcome::Failed { reason: reason.to_string() })
                    }
                    (Some("failed"), None) => {
                        bail!("migration record step {} failed without a reason", step.id)
                    }
                    (Some(other), _) => bail!(
                        "migration record step {} has unknown outcome {other:?} (expected \"succeeded\" or \"failed\")",
                        step.id
                    ),
                };
                Ok(StepRecord {
                    id: step.id,
                    intent: step.intent,
                    outcome,
                })
            })
            .collect::<Result<Vec<StepRecord>>>()?;
        Ok(MigrationRecord {
            schema: self.schema,
            id: self.id,
            definition_version: self.definition_version,
            from: self.from.into_capability()?,
            to: self.to.into_capability()?,
            handler,
            state,
            steps,
        })
    }
}

impl CapabilityDoc {
    fn from(cap: &Capability) -> CapabilityDoc {
        CapabilityDoc {
            domain: cap.domain.clone(),
            identity: cap.identity.clone(),
        }
    }

    /// Parse the capability key. The failure message names the key, not
    /// the file — the `load` boundary adds the file context (once).
    fn into_capability(self) -> Result<Capability> {
        Capability::parse(&format!("{}={}", self.domain, self.identity)).map_err(anyhow::Error::msg)
    }
}

/// Load the record of `id`, if it exists. Absence is not an error and not
/// a decision (§5.1 semantics, like the profile loader); a record that
/// fails validation is an error naming the file — fail closed. `load` is
/// the single site that names the file: every failure below it (parse,
/// schema, state, handler, capability, steps) is wrapped here exactly
/// once, so the rendered chain is `<file context>: <cause>`.
pub(crate) fn load(root: &Path, id: &str) -> Result<Option<MigrationRecord>> {
    let path = record_path(root, id)?;
    check_no_follow(&path)?;
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(e).with_context(|| format!("system migration record {}", path.display()))
        }
    };
    let doc: RecordDoc = toml::from_str(&raw)
        .context("record is not valid TOML")
        .with_context(|| format!("system migration record {}", path.display()))?;
    // every validation failure names the exact file (§10.3 fail closed):
    // the doc header promises "an error naming the file" for unknown
    // schema/state/handler/capability and invalid steps alike
    doc.into_record()
        .with_context(|| format!("system migration record {}", path.display()))
        .map(Some)
}

/// Durably publish `record` (§10.3: atomic publication, restrictive
/// permissions). The caller owns the lifecycle rules — when a record may
/// be created and which transitions are legal — and must apply them
/// through the domain state machine before calling this.
pub(crate) fn save(root: &Path, record: &MigrationRecord) -> Result<()> {
    let path = record_path(root, &record.id)?;
    check_no_follow(&path)?;
    ensure_records_dir(root)?;
    let doc = RecordDoc::from_record(record);
    atomic::write_atomic(&path, toml::to_string_pretty(&doc)?.as_bytes(), 0o600)
        .with_context(|| format!("publish migration record {}", path.display()))
}

/// Remove the record of `id`. A missing record is already removed
/// (`Ok(())`). The id is validated like every other store entry, so the
/// removal cannot escape the record directory. Production callers: the
/// coordinator's abandon path (a planned record whose gate never became
/// durable is removed, never left lying about) and the recovery slice's
/// discard decision.
pub(crate) fn remove(root: &Path, id: &str) -> Result<()> {
    let path = record_path(root, id)?;
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("remove migration record {}", path.display())),
    }
}

/// The ids of every record file under `root`, sorted. Directory absence
/// is not an error (`Ok(vec![])`): no migration record exists yet. Only
/// `*.toml` names count; a caller that needs the contents loads each id
/// through [`load`], which reports exactly what is wrong with a record
/// that fails validation.
pub(crate) fn record_ids(root: &Path) -> Result<Vec<String>> {
    let dir = migrations_dir(root);
    match std::fs::symlink_metadata(&dir) {
        Ok(meta) if meta.file_type().is_symlink() => bail!(
            "migration records directory {} is a symlink — refusing to follow it (§13)",
            dir.display()
        ),
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(e).with_context(|| format!("inspect migration records {}", dir.display()))
        }
    }
    let rd = match std::fs::read_dir(&dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(e).with_context(|| format!("read migration records {}", dir.display()))
        }
    };
    let mut ids: Vec<String> = rd
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .filter_map(|name| name.strip_suffix(".toml").map(|id| id.to_string()))
        .collect();
    ids.sort();
    Ok(ids)
}

/// One active migration gate found under `root` (§10.3): while any record
/// is in a gate-keeping state, every Nawa-owned package-mutating command
/// must stop before changing the package set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ActiveGate {
    /// the migration holding the gate
    pub(crate) id: String,
    /// its durable state (the gate-keeping one)
    pub(crate) state: MigrationState,
}

/// Every gate-keeping record under `root`, sorted by id. Empty when no
/// migration is active. Any record that fails validation fails the whole
/// scan: the mutation gate must refuse fail-closed rather than risk
/// changing the package set while a pending migration cannot be ruled out.
pub(crate) fn active_gates(root: &Path) -> Result<Vec<ActiveGate>> {
    let mut gates = Vec::new();
    for id in record_ids(root)? {
        if let Some(record) = load(root, &id)? {
            if record.state.keeps_gate() {
                gates.push(ActiveGate {
                    id: record.id,
                    state: record.state,
                });
            }
        }
    }
    Ok(gates)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::system::capability::Capability;
    use crate::domain::system::migration::{MigrationDefinition, StepOutcome, StepRecord};
    use std::os::unix::fs::PermissionsExt;

    fn cap(key: &str) -> Capability {
        Capability::parse(key).unwrap()
    }

    fn definition() -> MigrationDefinition {
        MigrationDefinition::new(
            "faseela-runit-to-systemd-v1",
            1,
            cap("init-system=runit"),
            cap("init-system=systemd"),
            HandlerId::FaseelaInitRunitToSystemd,
            true,
        )
        .unwrap()
    }

    fn record_with_steps() -> MigrationRecord {
        let mut record = MigrationRecord::new(&definition());
        record.state = MigrationState::AwaitingReboot;
        record.steps = vec![
            StepRecord {
                id: "stage-artifacts".to_string(),
                intent: Some("fetch and verify package artifacts".to_string()),
                outcome: Some(StepOutcome::Succeeded),
            },
            StepRecord {
                id: "install-packages".to_string(),
                intent: Some("install the systemd packages".to_string()),
                outcome: Some(StepOutcome::Failed {
                    reason: "package payload conflict".to_string(),
                }),
            },
            // intent without outcome: interrupted step (§10.3)
            StepRecord {
                id: "prepare-boot".to_string(),
                intent: Some("prepare boot/initramfs integration".to_string()),
                outcome: None,
            },
        ];
        record
    }

    #[test]
    fn a_record_roundtrips_through_the_text_format() {
        let root = crate::testutil::tmpdir("migrate-store-roundtrip");
        let record = record_with_steps();
        save(&root, &record).unwrap();
        let loaded = load(&root, &record.id).unwrap().unwrap();
        assert_eq!(loaded.schema, SCHEMA_VERSION);
        assert_eq!(loaded.id, "faseela-runit-to-systemd-v1");
        assert_eq!(loaded.definition_version, 1);
        assert_eq!(loaded.from.to_key(), "init-system=runit");
        assert_eq!(loaded.to.to_key(), "init-system=systemd");
        assert_eq!(loaded.handler, HandlerId::FaseelaInitRunitToSystemd);
        assert_eq!(loaded.state, MigrationState::AwaitingReboot);
        assert_eq!(loaded.steps.len(), 3);
        assert_eq!(loaded.steps[0].outcome, Some(StepOutcome::Succeeded));
        assert_eq!(
            loaded.steps[1].outcome,
            Some(StepOutcome::Failed {
                reason: "package payload conflict".to_string()
            })
        );
        assert_eq!(
            loaded.steps[2].outcome, None,
            "interrupted step stays interrupted"
        );
    }

    #[test]
    fn an_absent_record_is_none_not_an_error() {
        let root = crate::testutil::tmpdir("migrate-store-absent");
        assert_eq!(load(&root, "faseela-runit-to-systemd-v1").unwrap(), None);
        // no migrations directory at all: no gates either
        assert!(active_gates(&root).unwrap().is_empty());
    }

    #[test]
    fn the_record_directory_is_created_restrictively() {
        let root = crate::testutil::tmpdir("migrate-store-dir-perms");
        save(&root, &MigrationRecord::new(&definition())).unwrap();
        let dir = migrations_dir(&root);
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700, "the record directory is root-only (0700)");
    }

    #[test]
    fn symlinked_records_and_directories_are_refused_without_following() {
        let root = crate::testutil::tmpdir("migrate-store-symlink");
        let outside = crate::testutil::tmpdir("migrate-store-symlink-outside");
        let target = outside.join("outside.toml");
        let record = MigrationRecord::new(&definition());
        save(&outside, &record).unwrap();
        std::fs::rename(
            outside.join("var/lib/nawa/migrations/faseela-runit-to-systemd-v1.toml"),
            &target,
        )
        .unwrap();

        let dir = migrations_dir(&root);
        std::fs::create_dir_all(&dir).unwrap();
        std::os::unix::fs::symlink(&target, dir.join("faseela-runit-to-systemd-v1.toml")).unwrap();
        let error = load(&root, "faseela-runit-to-systemd-v1").unwrap_err();
        assert!(format!("{error:#}").contains("symlink"), "{error:#}");
        assert!(
            active_gates(&root).is_err(),
            "the gate scan fails closed instead of following the link"
        );
        assert!(
            save(&root, &record).is_err(),
            "publication never writes through a symlink"
        );

        // a symlinked record directory is refused too
        let other = crate::testutil::tmpdir("migrate-store-real-dir");
        let real = migrations_dir(&other);
        std::fs::create_dir_all(&real).unwrap();
        let swapped = crate::testutil::tmpdir("migrate-store-swapped");
        let swapped_dir = migrations_dir(&swapped);
        let _ = std::fs::remove_dir_all(&swapped_dir);
        std::fs::create_dir_all(swapped_dir.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&real, &swapped_dir).unwrap();
        assert!(record_ids(&swapped).is_err(), "no-follow on the directory");
    }

    #[test]
    fn records_are_saved_restrictively_and_replaced_atomically() {
        let root = crate::testutil::tmpdir("migrate-store-perms");
        let mut record = MigrationRecord::new(&definition());
        save(&root, &record).unwrap();
        let path = root.join("var/lib/nawa/migrations/faseela-runit-to-systemd-v1.toml");
        let mode = std::fs::Permissions::from_mode(0o100600);
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode(),
            mode.mode(),
            "the record is 0600 like the transaction journal"
        );
        // a state update replaces the file wholesale
        record.state = MigrationState::Preparing;
        save(&root, &record).unwrap();
        assert_eq!(
            load(&root, &record.id).unwrap().unwrap().state,
            MigrationState::Preparing
        );
    }

    #[test]
    fn corrupt_or_unknown_records_fail_closed_naming_the_file() {
        let root = crate::testutil::tmpdir("migrate-store-corrupt");
        let dir = root.join("var/lib/nawa/migrations");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("faseela-runit-to-systemd-v1.toml");

        std::fs::write(&path, "not [ valid toml").unwrap();
        let e = load(&root, "faseela-runit-to-systemd-v1").unwrap_err();
        assert!(
            format!("{e:#}").contains("faseela-runit-to-systemd-v1.toml"),
            "{e:#}"
        );

        // every validation failure names the exact file (the module
        // header's fail-closed promise) — the schema case asserts both
        // the file and the cause
        std::fs::write(
            &path,
            "schema = 99\nid = \"m\"\ndefinition_version = 1\nfrom = { domain = \"init-system\", identity = \"runit\" }\nto = { domain = \"init-system\", identity = \"systemd\" }\nhandler = \"faseela.init.runit_to_systemd\"\nstate = \"planned\"\n",
        )
        .unwrap();
        let e = load(&root, "faseela-runit-to-systemd-v1").unwrap_err();
        assert!(
            format!("{e:#}").contains("unsupported migration record schema 99"),
            "{e:#}"
        );
        assert!(
            format!("{e:#}").contains("faseela-runit-to-systemd-v1.toml"),
            "the validation failure names the file: {e:#}"
        );

        std::fs::write(
            &path,
            "schema = 1\nid = \"m\"\ndefinition_version = 1\nfrom = { domain = \"init-system\", identity = \"runit\" }\nto = { domain = \"init-system\", identity = \"systemd\" }\nhandler = \"faseela.init.runit_to_systemd\"\nstate = \"prepped\"\n",
        )
        .unwrap();
        let e = load(&root, "faseela-runit-to-systemd-v1").unwrap_err();
        assert!(
            format!("{e:#}").contains("unknown migration state"),
            "{e:#}"
        );

        std::fs::write(
            &path,
            "schema = 1\nid = \"m\"\ndefinition_version = 1\nfrom = { domain = \"init-system\", identity = \"runit\" }\nto = { domain = \"init-system\", identity = \"systemd\" }\nhandler = \"/usr/libexec/evil\"\nstate = \"planned\"\n",
        )
        .unwrap();
        let e = load(&root, "faseela-runit-to-systemd-v1").unwrap_err();
        assert!(format!("{e:#}").contains("allowlisted ID"), "{e:#}");

        // an outcome without its failure reason
        std::fs::write(
            &path,
            "schema = 1\nid = \"m\"\ndefinition_version = 1\nfrom = { domain = \"init-system\", identity = \"runit\" }\nto = { domain = \"init-system\", identity = \"systemd\" }\nhandler = \"faseela.init.runit_to_systemd\"\nstate = \"preparing\"\n\n[[steps]]\nid = \"install-packages\"\noutcome = \"failed\"\n",
        )
        .unwrap();
        let e = load(&root, "faseela-runit-to-systemd-v1").unwrap_err();
        assert!(
            format!("{e:#}").contains("failed without a reason"),
            "{e:#}"
        );
    }

    #[test]
    fn ids_that_traverse_are_refused_before_touching_the_filesystem() {
        let root = crate::testutil::tmpdir("migrate-store-traversal");
        assert!(record_path(&root, "../host").is_err());
        assert!(record_path(&root, "sub/dir").is_err());
        assert!(record_path(&root, "").is_err());
        // nothing was written anywhere
        assert!(!root.join("var").exists());
    }

    #[test]
    fn active_gates_reports_gate_keeping_records_sorted() {
        let root = crate::testutil::tmpdir("migrate-store-gates");

        // a completed migration is not a gate
        let mut done = MigrationRecord::new(&definition());
        done.state = MigrationState::Completed;
        save(&root, &done).unwrap();
        assert!(active_gates(&root).unwrap().is_empty());

        // two other migrations, one gate-keeping one not, sorted by id
        let mut planned = MigrationRecord::new(&definition());
        planned.id = "aaa-planned".to_string();
        save(&root, &planned).unwrap();
        let mut preparing = MigrationRecord::new(&definition());
        preparing.id = "zzz-preparing".to_string();
        preparing.state = MigrationState::Preparing;
        save(&root, &preparing).unwrap();

        let gates = active_gates(&root).unwrap();
        assert_eq!(
            gates,
            vec![ActiveGate {
                id: "zzz-preparing".to_string(),
                state: MigrationState::Preparing,
            }],
            "only gate-keeping states are reported, sorted by id"
        );
    }

    #[test]
    fn a_gate_keeping_state_survives_the_roundtrip() {
        // the exact §10.3 gate-active set, through the store's format
        let root = crate::testutil::tmpdir("migrate-store-gate-states");
        for state in [
            MigrationState::Preparing,
            MigrationState::AwaitingReboot,
            MigrationState::Verifying,
            MigrationState::RollbackRequired,
            MigrationState::NeedsRecovery,
        ] {
            let mut record = MigrationRecord::new(&definition());
            record.state = state;
            save(&root, &record).unwrap();
            assert_eq!(
                load(&root, &record.id).unwrap().unwrap().state,
                state,
                "{state:?} roundtrip"
            );
            let gates = active_gates(&root).unwrap();
            assert_eq!(gates.len(), 1, "{state:?} keeps the gate");
            assert_eq!(gates[0].state, state);
            // clear for the next iteration
            std::fs::remove_file(root.join(format!("var/lib/nawa/migrations/{}.toml", record.id)))
                .unwrap();
        }
    }
}
