//! End-to-end application seam: authenticated pin -> staging -> coordinator ->
//! the real package WAL/installer, exclusively under a disposable root.

use super::{migrate_apply, migrate_coordinator, observation};
use crate::adapters::pathutil::create_managed_dirs;
use crate::domain::planning::install::Why;
use crate::domain::system::plan::{
    ArtifactPin, PackageActionKind, PackageActionSource, SystemPlanPin,
};
use std::path::{Path, PathBuf};

struct Fixture {
    root: PathBuf,
    repository: PathBuf,
    pin: SystemPlanPin,
}

impl Fixture {
    fn new(tag: &str) -> Self {
        let root = crate::testutil::tmpdir(tag);
        let repository = root.join("repository");
        std::fs::create_dir_all(&repository).unwrap();
        std::fs::create_dir_all(root.join("etc/runit/runsvdir/default/nested/empty")).unwrap();
        std::fs::write(
            root.join("etc/runit/runsvdir/default/nested/run"),
            "old service tree",
        )
        .unwrap();
        let mut entries = Vec::new();
        let mut artifacts = Vec::new();
        for (name, deps, provides) in [
            ("libfixture", "", ""),
            ("systemd", "libfixture", "init-system=systemd"),
        ] {
            let stage = root.join(format!("fixture-{name}"));
            std::fs::create_dir_all(stage.join("usr/share/fixture")).unwrap();
            std::fs::write(stage.join("usr/share/fixture").join(name), name).unwrap();
            let file = format!("{name}-1-1.nawa.tar.zst");
            let archive = repository.join(&file);
            crate::adapters::archive::create(&stage, &format!("pkgname = {name}\npkgver = 1\npkgrel = 1\narch = x86_64\nrundeps = {deps}\nprovides = {provides}\nrelations_schema = 1\nconflicts = \nreplaces = \n"), &archive).unwrap();
            let sha256 = crate::adapters::checksum::sha256_of_file(&archive).unwrap();
            let size = std::fs::metadata(&archive).unwrap().len();
            let url = format!("https://invalid.example.test/approved/{file}");
            // Use the existing verified download cache, avoiding all network
            // access. Missing/corrupt cached artifacts still go through the
            // production fetch verifier and may never become replacements.
            let cache = crate::fetch::remote_cache_dir(&root, &url);
            create_managed_dirs(&cache).unwrap();
            std::fs::copy(&archive, cache.join(&file)).unwrap();
            entries.push(crate::repo::RepoEntry {
                name: name.into(),
                version: "1".into(),
                rel: 1,
                arch: "x86_64".into(),
                file: file.clone(),
                sha256: sha256.clone(),
                size,
                installed_size: 0,
                download_url: Some(url.clone()),
                rundeps: Some(if deps.is_empty() {
                    vec![]
                } else {
                    vec![deps.into()]
                }),
                provides: Some(if provides.is_empty() {
                    vec![]
                } else {
                    vec![provides.into()]
                }),
                relations: Some(crate::domain::package::relations::PackageRelations {
                    schema: 1,
                    conflicts: vec![],
                    replaces: vec![],
                }),
            });
            artifacts.push(ArtifactPin {
                kind: PackageActionKind::Install,
                source: PackageActionSource::AuthenticatedRepository,
                name: name.into(),
                current: None,
                target: "1-1".into(),
                archive: file,
                sha256,
                size,
                download_url_sha256: Some(hash(url.as_bytes())),
                reason: "reviewed fixture action".into(),
                install_reason: if name == "systemd" {
                    Why::Explicit
                } else {
                    Why::Dependency
                },
            });
        }
        let index = crate::repo::Index {
            generated: Some(crate::adapters::artifact::now_secs()),
            expires: Some(crate::adapters::artifact::now_secs() + 3600),
            repository: Some(crate::repo::RepositoryMeta::v5_per_package_url()),
            recipes: None,
            groups: None,
            packages: entries,
        };
        let bytes = toml::to_string(&index).unwrap();
        let index_path = repository.join("index.toml");
        std::fs::write(&index_path, &bytes).unwrap();
        let (secret, public) = crate::adapters::sign::keygen(&root.join("keys")).unwrap();
        crate::adapters::sign::sign(&index_path, &secret).unwrap();
        crate::adapters::sign::trust(&public, &root).unwrap();
        let snapshot = observation::observe(&root, Path::new("/proc"));
        let mut pin = crate::domain::system::plan::fixture_pin();
        pin.source_fingerprint = hash(format!("local:{}", repository.display()).as_bytes());
        pin.index_sha256 = hash(bytes.as_bytes());
        pin.signer_sha256 = hash(&std::fs::read(&public).unwrap());
        pin.observed_sha256 = hash(format!("{:?}", snapshot.observed).as_bytes());
        pin.installed_identities = snapshot.installed_identities;
        pin.artifacts = artifacts;
        pin.identity = pin.computed_identity();
        Self {
            root,
            repository,
            pin,
        }
    }

