//! Read-only migration-candidate planning for ordinary repository upgrades.
//!
//! This module never applies a migration. It uses signed v4 repository
//! metadata and the existing pure install resolver to preview the selected
//! init provider and its runtime dependency closure. It is deliberately
//! separate from the normal upgrade transaction.

use std::collections::{BTreeMap, HashMap};
use crate::domain::system::migration::builtin_faseela_runit_to_systemd;
use crate::domain::system::observation::{
    Confidence, Context, DesiredInit, ObservedSystem, RuntimeInit,
};
use crate::domain::package::metadata::RunDeps;
use crate::domain::planning::install::{self, Candidate, CandidateOrigin, Why};

use super::{ArchiveAuth, OpenRepo};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Preview {
    Unresolved {
        from: Option<String>,
        to: String,
        profile_source: String,
        reason: String,
    },
    Candidate {
        id: String,
        version: u32,
        from: String,
        to: String,
        profile_source: String,
        provider_package: String,
        package_actions: Vec<PackageAction>,
        requires_reboot: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PackageAction {
    pub(crate) kind: PackageActionKind,
    pub(crate) name: String,
    pub(crate) current: Option<String>,
    pub(crate) target: String,
    pub(crate) reason: String,
    /// The action is pinned to an archive entry in the authenticated index.
    pub(crate) source: PackageActionSource,
    pub(crate) archive: String,
    pub(crate) sha256: String,
    pub(crate) size: u64,
    pub(crate) download_url: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PackageActionKind {
    Install,
    Upgrade,
    Keep,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PackageActionSource {
    AuthenticatedRepository,
}

pub(crate) fn plan(
    observed: &ObservedSystem,
    open: &OpenRepo,
    installed: &HashMap<String, String>,
) -> Option<Preview> {
    build_from_observation(observed, open, installed)
}

fn build_from_observation(
    observed: &ObservedSystem,
    open: &OpenRepo,
    installed: &HashMap<String, String>,
) -> Option<Preview> {
    let (target, profile_source) = match &observed.desired {
        DesiredInit::Declared { identity, source } => (identity.clone(), source.clone()),
        DesiredInit::NotDeclared => {
            let policy = observed
                .effective_profile
                .as_ref()
                .and_then(|profile| profile.provider_policy("init-system"))?;
            if policy.preferred.is_empty() && policy.forbidden.is_empty() {
                return None;
            }

            let profile_source = observed
                .policy_sources
                .get("init-system")
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "system profile policy".to_string());
            let proven_live_identity = match (&observed.context, &observed.runtime) {
                (
                    Context::LiveHost { .. },
                    RuntimeInit::Observed {
                        kind,
                        confidence: Confidence::Proven,
                        ..
                    },
                ) => Some(kind.capability_identity()),
                _ => None,
            };
            let from = runtime_identity(&observed.runtime);

            let Some(active) = proven_live_identity else {
                let reason = match &observed.runtime {
                    RuntimeInit::Observed {
                        kind,
                        confidence: Confidence::Indicated,
                        ..
                    } => format!(
                        "PID 1 is only indicated as {}; the active provider cannot be trusted for policy evaluation",
                        kind.capability_identity()
                    ),
                    RuntimeInit::Unknown { reason } => format!(
                        "the running init cannot be established ({reason}); provider policy cannot be evaluated"
                    ),
                    RuntimeInit::Observed { .. } => {
                        "the selected root is not a verified live host; runtime provider policy cannot be evaluated".to_string()
                    }
                };
                return Some(Preview::Unresolved {
                    from,
                    to: if policy.preferred.is_empty() {
                        "no required replacement".to_string()
                    } else {
                        format!("preferred: {}", policy.preferred.join(", "))
                    },
                    profile_source,
                    reason: format!(
                        "{reason}; preferred [{}] and forbidden [{}] policy do not select a replacement",
                        policy.preferred.join(", "),
                        policy.forbidden.join(", ")
                    ),
                });
            };

            if policy.preferred.contains(&active) {
                return None;
            }
            if policy.forbidden.contains(&active) {
                return Some(Preview::Unresolved {
                    from: Some(active.clone()),
                    to: "no required replacement".to_string(),
                    profile_source,
                    reason: format!(
                        "the active init provider {active} is forbidden, but the profile does not require a replacement; no automatic migration is selected"
                    ),
                });
            }
            if policy.preferred.is_empty() {
                return None;
            }
            return Some(Preview::Unresolved {
                from: Some(active),
                to: format!("preferred: {}", policy.preferred.join(", ")),
                profile_source,
                reason: format!(
                    "the active init does not match the advisory preference [{}]; preferences do not authorize an automatic init transition",
                    policy.preferred.join(", ")
                ),
            });
        },
        DesiredInit::Invalid { reason } => {
            return Some(Preview::Unresolved {
                from: runtime_identity(&observed.runtime),
                to: "unknown".to_string(),
                profile_source: "invalid system profile".to_string(),
                reason: format!("the target system profile is invalid: {reason}"),
            });
        }
    };

    let context_reason = match &observed.context {
        Context::LiveHost { .. } => None,
        Context::Container { .. } => Some(
            "the root is a container; its PID 1 is not the verified host init".to_string(),
        ),
        Context::Chroot { .. } => Some(
            "the root is a chroot; /proc/1 belongs to the outer environment".to_string(),
        ),
        Context::OfflineRoot { .. } => Some(
            "the selected root is offline; the host PID 1 cannot describe its runtime state"
                .to_string(),
        ),
        Context::Unknown { reason } => Some(format!(
            "the execution context cannot be attributed safely: {reason}"
        )),
    };
    if let Some(reason) = context_reason {
        return Some(Preview::Unresolved {
            from: runtime_identity(&observed.runtime),
            to: target,
            profile_source,
            reason,
        });
    }

    let from = match &observed.runtime {
        RuntimeInit::Observed {
            kind,
            confidence: Confidence::Proven,
            ..
        } => kind.capability_identity(),
        RuntimeInit::Observed {
            kind,
            confidence: Confidence::Indicated,
            ..
        } => {
            return Some(Preview::Unresolved {
                from: Some(kind.capability_identity()),
                to: target,
                profile_source,
                reason: "PID 1 identity is only indicated by one evidence source; proven runtime identity is required".to_string(),
            });
        }
        RuntimeInit::Unknown { reason } => {
            return Some(Preview::Unresolved {
                from: None,
                to: target,
                profile_source,
                reason: format!("the running init cannot be established: {reason}"),
            });
        }
    };
    if from == target {
        return None;
    }

    let definition = builtin_faseela_runit_to_systemd();
    if from != definition.from.identity || target != definition.to.identity {
        return Some(Preview::Unresolved {
            from: Some(from.clone()),
            to: target,
            profile_source,
            reason: format!(
                "no built-in migration definition supports init-system={from} → init-system={}",
                definition.to.identity
            ),
        });
    }

    let schema = open.index.repository.as_ref().map(|meta| meta.schema);
    if schema != Some(crate::repo::INDEX_SCHEMA_V4)
        || open.archive_auth != ArchiveAuth::SignedIndexSha256
        || open.signer_public.is_none()
    {
        return Some(Preview::Unresolved {
            from: Some(from),
            to: target,
            profile_source,
            reason: "a trusted signing key and authenticated repository index schema v4 are required; legacy or unsigned metadata cannot select a migration provider".to_string(),
        });
    }

    let wanted = definition.to.to_key();
    let mut provider_candidates = BTreeMap::new();
    for entry in &open.index.packages {
        if !crate::adapters::sys::arch_fits(&entry.arch) {
            continue;
        }
        let Some(metadata) = open.metadata_for(&entry.name) else {
            continue;
        };
        provider_candidates.insert(
            entry.name.clone(),
            Candidate {
                name: entry.name.clone(),
                version: entry.version.clone(),
                rel: entry.rel,
                // Provider discovery needs only capability declarations.
                // The actual install candidates below require known runtime
                // dependencies before they can enter the package solver.
                deps: Vec::new(),
                dependencies_known: false,
                origin: CandidateOrigin::Repository,
                provides: metadata.provides,
            },
        );
    }
    let provider_selection = install::providers_for(&provider_candidates, &definition.to);
    let mut providers = provider_selection.matching;
    let ambiguous = provider_selection.ambiguous;
    if !ambiguous.is_empty() {
        return Some(Preview::Unresolved {
            from: Some(from),
            to: target,
            profile_source,
            reason: format!(
                "provider declarations are ambiguous in package metadata: {}; no provider was selected",
                ambiguous.join(", ")
            ),
        });
    }
    if providers.len() != 1 {
        let reason = if providers.is_empty() {
            let unknown = if provider_selection.unknown.is_empty() {
                String::new()
            } else {
                format!(
                    "; {} architecture-compatible candidate(s) have unknown capability metadata ({})",
                    provider_selection.unknown.len(),
                    provider_selection.unknown.join(", ")
                )
            };
            format!(
                "no architecture-compatible package in the trusted v4 index declares the target init capability{unknown}"
            )
        } else {
            format!(
                "more than one package declares the exclusive target provider ({}); package selection is ambiguous",
                providers.join(", ")
            )
        };
        return Some(Preview::Unresolved {
            from: Some(from),
            to: target,
            profile_source,
            reason,
        });
    }

    let provider_package = providers.remove(0);
    let Some(target_profile) = observed.effective_profile.as_ref() else {
        return Some(Preview::Unresolved {
            from: Some(from),
            to: target,
            profile_source,
            reason: "the observed target profile is unavailable; no package candidate can be planned"
                .to_string(),
        });
    };

    let mut candidates = BTreeMap::new();
    for entry in &open.index.packages {
        if !crate::adapters::sys::arch_fits(&entry.arch) {
            continue;
        }
        let Some(metadata) = open.metadata_for(&entry.name) else {
            continue;
        };
        let RunDeps::Declared(deps) = metadata.rundeps else {
            continue;
        };
        candidates.insert(
            entry.name.clone(),
            Candidate {
                name: entry.name.clone(),
                version: entry.version.clone(),
                rel: entry.rel,
                deps,
                dependencies_known: true,
                origin: CandidateOrigin::Repository,
                provides: metadata.provides,
            },
        );
    }

    let installed_versions = installed.clone();
    let resolution = match install::install_plan(
        std::slice::from_ref(&provider_package),
        &installed_versions,
        &candidates,
        Some(target_profile),
        Why::Explicit,
        true,
    ) {
        Ok(resolution) => resolution,
        Err(error) => {
            return Some(Preview::Unresolved {
                from: Some(from),
                to: target,
                profile_source,
                reason: format!(
                    "the authenticated runtime dependency closure could not be resolved: {error:#}"
                ),
            });
        }
    };

    let mut package_actions = Vec::new();
    for item in resolution.items {
        let Some(candidate) = candidates.get(&item.name) else {
            return Some(Preview::Unresolved {
                from: Some(from),
                to: target,
                profile_source,
                reason: format!(
                    "the solver returned {} without a matching authenticated candidate",
                    item.name
                ),
            });
        };
        let target_version = install::version_rel(&candidate.version, candidate.rel);
        let reason = match item.why {
            Why::Explicit => format!("required by target profile ({wanted})"),
            Why::Dependency => format!(
                "runtime dependency of the selected {provider_package} provider"
            ),
            Why::Group => "selected by a package group".to_string(),
            Why::Update => "requested package update".to_string(),
        };
        let action = match item.replaces {
            Some(current) => {
                let Some(entry) = open.index.find(&item.name) else {
                    return Some(Preview::Unresolved {
                        from: Some(from),
                        to: target,
                        profile_source,
                        reason: format!("the authenticated candidate {} has no matching index entry", item.name),
                    });
                };
                PackageAction {
                    kind: PackageActionKind::Upgrade,
                    name: item.name,
                    current: Some(current),
                    target: target_version,
                    reason,
                    source: PackageActionSource::AuthenticatedRepository,
                    archive: entry.file.clone(),
                    sha256: entry.sha256.clone(),
                    size: entry.size,
                    download_url: entry.download_url.clone(),
                }
            },
            None => {
                let Some(entry) = open.index.find(&item.name) else {
                    return Some(Preview::Unresolved {
                        from: Some(from),
                        to: target,
                        profile_source,
                        reason: format!("the authenticated candidate {} has no matching index entry", item.name),
                    });
                };
                PackageAction {
                    kind: PackageActionKind::Install,
                    name: item.name,
                    current: None,
                    target: target_version,
                    reason,
                    source: PackageActionSource::AuthenticatedRepository,
                    archive: entry.file.clone(),
                    sha256: entry.sha256.clone(),
                    size: entry.size,
                    download_url: entry.download_url.clone(),
                }
            },
        };
        package_actions.push(action);
    }
    for name in resolution.up_to_date {
        if let Some(candidate) = candidates.get(&name) {
            let Some(entry) = open.index.find(&name) else {
                return Some(Preview::Unresolved {
                    from: Some(from),
                    to: target,
                    profile_source,
                    reason: format!("the retained candidate {name} has no matching authenticated index entry"),
                });
            };
            package_actions.push(PackageAction {
                kind: PackageActionKind::Keep,
                name: name.clone(),
                current: installed.get(&name).cloned(),
                target: install::version_rel(&candidate.version, candidate.rel),
                reason: "already satisfies the authenticated target package candidate".to_string(),
                source: PackageActionSource::AuthenticatedRepository,
                archive: entry.file.clone(),
                sha256: entry.sha256.clone(),
                size: entry.size,
                download_url: entry.download_url.clone(),
            });
        }
    }

    Some(Preview::Candidate {
        id: definition.id,
        version: definition.version,
        from: definition.from.to_key(),
        to: definition.to.to_key(),
        profile_source,
        provider_package,
        package_actions,
        requires_reboot: definition.requires_reboot,
    })
}

fn runtime_identity(runtime: &RuntimeInit) -> Option<String> {
    match runtime {
        RuntimeInit::Observed { kind, .. } => Some(kind.capability_identity()),
        RuntimeInit::Unknown { .. } => None,
    }
}

pub(crate) fn render(preview: &Preview) -> Vec<String> {
    match preview {
        Preview::Unresolved { from, to, profile_source, reason } => vec![
            "System transition check (separate from the ordinary package upgrade)".to_string(),
            format!("  Running provider:  {}", from.as_deref().unwrap_or("unknown")),
            format!("  Target provider:   {to}"),
            format!("  Target profile:    {profile_source}"),
            "  Decision:          unresolved — no migration package plan was selected".to_string(),
            format!("  Reason:            {reason}"),
            "  Safety:            this preview changes no packages, init, boot or service state".to_string(),
        ],
        Preview::Candidate {
            id, version, from, to, profile_source, provider_package, package_actions, requires_reboot,
        } => {
            let mut lines = vec![
                "System transition candidate (NOT applied by nawa upgrade)".to_string(),
                format!("  Migration:         {id} (definition v{version})"),
                format!("  Running provider:  {from}"),
                format!("  Target provider:   {to}"),
                format!("  Target profile:    {profile_source}"),
                format!("  Provider package:  {provider_package} (authenticated index v4)"),
                "  Package changes (runtime-dependency closure preview only):".to_string(),
            ];
            if package_actions.is_empty() {
                lines.push("    no package changes; the target provider is already installed at the selected version".to_string());
            }
            for action in package_actions {
                let marker = match action.kind {
                    PackageActionKind::Install => "+",
                    PackageActionKind::Upgrade => "~",
                    PackageActionKind::Keep => "=",
                };
                let version_text = match (&action.current, action.kind) {
                    (Some(current), PackageActionKind::Upgrade) => format!("{current} -> {}", action.target),
                    (Some(current), _) => current.clone(),
                    (None, _) => action.target.clone(),
                };
                lines.push(format!("    {marker} {} {version_text} — {}", action.name, action.reason));
                let source = match action.source {
                    PackageActionSource::AuthenticatedRepository => "authenticated repository",
                };
                let location = if action.download_url.is_some() {
                    "per-package download location pinned"
                } else {
                    "index-relative archive location pinned"
                };
                lines.push(format!(
                    "      source: {source}; {location}; archive: {}; size: {} bytes; sha256: {}",
                    action.archive, action.size, action.sha256
                ));
            }
            lines.extend([
                "  Deferred system actions:".to_string(),
                "    retain the currently running init provider until target PID 1 is verified after reboot".to_string(),
                "    prepare boot/initramfs only through the allowlisted built-in migration handler".to_string(),
                format!("    reboot {} — Nawa never reboots automatically", if *requires_reboot { "required" } else { "not required" }),
                "    defer old-provider cleanup until verification and separate authorized finalization".to_string(),
                "  Safety and limits:".to_string(),
                "    these package actions are not part of the ordinary upgrade transaction".to_string(),
                "    --yes confirms ordinary package upgrades only; it never authorizes this migration".to_string(),
                "    index v4 carries runtime dependency names, not version ranges, conflicts or replacements".to_string(),
                "    no package is removed and no boot configuration is changed by this preview".to_string(),
            ]);
            lines
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::system::identity::InitKind;
    use crate::domain::system::observation::InstalledInit;
    use crate::domain::system::profile::SystemProfile;
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};

    fn root() -> PathBuf {
        crate::testutil::tmpdir("system-transition-profile")
    }

    fn observed(root: &Path, runtime: InitKind) -> ObservedSystem {
        ObservedSystem {
            root: root.to_path_buf(),
            context: Context::LiveHost { evidence: Vec::new() },
            runtime: RuntimeInit::Observed {
                kind: runtime,
                confidence: Confidence::Proven,
                evidence: Vec::new(),
            },
            installed: InstalledInit::default(),
            policy_sources: BTreeMap::new(),
            desired: DesiredInit::Declared {
                identity: "systemd".to_string(),
                source: "/usr/share/nawa/system-profile.toml".to_string(),
            },
            effective_profile: Some(SystemProfile {
                schema: 1,
                distribution: Some("faseela".to_string()),
                profile: Some("desktop".to_string()),
                architecture: Some(std::env::consts::ARCH.to_string()),
                providers: BTreeMap::from([(
                    "init-system".to_string(),
                    "systemd".to_string(),
                )]),
                policies: BTreeMap::from([(
                    "init-system".to_string(),
                    crate::domain::system::profile::ProviderPolicy {
                        required: vec!["systemd".to_string()],
                        preferred: Vec::new(),
                        forbidden: Vec::new(),
                    },
                )]),
            }),
        }
    }

    fn entry(name: &str, deps: &[&str], provides: &[&str]) -> crate::repo::RepoEntry {
        crate::repo::RepoEntry {
            name: name.to_string(),
            version: "1.0".to_string(),
            rel: 1,
            arch: std::env::consts::ARCH.to_string(),
            file: format!("{name}-1.0-1.nawa.tar.zst"),
            sha256: "00".repeat(32),
            download_url: None,
            size: 1,
            installed_size: 1,
            rundeps: Some(deps.iter().map(|value| value.to_string()).collect()),
            provides: Some(provides.iter().map(|value| value.to_string()).collect()),
        }
    }

    fn open_v4(entries: Vec<crate::repo::RepoEntry>) -> OpenRepo {
        OpenRepo {
            dir: PathBuf::new(),
            base_url: None,
            index: crate::repo::Index {
                generated: None,
                expires: None,
                repository: Some(crate::repo::RepositoryMeta::v4_per_package_url()),
                recipes: None,
                groups: None,
                packages: entries,
            },
            archive_auth: ArchiveAuth::SignedIndexSha256,
            index_bytes: b"test-index-bytes".to_vec(),
            index_signature: Some(b"signature".to_vec()),
            signer_public: Some(b"trusted-key".to_vec()),
            groups: None,
        }
    }

    fn action_names(actions: &[PackageAction]) -> Vec<&str> {
        actions.iter().map(|action| action.name.as_str()).collect()
    }

    #[test]
    fn trusted_v4_provider_uses_the_existing_runtime_dependency_solver() {
        let root = root();
        let repo = open_v4(vec![
            entry("libfoo", &[], &[]),
            entry("systemd", &["libfoo"], &["init-system=systemd"]),
        ]);
        let result = build_from_observation(&observed(&root, InitKind::Runit), &repo, &HashMap::new()).unwrap();
        match result {
            Preview::Candidate { provider_package, package_actions, from, to, .. } => {
                assert_eq!(provider_package, "systemd");
                assert_eq!(from, "init-system=runit");
                assert_eq!(to, "init-system=systemd");
                assert_eq!(action_names(&package_actions), ["libfoo", "systemd"]);
                assert!(package_actions.iter().all(|action| action.kind == PackageActionKind::Install));
                assert!(package_actions.iter().all(|action| {
                    action.source == PackageActionSource::AuthenticatedRepository
                        && action.archive.ends_with(".nawa.tar.zst")
                        && action.sha256 == "00".repeat(32)
                        && action.size == 1
                        && action.download_url.is_none()
                }));
                assert!(!action_names(&package_actions).contains(&"runit"));
            }
            other => panic!("expected Candidate, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn unsigned_or_untrusted_index_never_selects_a_migration_provider() {
        let root = root();
        let mut repo = open_v4(vec![entry("systemd", &[], &["init-system=systemd"])]);
        repo.signer_public = None;
        let result = build_from_observation(&observed(&root, InitKind::Runit), &repo, &HashMap::new()).unwrap();
        assert!(matches!(result, Preview::Unresolved { reason, .. } if reason.contains("trusted signing key")));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn multiple_packages_for_the_exclusive_init_provider_are_not_selected_arbitrarily() {
        let root = root();
        let repo = open_v4(vec![
            entry("systemd", &[], &["init-system=systemd"]),
            entry("systemd-alt", &[], &["init-system=systemd"]),
        ]);
        let result = build_from_observation(&observed(&root, InitKind::Runit), &repo, &HashMap::new()).unwrap();
        assert!(matches!(result, Preview::Unresolved { reason, .. } if reason.contains("more than one package")));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_package_declaring_two_init_providers_is_reported_as_ambiguous() {
        let root = root();
        let repo = open_v4(vec![entry("systemd", &[], &["init-system=runit", "init-system=systemd"])]);
        let result = build_from_observation(&observed(&root, InitKind::Runit), &repo, &HashMap::new()).unwrap();
        assert!(matches!(result, Preview::Unresolved { reason, .. } if reason.contains("ambiguous")));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn offline_roots_never_use_host_pid_one_for_a_migration_candidate() {
        let root = root();
        let repo = open_v4(vec![entry("systemd", &[], &["init-system=systemd"])]);
        let mut observation = observed(&root, InitKind::Runit);
        observation.context = Context::OfflineRoot { root: root.clone() };
        let result = build_from_observation(&observation, &repo, &HashMap::new()).unwrap();
        assert!(matches!(result, Preview::Unresolved { reason, .. } if reason.contains("offline")));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn indicated_runtime_does_not_hide_an_unresolved_preference_policy() {
        let root = root();
        let repo = open_v4(vec![entry("systemd", &[], &["init-system=systemd"])]);
        let mut observation = observed(&root, InitKind::Runit);
        observation.desired = DesiredInit::NotDeclared;
        observation.runtime = RuntimeInit::Observed {
            kind: InitKind::Runit,
            confidence: Confidence::Indicated,
            evidence: Vec::new(),
        };
        observation.effective_profile = Some(SystemProfile {
            schema: 2,
            distribution: Some("faseela".to_string()),
            profile: Some("desktop".to_string()),
            architecture: Some(std::env::consts::ARCH.to_string()),
            providers: BTreeMap::new(),
            policies: BTreeMap::from([(
                "init-system".to_string(),
                crate::domain::system::profile::ProviderPolicy {
                    required: Vec::new(),
                    preferred: vec!["runit".to_string()],
                    forbidden: Vec::new(),
                },
            )]),
        });

        let result = build_from_observation(&observation, &repo, &HashMap::new()).unwrap();
        assert!(matches!(
            result,
            Preview::Unresolved { reason, .. }
                if reason.contains("only indicated")
                    && reason.contains("cannot be trusted for policy evaluation")
        ));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn an_active_preferred_provider_satisfies_advisory_policy_without_preview_noise() {
        let root = root();
        let repo = open_v4(vec![entry("systemd", &[], &["init-system=systemd"])]);
        let mut observation = observed(&root, InitKind::Systemd);
        observation.desired = DesiredInit::NotDeclared;
        observation.effective_profile = Some(SystemProfile {
            schema: 2,
            distribution: Some("faseela".to_string()),
            profile: Some("desktop".to_string()),
            architecture: Some(std::env::consts::ARCH.to_string()),
            providers: BTreeMap::new(),
            policies: BTreeMap::from([(
                "init-system".to_string(),
                crate::domain::system::profile::ProviderPolicy {
                    required: Vec::new(),
                    preferred: vec!["systemd".to_string(), "runit".to_string()],
                    forbidden: Vec::new(),
                },
            )]),
        });

        assert!(
            build_from_observation(&observation, &repo, &HashMap::new()).is_none(),
            "the active provider satisfies the first advisory preference; no transition preview is needed"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_forbidden_active_provider_is_reported_without_inventing_a_replacement() {
        let root = root();
        let repo = open_v4(vec![entry("systemd", &[], &["init-system=systemd"])]);
        let mut observation = observed(&root, InitKind::Runit);
        observation.desired = DesiredInit::NotDeclared;
        observation.effective_profile = Some(SystemProfile {
            schema: 2,
            distribution: Some("faseela".to_string()),
            profile: Some("desktop".to_string()),
            architecture: Some(std::env::consts::ARCH.to_string()),
            providers: BTreeMap::new(),
            policies: BTreeMap::from([(
                "init-system".to_string(),
                crate::domain::system::profile::ProviderPolicy {
                    required: Vec::new(),
                    preferred: Vec::new(),
                    forbidden: vec!["runit".to_string()],
                },
            )]),
        });
        observation.policy_sources = BTreeMap::from([(
            "init-system".to_string(),
            PathBuf::from("/etc/nawa/system-profile.toml"),
        )]);

        let result = build_from_observation(&observation, &repo, &HashMap::new()).unwrap();
        assert!(matches!(
            result,
            Preview::Unresolved { reason, profile_source, .. }
                if reason.contains("is forbidden")
                    && reason.contains("does not require a replacement")
                    && profile_source == "/etc/nawa/system-profile.toml"
        ));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn preferred_provider_policy_is_advisory_not_an_automatic_migration_target() {
        let root = root();
        let repo = open_v4(vec![entry("systemd", &[], &["init-system=systemd"])]);
        let mut observation = observed(&root, InitKind::Runit);
        observation.desired = DesiredInit::NotDeclared;
        observation.effective_profile = Some(SystemProfile {
            schema: 2,
            distribution: Some("faseela".to_string()),
            profile: Some("desktop".to_string()),
            architecture: Some(std::env::consts::ARCH.to_string()),
            providers: BTreeMap::new(),
            policies: BTreeMap::from([(
                "init-system".to_string(),
                crate::domain::system::profile::ProviderPolicy {
                    required: Vec::new(),
                    preferred: vec!["systemd".to_string()],
                    forbidden: Vec::new(),
                },
            )]),
        });

        let result = build_from_observation(&observation, &repo, &HashMap::new()).unwrap();
        assert!(matches!(
            result,
            Preview::Unresolved { reason, to, .. }
                if reason.contains("advisory preference")
                    && to == "preferred: systemd"
        ));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn already_matching_runtime_and_profile_produces_no_migration_candidate() {
        let root = root();
        let repo = open_v4(vec![entry("systemd", &[], &["init-system=systemd"])]);
        assert!(build_from_observation(&observed(&root, InitKind::Systemd), &repo, &HashMap::new()).is_none());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn unresolved_runtime_dependency_prevents_a_package_candidate() {
        let root = root();
        let repo = open_v4(vec![entry("systemd", &["missing-library"], &["init-system=systemd"])]);
        let result = build_from_observation(&observed(&root, InitKind::Runit), &repo, &HashMap::new()).unwrap();
        assert!(matches!(result, Preview::Unresolved { reason, .. } if reason.contains("missing-library")));
        let _ = std::fs::remove_dir_all(root);
    }
}
