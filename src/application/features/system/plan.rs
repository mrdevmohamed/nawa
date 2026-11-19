//! One immutable, read-only SystemPlan assembled from a single system snapshot.
//!
//! This application use case is the composition boundary between observation,
//! provider policy, authenticated repository metadata and the existing domain
//! resolver. It does not fetch packages, open a transaction, or perform a
//! migration. Exact signed-index and archive identities are retained for a
//! later approval/execution integration; no resolver is repeated here.

use std::path::Path;

use anyhow::{bail, Context, Result};

use crate::application::features::system::observation::{self, PackageInventory, SystemSnapshot};
use crate::fetch::RepoRef;
use crate::upgrade::{
    ArchiveAuth, OpenRepo, PackageActionKind, PackageActionSource, SystemTransitionPreview,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RepositoryKind {
    Local,
    Remote,
}

/// Repository evidence captured while constructing the plan. The source is
/// represented by a fingerprint so credentials or private local paths are
/// never echoed by the renderer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RepositoryAssessment {
    NotConfigured,
    NotInspected {
        reason: String,
    },
    InvalidConfiguration {
        source_fingerprint: String,
        reason: String,
    },
    Unavailable {
        kind: RepositoryKind,
        source_fingerprint: String,
        reason: String,
    },
    Opened {
        kind: RepositoryKind,
        source_fingerprint: String,
        index_schema: Option<u32>,
        index_sha256: String,
        trusted_signer_sha256: Option<String>,
        authenticated_by_trusted_key: bool,
        archive_auth: ArchiveAuth,
        package_count: usize,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TransitionAssessment {
    NotEvaluated { reason: String },
    NoChange,
    Preview(SystemTransitionPreview),
}

/// The single result returned by `nawa system plan`. Its identity covers
/// the observed snapshot, installed inventory, repository proof and every
/// selected artifact digest. The value is read-only after construction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SystemPlan {
    pub(crate) snapshot: SystemSnapshot,
    pub(crate) repository: RepositoryAssessment,
    pub(crate) transition: TransitionAssessment,
    pub(crate) identity: String,
    pub(crate) packages:
        Result<crate::domain::system::package_assessment::PackageAssessment, String>,
}

/// Exact identities copied from the reviewed SystemPlan into migration approval.
/// The ordered artifact vector is authoritative; execution must not re-solve.
pub(crate) use crate::domain::system::plan::{ArtifactPin, SystemPlanPin};

impl SystemPlan {
    /// Only a fully resolved transition backed by a trusted signed v5 index
    /// can be turned into an explicit migration approval.
    pub(crate) fn approval_pin(&self, expected_migration_id: &str) -> Result<SystemPlanPin> {
        let pin = self.pin_evidence(expected_migration_id)?;
        pin.validate().map_err(anyhow::Error::msg)?;
        Ok(pin)
    }

    fn pin_evidence(&self, expected_migration_id: &str) -> Result<SystemPlanPin> {
        let (source_fingerprint, index_sha256, signer_sha256) = match &self.repository {
            RepositoryAssessment::Opened {
                source_fingerprint,
                index_schema: Some(5),
                index_sha256,
                trusted_signer_sha256: Some(signer),
                authenticated_by_trusted_key: true,
                archive_auth: ArchiveAuth::SignedIndexSha256,
                ..
            } => (
                source_fingerprint.clone(),
                index_sha256.clone(),
                signer.clone(),
            ),
            _ => bail!("migration approval requires a trusted signed schema-v5 repository index with explicit package relations"),
        };

        let (actions, version, from, to, provider_package) = match &self.transition {
            TransitionAssessment::Preview(SystemTransitionPreview::Candidate {
                id,
                package_actions,
                version,
                from,
                to,
                provider_package,
                ..
            }) if id == expected_migration_id => (
                package_actions,
                *version,
                from.clone(),
                to.clone(),
                provider_package.clone(),
            ),
            TransitionAssessment::Preview(SystemTransitionPreview::Candidate { id, .. }) => {
                bail!("the reviewed plan selects migration {id}, not {expected_migration_id}")
            }
            TransitionAssessment::Preview(SystemTransitionPreview::Unresolved {
                reason, ..
            }) => {
                bail!("an unresolved system plan cannot be approved: {reason}")
            }
            TransitionAssessment::NotEvaluated { reason } => {
                bail!("system transition was not evaluated: {reason}")
            }
            TransitionAssessment::NoChange => {
                bail!("the reviewed SystemPlan contains no init-system transition to approve")
            }
        };

        let installed_packages = installed_packages(&self.snapshot)?;
        let artifacts = actions
            .iter()
            .map(|action| ArtifactPin {
                kind: action.kind,
                source: action.source,
                name: action.name.clone(),
                current: action.current.clone(),
                target: action.target.clone(),
                archive: action.archive.clone(),
                sha256: action.sha256.clone(),
                size: action.size,
                download_url_sha256: action
                    .download_url
                    .as_deref()
                    .map(|url| fingerprint(url.as_bytes())),
                reason: action.reason.clone(),
                install_reason: action.install_reason,
            })
            .collect();

        Ok(SystemPlanPin {
            identity: self.identity.clone(),
            migration_id: expected_migration_id.to_string(),
            definition_version: version,
            from,
            to,
            provider_package,
            source_fingerprint,
            index_sha256,
            signer_sha256,
            observed_sha256: fingerprint(format!("{:?}", self.snapshot.observed).as_bytes()),
            installed_packages,
            installed_identities: self.snapshot.installed_identities.clone(),
            artifacts,
        })
    }
}

pub(crate) fn build(root: &Path, proc: &Path, repository: Option<&str>) -> SystemPlan {
    build_with_recipes(root, proc, repository, &root.join("var/lib/nawa/recipes"))
}

pub(crate) fn build_with_recipes(
    root: &Path,
    proc: &Path,
    repository: Option<&str>,
    recipes: &Path,
) -> SystemPlan {
    let snapshot = observation::observe(root, proc);
    let mut packages =
        Err("package assessment requires an available inventory and repository".into());

    let (repository, transition) = match &snapshot.packages {
        PackageInventory::Unavailable { reason } => (
            RepositoryAssessment::NotInspected {
                reason: format!(
                    "repository candidates were not evaluated because the installed package inventory is unavailable: {reason}"
                ),
            },
            TransitionAssessment::NotEvaluated {
                reason: format!("installed package inventory is unavailable: {reason}"),
            },
        ),
        PackageInventory::Available(installed) => match repository {
            None => (
                RepositoryAssessment::NotConfigured,
                TransitionAssessment::NotEvaluated {
                    reason: "no repository is configured; provider candidates cannot be verified".to_string(),
                },
            ),
            Some(configured) => match RepoRef::parse(configured) {
                Err(error) => (
                    RepositoryAssessment::InvalidConfiguration {
                        source_fingerprint: fingerprint(configured.as_bytes()),
                        reason: format!("{error:#}"),
                    },
                    TransitionAssessment::NotEvaluated {
                        reason: format!("repository configuration is invalid: {error:#}"),
                    },
                ),
                Ok(repo_ref) => {
                    let (kind, source_fingerprint) = repository_source(&repo_ref);
                    { match crate::upgrade::open_repo_cached(root, &repo_ref, false, false) {
                        Err(error) => (
                            RepositoryAssessment::Unavailable {
                                kind,
                                source_fingerprint,
                                reason: format!("{error:#}"),
                            },
                            TransitionAssessment::NotEvaluated {
                                reason: format!(
                                    "authenticated repository metadata could not be loaded or verified: {error:#}"
                                ),
                            },
                        ),
                        Ok(open) => {
                            packages = crate::upgrade::assess_package_upgrades(
                                &open.index,
                                &snapshot.package_states,
                                recipes,
                            )
                            .map_err(|error| format!("{error:#}"));
                            let assessment = assess_repository(
                                kind,
                                source_fingerprint,
                                &open,
                            );
                            let transition = match crate::upgrade::plan_system_transition(
                                &snapshot.observed,
                                &open,
                                installed,
                            ) {
                                Some(preview) => TransitionAssessment::Preview(preview),
                                None => TransitionAssessment::NoChange,
                            };
                            (assessment, transition)
                        }
                    } }
                }
            },
        },
    };

    let identity = plan_identity(&snapshot, &repository, &transition, &packages);
    let mut plan = SystemPlan {
        snapshot,
        repository,
        transition,
        identity,
        packages,
    };
    if let TransitionAssessment::Preview(SystemTransitionPreview::Candidate { id, .. }) =
        &plan.transition
    {
        if let Ok(pin) = plan.pin_evidence(id) {
            plan.identity = pin.computed_identity();
        }
    }
    plan
}

/// Verify an approved plan without running the dependency solver again.
/// Current observations, inventory, repository proof and every ordered archive
/// identity must still match the explicit approval.
pub(crate) fn verify_approval_pin(
    root: &Path,
    proc: &Path,
    repository: &str,
    pin: &SystemPlanPin,
) -> Result<OpenRepo> {
    pin.validate().map_err(anyhow::Error::msg)?;

    let snapshot = observation::observe(root, proc);
    let current_observed = fingerprint(format!("{:?}", snapshot.observed).as_bytes());
    if current_observed != pin.observed_sha256 {
        bail!("observed system facts changed after approval; create and review a new SystemPlan");
    }
    if installed_packages(&snapshot)? != pin.installed_packages {
        bail!("installed package inventory changed after approval; create and review a new SystemPlan");
    }
    if snapshot.installed_identities != pin.installed_identities {
        bail!("installed package identities changed after approval; create and review a new SystemPlan");
    }

    let repo_ref = RepoRef::parse(repository).context("parse approved repository source")?;
    let (_, source_fingerprint) = repository_source(&repo_ref);
    if source_fingerprint != pin.source_fingerprint {
        bail!("configured repository source changed after approval");
    }

    let open = crate::upgrade::open_repo_cached(root, &repo_ref, false, false)
        .context("re-open and authenticate approved repository")?;
    if fingerprint(&open.index_bytes) != pin.index_sha256 {
        bail!(
            "authenticated repository index changed after approval; refusing stale migration plan"
        );
    }
    let signer = open.signer_public.as_deref().map(fingerprint);
    if signer.as_deref() != Some(pin.signer_sha256.as_str()) {
        bail!("trusted repository signing key changed after approval");
    }
    if open.index.repository.as_ref().map(|meta| meta.schema) != Some(5)
        || open.archive_auth != ArchiveAuth::SignedIndexSha256
    {
        bail!("approved repository no longer satisfies the signed schema-v5 migration policy");
    }

    let mut seen_packages = std::collections::BTreeSet::new();
    for artifact in &pin.artifacts {
        if !seen_packages.insert(artifact.name.as_str()) {
            bail!(
                "approved SystemPlan contains duplicate package action {}",
                artifact.name
            );
        }
        if artifact.source != PackageActionSource::AuthenticatedRepository {
            bail!(
                "package {} has an unsupported source in the approved plan",
                artifact.name
            );
        }
        let installed_version = pin
            .installed_packages
            .iter()
            .find(|(name, _)| name == &artifact.name)
            .map(|(_, version)| version.as_str());
        match artifact.kind {
            PackageActionKind::Install
                if artifact.current.is_some() || installed_version.is_some() =>
            {
                bail!(
                    "install action for {} no longer matches the approved installed state",
                    artifact.name
                );
            }
            PackageActionKind::Upgrade | PackageActionKind::Keep
                if artifact.current.as_deref() != installed_version =>
            {
                bail!(
                    "current version for {} no longer matches the approved action",
                    artifact.name
                );
            }
            _ => {}
        }
        let entry = open.index.find(&artifact.name).with_context(|| {
            format!(
                "approved package {} is no longer present in the authenticated index",
                artifact.name
            )
        })?;
        let version = crate::domain::planning::install::version_rel(&entry.version, entry.rel);
        if version != artifact.target
            || entry.file != artifact.archive
            || entry.sha256 != artifact.sha256
            || entry.size != artifact.size
            || entry
                .download_url
                .as_deref()
                .map(|url| fingerprint(url.as_bytes()))
                != artifact.download_url_sha256
        {
            bail!(
                "authenticated artifact for {} changed after approval; expected {} with sha256 {}",
                artifact.name,
                artifact.archive,
                artifact.sha256
            );
        }
    }
    Ok(open)
}

/// Run under the coordinator's lock immediately before creating durable intent.
/// No repository access, networking or solver work occurs in this check.
pub(crate) fn verify_local_pin(
    root: &Path,
    proc: &Path,
    pin: &SystemPlanPin,
    repository: &OpenRepo,
) -> Result<()> {
    let snapshot = observation::observe(root, proc);
    if fingerprint(format!("{:?}", snapshot.observed).as_bytes()) != pin.observed_sha256
        || installed_packages(&snapshot)? != pin.installed_packages
        || snapshot.installed_identities != pin.installed_identities
    {
        bail!(
            "approved system observations or installed package inventory changed before mutation"
        );
    }
    #[cfg(not(test))]
    {
        if !matches!(
            snapshot.observed.context,
            crate::domain::system::observation::Context::LiveHost { .. }
        ) || !matches!(
            snapshot.observed.runtime,
            crate::domain::system::observation::RuntimeInit::Observed {
                kind: crate::domain::system::identity::InitKind::Runit,
                confidence: crate::domain::system::observation::Confidence::Proven,
                ..
            }
        ) || !matches!(snapshot.observed.desired, crate::application::features::system::detect::DesiredInit::Declared { ref identity, .. } if identity == "systemd")
        {
            bail!("the fresh live-host, source-init or required target profile evidence changed before mutation");
        }
    }
    if repository.index_bytes.is_empty() {
        bail!("approved repository proof has no signed index bytes");
    }
    if fingerprint(&repository.index_bytes) != pin.index_sha256
        || repository
            .signer_public
            .as_deref()
            .map(fingerprint)
            .as_deref()
            != Some(pin.signer_sha256.as_str())
    {
        bail!("approved signed-index or signer evidence changed before mutation");
    }
    let signature = repository
        .index_signature
        .as_deref()
        .context("approved signed-index signature sidecar is unavailable")?;
    crate::adapters::sign::verify_trusted_index_proof(
        &repository.index_bytes,
        signature,
        &pin.signer_sha256,
        root,
    )?;
    Ok(())
}

fn installed_packages(snapshot: &SystemSnapshot) -> Result<Vec<(String, String)>> {
    let packages = match &snapshot.packages {
        PackageInventory::Available(packages) => packages,
        PackageInventory::Unavailable { reason } => {
            bail!("installed package inventory is unavailable: {reason}");
        }
    };
    let mut installed: Vec<_> = packages
        .iter()
        .map(|(name, version)| (name.clone(), version.clone()))
        .collect();
    installed.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(installed)
}

fn repository_source(repo_ref: &RepoRef) -> (RepositoryKind, String) {
    let (kind, material) = match repo_ref {
        RepoRef::Local(path) => (RepositoryKind::Local, format!("local:{}", path.display())),
        RepoRef::Remote(url) => (RepositoryKind::Remote, format!("remote:{url}")),
    };
    (kind, fingerprint(material.as_bytes()))
}

fn assess_repository(
    kind: RepositoryKind,
    source_fingerprint: String,
    open: &OpenRepo,
) -> RepositoryAssessment {
    let trusted_signer_sha256 = open.signer_public.as_deref().map(fingerprint);
    RepositoryAssessment::Opened {
        kind,
        source_fingerprint,
        index_schema: open.index.repository.as_ref().map(|meta| meta.schema),
        index_sha256: fingerprint(&open.index_bytes),
        authenticated_by_trusted_key: trusted_signer_sha256.is_some(),
        trusted_signer_sha256,
        archive_auth: open.archive_auth,
        package_count: open.index.packages.len(),
    }
}

fn fingerprint(bytes: &[u8]) -> String {
    crate::adapters::checksum::sha256_bytes(bytes)
}

fn plan_identity(
    snapshot: &SystemSnapshot,
    repository: &RepositoryAssessment,
    transition: &TransitionAssessment,
    packages: &Result<crate::domain::system::package_assessment::PackageAssessment, String>,
) -> String {
    let mut material = String::from("nawa-system-plan-v1");
    append_part(&mut material, &format!("{:?}", snapshot.observed));
    match &snapshot.packages {
        PackageInventory::Available(packages) => {
            append_part(&mut material, "inventory:available");
            let mut sorted: Vec<_> = packages.iter().collect();
            sorted.sort_by(|left, right| left.0.cmp(right.0));
            for (name, version) in sorted {
                append_part(&mut material, name);
                append_part(&mut material, version);
            }
        }
        PackageInventory::Unavailable { reason } => {
            append_part(&mut material, "inventory:unavailable");
            append_part(&mut material, reason);
        }
    }
    append_part(&mut material, &format!("{repository:?}"));
    append_part(&mut material, &format!("{transition:?}"));
    append_part(&mut material, &format!("{packages:?}"));
    format!("sp-{}", fingerprint(material.as_bytes()))
}

fn append_part(material: &mut String, value: &str) {
    material.push_str(&value.len().to_string());
    material.push(':');
    material.push_str(value);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn root(tag: &str) -> std::path::PathBuf {
        crate::testutil::tmpdir(tag)
    }

    #[test]
    fn approval_pin_preserves_ordered_artifacts_and_exact_digests() {
        use crate::domain::system::identity::InitKind;
        use crate::domain::system::observation::{
            Confidence, Context, DesiredInit, InstalledInit, RuntimeInit,
        };
        use crate::domain::system::profile::{ProviderPolicy, SystemProfile};
        use std::collections::BTreeMap;

        let snapshot = SystemSnapshot {
            installed_identities: vec![crate::domain::system::plan::InstalledIdentity {
                name: "runit".into(),
                version: "2.1-1".into(),
                sha256: String::new(),
                reason: String::new(),
            }],
            observed: crate::domain::system::observation::ObservedSystem {
                root: std::path::PathBuf::from("/"),
                context: Context::LiveHost {
                    evidence: Vec::new(),
                },
                runtime: RuntimeInit::Observed {
                    kind: InitKind::Runit,
                    confidence: Confidence::Proven,
                    evidence: Vec::new(),
                },
                installed: InstalledInit::default(),
                desired: DesiredInit::Declared {
                    identity: "systemd".to_string(),
                    source: "/usr/share/nawa/system-profile.toml".to_string(),
                },
                policy_sources: BTreeMap::new(),
                effective_profile: Some(SystemProfile {
                    schema: 2,
                    distribution: Some("faseela".to_string()),
                    profile: Some("desktop".to_string()),
                    architecture: Some(std::env::consts::ARCH.to_string()),
                    providers: BTreeMap::from([("init-system".to_string(), "systemd".to_string())]),
                    policies: BTreeMap::from([(
                        "init-system".to_string(),
                        ProviderPolicy {
                            required: vec!["systemd".to_string()],
                            preferred: Vec::new(),
                            forbidden: Vec::new(),
                        },
                    )]),
                }),
            },
            packages: PackageInventory::Available(std::collections::HashMap::from([(
                "runit".to_string(),
                "2.1-1".to_string(),
            )])),
            package_states: std::collections::BTreeMap::new(),
        };
        let action = |name: &str, digest: &str| crate::upgrade::system_transition::PackageAction {
            kind: PackageActionKind::Install,
            source: PackageActionSource::AuthenticatedRepository,
            name: name.to_string(),
            current: None,
            target: "1.0-1".to_string(),
            reason: "runtime dependency".to_string(),
            install_reason: crate::domain::planning::install::Why::Dependency,
            archive: format!("{name}-1.0-1.nawa.tar.zst"),
            sha256: digest.to_string(),
            size: 123,
            download_url: Some(format!("https://example.test/{name}.tar.zst")),
        };
        let mut plan = SystemPlan {
            snapshot,
            packages: Ok(crate::domain::system::package_assessment::PackageAssessment::default()),
            repository: RepositoryAssessment::Opened {
                kind: RepositoryKind::Remote,
                source_fingerprint: "a".repeat(64),
                index_schema: Some(5),
                index_sha256: "b".repeat(64),
                trusted_signer_sha256: Some("c".repeat(64)),
                authenticated_by_trusted_key: true,
                archive_auth: ArchiveAuth::SignedIndexSha256,
                package_count: 2,
            },
            transition: TransitionAssessment::Preview(SystemTransitionPreview::Candidate {
                id: "faseela-runit-to-systemd-v1".to_string(),
                version: 1,
                from: "init-system=runit".to_string(),
                to: "init-system=systemd".to_string(),
                profile_source: "/usr/share/nawa/system-profile.toml".to_string(),
                provider_package: "systemd".to_string(),
                package_actions: vec![
                    action("libfoo", &"1".repeat(64)),
                    action("systemd", &"2".repeat(64)),
                ],
                requires_reboot: true,
            }),
            identity: format!("sp-{}", "d".repeat(64)),
        };

        plan.identity = plan
            .pin_evidence("faseela-runit-to-systemd-v1")
            .unwrap()
            .computed_identity();
        let pin = plan.approval_pin("faseela-runit-to-systemd-v1").unwrap();
        assert_eq!(pin.identity, plan.identity);
        assert_eq!(
            pin.installed_packages,
            vec![("runit".to_string(), "2.1-1".to_string())]
        );
        assert_eq!(
            pin.artifacts
                .iter()
                .map(|item| item.name.as_str())
                .collect::<Vec<_>>(),
            ["libfoo", "systemd"]
        );
        assert_eq!(pin.artifacts[0].sha256, "1".repeat(64));
        assert_eq!(pin.artifacts[1].sha256, "2".repeat(64));
    }

    #[test]
    fn approval_pin_rejects_unresolved_or_untrusted_repository_plans() {
        let root = root("system-plan-pin-negative");
        let plan = build(&root, Path::new("/proc"), None);
        assert!(plan.approval_pin("faseela-runit-to-systemd-v1").is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn verification_rejects_changed_observations_before_repository_access() {
        let root = root("system-plan-pin-stale-observation");
        let snapshot = observation::observe(&root, Path::new("/proc"));
        let mut pin = crate::domain::system::plan::fixture_pin();
        pin.observed_sha256 = fingerprint(format!("{:?}", snapshot.observed).as_bytes());
        pin.installed_packages = installed_packages(&snapshot).unwrap();
        pin.installed_identities = snapshot.installed_identities.clone();
        pin.identity = pin.computed_identity();
        let profile = root.join("usr/share/nawa/system-profile.toml");
        fs::create_dir_all(profile.parent().unwrap()).unwrap();
        fs::write(
            &profile,
            "schema = 1\n[providers]\ninit-system = \"systemd\"\n",
        )
        .unwrap();

        let error = verify_approval_pin(&root, Path::new("/proc"), "unused-repository", &pin)
            .err()
            .unwrap();
        assert!(
            format!("{error:#}").contains("observed system facts changed"),
            "{error:#}"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn offline_plan_without_repository_is_typed_and_stable() {
        let root = root("system-plan-offline");
        let first = build(&root, Path::new("/proc"), None);
        let second = build(&root, Path::new("/proc"), None);

        assert!(matches!(
            &first.snapshot.observed.context,
            crate::domain::system::observation::Context::OfflineRoot { .. }
        ));
        assert_eq!(first.repository, RepositoryAssessment::NotConfigured);
        assert!(matches!(
            &first.transition,
            TransitionAssessment::NotEvaluated { .. }
        ));
        assert_eq!(first.identity, second.identity);
        assert!(first.identity.starts_with("sp-"));
        assert_eq!(first.identity.len(), 67);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn offline_remote_plan_does_not_materialize_or_write_a_cache() {
        let root = root("system-plan-offline-no-network");
        let plan = build(
            &root,
            Path::new("/proc"),
            Some("https://invalid.example.test/repo"),
        );
        assert!(matches!(
            plan.repository,
            RepositoryAssessment::Unavailable { .. }
        ));
        assert!(!root.join("var").exists());
        assert!(!root.join(".nawa").exists());
    }

    #[test]
    fn remote_plan_uses_an_existing_trusted_cache_without_refreshing_or_marking_it_seen() {
        let root = root("system-plan-cached-remote-readonly");
        let (secret, public) = crate::adapters::sign::keygen(&root.join("keys")).unwrap();
        crate::adapters::sign::trust(&public, &root).unwrap();
        let repository = "https://mirror.example.test/nawa";
        let cache = crate::fetch::remote_cache_dir(&root, repository);
        fs::create_dir_all(&cache).unwrap();
        let index = crate::repo::Index {
            generated: Some(crate::adapters::artifact::now_secs()),
            expires: Some(crate::adapters::artifact::now_secs() + 3600),
            repository: Some(crate::repo::RepositoryMeta::v5_per_package_url()),
            recipes: None,
            groups: None,
            packages: Vec::new(),
        };
        let path = cache.join("index.toml");
        fs::write(&path, toml::to_string(&index).unwrap()).unwrap();
        crate::adapters::sign::sign(&path, &secret).unwrap();
        let before = fs::read(&path).unwrap();
        let plan = build(&root, Path::new("/proc"), Some(repository));
        assert!(matches!(
            plan.repository,
            RepositoryAssessment::Opened {
                authenticated_by_trusted_key: true,
                ..
            }
        ));
        assert_eq!(fs::read(path).unwrap(), before);
        assert!(!cache.join("index.seen").exists());
        assert!(!root.join(".nawa").exists());
    }

    #[test]
    fn plan_identity_changes_when_installed_inventory_changes() {
        let root = root("system-plan-inventory");
        let before = build(&root, Path::new("/proc"), None);

        let package = root.join("var/lib/nawa/local/example");
        fs::create_dir_all(&package).unwrap();
        fs::write(
            package.join("desc"),
            "name = \"example\"\nversion = \"1.0\"\nrel = 1\n",
        )
        .unwrap();

        let after = build(&root, Path::new("/proc"), None);
        assert_ne!(before.identity, after.identity);
        let _ = fs::remove_dir_all(root);
    }
}