    fn ctx(&self) -> crate::application::context::Ctx {
        crate::application::context::Ctx::from_config(&crate::adapters::config::Config {
            root: self.root.clone(),
            recipes: self.root.join("no-recipes"),
            builddir: self.root.join("build"),
            profiles: self.root.join("profiles"),
            jobs: None,
            repo: None,
            install_mode: crate::adapters::config::InstallMode::Auto,
            downloads: 4,
            file: None,
            system_file: None,
            sources: Default::default(),
        })
    }

    fn approval(&self) -> migrate_apply::MigrationApproval {
        let d = migrate_apply::builtin_definition();
        migrate_apply::MigrationApproval::explicit(
            &d.id,
            &d.from,
            &d.to,
            true,
            true,
            self.pin.clone(),
        )
        .unwrap()
    }

    fn stage(&self) -> anyhow::Result<migrate_coordinator::ApprovedPlan> {
        migrate_coordinator::ApprovedPlan::stage(
            &self.ctx(),
            Path::new("/proc"),
            self.repository.to_str().unwrap(),
            self.approval(),
        )
    }
}

fn hash(bytes: &[u8]) -> String {
    crate::adapters::checksum::sha256_bytes(bytes)
}

fn ui() -> crate::application::ports::user_interaction::UserInteraction {
    crate::application::ports::user_interaction::UserInteraction {
        stderr_line: Box::new(|_| {}),
        info: Box::new(|_| {}),
        warn: Box::new(|_| {}),
        show_plan: Box::new(|_| {}),
        choose: Box::new(|_, _, _| anyhow::bail!("no generic confirmation")),
        confirm: Box::new(|_| anyhow::bail!("no generic confirmation")),
    }
}

