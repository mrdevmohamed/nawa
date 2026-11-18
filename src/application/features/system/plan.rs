//! One immutable, read-only SystemPlan assembled from a single system snapshot.
//!
//! This application use case is the composition boundary between observation,
//! provider policy, authenticated repository metadata and the existing domain
//! resolver. It does not fetch packages, open a transaction, or perform a
//! migration. Exact signed-index and archive identities are retained for a
//! later approval/execution integration; no resolver is repeated here.

use std::collections::BTreeMap;
use std::path::Path;

use crate::application::features::system::observation::{
    self, PackageInventory, SystemSnapshot,
};
use crate::fetch::RepoRef;
use crate::upgrade::{ArchiveAuth, OpenRepo, SystemTransitionPreview};

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
    NotEvaluated {
        reason: String,
    },
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
}

pub(crate) fn build(root: &Path, proc: &Path, repository: Option<&str>) -> SystemPlan {
    let snapshot = observation::observe(root, proc);

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
                    match crate::upgrade::open_repo(root, &repo_ref, false, false) {
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
                    }
                }
            },
        },
    };

    let identity = plan_identity(&snapshot, &repository, &transition);
    SystemPlan {
        snapshot,
        repository,
        transition,
        identity,
    }
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
    let trusted_signer_sha256 = open
        .signer_public
        .as_deref()
        .map(fingerprint);
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
    fn offline_plan_without_repository_is_typed_and_stable() {
        let root = root("system-plan-offline");
        let first = build(&root, Path::new("/proc"), None);
        let second = build(&root, Path::new("/proc"), None);

        assert!(matches!(
            first.snapshot.observed.context,
            crate::domain::system::observation::Context::OfflineRoot { .. }
        ));
        assert_eq!(first.repository, RepositoryAssessment::NotConfigured);
        assert!(matches!(
            first.transition,
            TransitionAssessment::NotEvaluated { .. }
        ));
        assert_eq!(first.identity, second.identity);
        assert!(first.identity.starts_with("sp-"));
        assert_eq!(first.identity.len(), 67);
        let _ = fs::remove_dir_all(root);
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