#[test]
fn pinned_execution_retains_proof_order_reasons_snapshot_and_the_gate() {
    let fixture = Fixture::new("pinned-execution");
    let plan = fixture.stage().unwrap();
    let result = migrate_coordinator::prepare(
        &fixture.ctx(),
        migrate_coordinator::PreparationRequest {
            definition: &migrate_apply::builtin_definition(),
            plan,
            ui: &ui(),
        },
    )
    .unwrap();
    // Packages execute, but the unresolved distribution boot contract must
    // still prevent a fabricated awaiting-reboot/completed result.
    assert!(matches!(
        result,
        migrate_coordinator::PreparationOutcome::Ambiguous(_)
    ));
    let record = crate::adapters::migration_record::load(&fixture.root, &fixture.pin.migration_id)
        .unwrap()
        .unwrap();
    assert_eq!(record.schema, 2);
    assert_eq!(record.approval, Some(fixture.pin.clone()));
    assert!(record.snapshot.is_some());
    assert_eq!(
        record
            .steps
            .iter()
            .map(|s| s.id.as_str())
            .collect::<Vec<_>>(),
        [
            "capture-boot-snapshot",
            "install-libfixture",
            "install-systemd"
        ]
    );
    assert!(record
        .steps
        .iter()
        .all(|s| s.outcome == Some(crate::domain::system::migration::StepOutcome::Succeeded)));
    let db = crate::adapters::db::Db::new(&fixture.root);
    assert_eq!(
        db.entry("libfixture").unwrap().unwrap().reason,
        "dependency"
    );
    assert_eq!(db.entry("systemd").unwrap().unwrap().reason, "explicit");
    for pin in &fixture.pin.artifacts {
        assert_eq!(
            db.entry(&pin.name).unwrap().unwrap().artifact_sha256,
            pin.sha256
        );
        let provenance = crate::adapters::artifact::load_provenance(&fixture.root, &pin.sha256)
            .unwrap()
            .unwrap();
        assert_eq!(provenance.authentication, "signed-index");
        assert_eq!(
            provenance.repository.unwrap().index_sha256,
            fixture.pin.index_sha256
        );
    }
    let txs = crate::transaction::history::list_records(&fixture.root).unwrap();
    let tx = crate::transaction::history::load_record(&fixture.root, txs[0].id).unwrap();
    assert_eq!(
        tx.package_changes
            .iter()
            .map(|c| c.name.as_str())
            .collect::<Vec<_>>(),
        ["libfixture", "systemd"]
    );
    assert!(
        !crate::adapters::migration_record::active_gates(&fixture.root)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn inventory_changes_after_staging_are_refused_before_durable_intent() {
    let fixture = Fixture::new("pinned-stale-inventory");
    let plan = fixture.stage().unwrap();
    let directory = fixture.root.join("var/lib/nawa/local/changed");
    create_managed_dirs(&directory).unwrap();
    std::fs::write(
        directory.join("desc"),
        "name='changed'\nversion='1'\nrel=1\n",
    )
    .unwrap();
    let error = migrate_coordinator::prepare(
        &fixture.ctx(),
        migrate_coordinator::PreparationRequest {
            definition: &migrate_apply::builtin_definition(),
            plan,
            ui: &ui(),
        },
    )
    .unwrap_err();
    assert!(
        format!("{error:#}").contains("installed package inventory changed before mutation"),
        "{error:#}"
    );
    assert!(
        crate::adapters::migration_record::load(&fixture.root, &fixture.pin.migration_id)
            .unwrap()
            .is_none()
    );
    assert!(crate::adapters::db::Db::new(&fixture.root)
        .entry("systemd")
        .unwrap()
        .is_none());
}

#[test]
fn reordered_or_substituted_approval_cannot_reach_staging() {
    let mut fixture = Fixture::new("pinned-order");
    fixture.pin.artifacts.swap(0, 1);
    let d = migrate_apply::builtin_definition();
    assert!(migrate_apply::MigrationApproval::explicit(
        &d.id,
        &d.from,
        &d.to,
        true,
        true,
        fixture.pin.clone()
    )
    .is_err());
    fixture.pin.artifacts.swap(0, 1);
    fixture.pin.artifacts[0].size += 1;
    assert!(fixture.pin.validate().is_err());
}

#[test]
fn tampered_private_staging_cannot_open_the_mutation_window() {
    let fixture = Fixture::new("pinned-stage-tamper");
    let plan = fixture.stage().unwrap();
    let path = std::fs::read_dir(fixture.root.join(".nawa"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    std::fs::write(path, "tampered").unwrap();
    assert!(migrate_coordinator::prepare(
        &fixture.ctx(),
        migrate_coordinator::PreparationRequest {
            definition: &migrate_apply::builtin_definition(),
            plan,
            ui: &ui(),
        }
    )
    .is_err());
    assert!(
        crate::adapters::migration_record::load(&fixture.root, &fixture.pin.migration_id)
            .unwrap()
            .is_none()
    );
    assert!(std::fs::read_dir(fixture.root.join(".nawa"))
        .unwrap()
        .next()
        .is_none());
}

#[test]
fn same_version_inventory_digest_changes_also_invalidate_approval() {
    let mut fixture = Fixture::new("pinned-inventory-identity");
    let directory = fixture.root.join("var/lib/nawa/local/runit");
    create_managed_dirs(&directory).unwrap();
    std::fs::write(
        directory.join("desc"),
        format!(
            "name='runit'\nversion='1'\nrel=1\nartifact_sha256='{}'\n",
            "a".repeat(64)
        ),
    )
    .unwrap();
    let observed = observation::observe(&fixture.root, Path::new("/proc"));
    fixture.pin.installed_packages = vec![("runit".into(), "1-1".into())];
    fixture.pin.installed_identities = observed.installed_identities;
    fixture.pin.observed_sha256 = hash(format!("{:?}", observed.observed).as_bytes());
    fixture.pin.identity = fixture.pin.computed_identity();
    let plan = fixture.stage().unwrap();
    std::fs::write(
        directory.join("desc"),
        format!(
            "name='runit'\nversion='1'\nrel=1\nartifact_sha256='{}'\n",
            "b".repeat(64)
        ),
    )
    .unwrap();
    let error = migrate_coordinator::prepare(
        &fixture.ctx(),
        migrate_coordinator::PreparationRequest {
            definition: &migrate_apply::builtin_definition(),
            plan,
            ui: &ui(),
        },
    )
    .unwrap_err();
    assert!(
        format!("{error:#}").contains("installed package inventory changed before mutation"),
        "{error:#}"
    );
}

#[test]
fn index_and_signing_key_changes_cannot_replace_the_approved_source() {
    let fixture = Fixture::new("pinned-changed-index");
    let path = fixture.repository.join("index.toml");
    let original = std::fs::read(&path).unwrap();
    let mut changed = original.clone();
    changed.push(b'\n');
    std::fs::write(&path, changed).unwrap();
    crate::adapters::sign::sign(&path, &fixture.root.join("keys/nawa.sec")).unwrap();
    assert!(fixture.stage().is_err());
    std::fs::write(&path, original).unwrap();
    let (secret, public) = crate::adapters::sign::keygen(&fixture.root.join("other-key")).unwrap();
    crate::adapters::sign::trust(&public, &fixture.root).unwrap();
    crate::adapters::sign::sign(&path, &secret).unwrap();
    assert!(fixture.stage().is_err());
}

#[test]
fn trust_store_changes_after_staging_are_rechecked_under_the_coordinator_lock() {
    let fixture = Fixture::new("pinned-trust-recheck");
    let plan = fixture.stage().unwrap();
    std::fs::remove_dir_all(crate::adapters::sign::trust_dir(&fixture.root)).unwrap();
    assert!(migrate_coordinator::prepare(
        &fixture.ctx(),
        migrate_coordinator::PreparationRequest {
            definition: &migrate_apply::builtin_definition(),
            plan,
            ui: &ui(),
        },
    )
    .is_err());
    assert!(
        crate::adapters::migration_record::load(&fixture.root, &fixture.pin.migration_id)
            .unwrap()
            .is_none()
    );
    assert!(crate::adapters::db::Db::new(&fixture.root)
        .entry("systemd")
        .unwrap()
        .is_none());
}
