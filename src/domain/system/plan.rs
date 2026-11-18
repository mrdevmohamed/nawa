//! Immutable approval evidence. No filesystem, repository or execution authority.
//!
//! Ordered arrays are part of the identity. Repository locations are represented
//! only by digests; neither credentials nor private URLs belong in durable state.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::domain::planning::install::Why;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum PackageActionKind {
    Install,
    Upgrade,
    Keep,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum PackageActionSource {
    AuthenticatedRepository,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ArtifactPin {
    pub(crate) kind: PackageActionKind,
    pub(crate) source: PackageActionSource,
    pub(crate) name: String,
    pub(crate) current: Option<String>,
    pub(crate) target: String,
    pub(crate) archive: String,
    pub(crate) sha256: String,
    pub(crate) size: u64,
    pub(crate) download_url_sha256: Option<String>,
    pub(crate) reason: String,
    pub(crate) install_reason: Why,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct InstalledIdentity {
    pub(crate) name: String,
    pub(crate) version: String,
    /// Empty means legacy/unknown, never an authenticated digest.
    pub(crate) sha256: String,
    pub(crate) reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SystemPlanPin {
    pub(crate) identity: String,
    pub(crate) migration_id: String,
    pub(crate) definition_version: u32,
    pub(crate) from: String,
    pub(crate) to: String,
    pub(crate) provider_package: String,
    pub(crate) source_fingerprint: String,
    pub(crate) index_sha256: String,
    pub(crate) signer_sha256: String,
    pub(crate) observed_sha256: String,
    pub(crate) installed_packages: Vec<(String, String)>,
    pub(crate) installed_identities: Vec<InstalledIdentity>,
    pub(crate) artifacts: Vec<ArtifactPin>,
}

pub(crate) fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

impl SystemPlanPin {
    /// Predict only the approved prefix. This is evidence comparison, not a
    /// dependency solve; unrelated installed packages and old providers survive.
    pub(crate) fn inventory_after(&self, count: usize) -> Vec<InstalledIdentity> {
        let mut expected: std::collections::BTreeMap<_, _> = self
            .installed_identities
            .iter()
            .map(|i| (i.name.clone(), i.clone()))
            .collect();
        for artifact in self
            .artifacts
            .iter()
            .filter(|a| a.kind != PackageActionKind::Keep)
            .take(count)
        {
            let old_reason = expected
                .get(&artifact.name)
                .map(|i| i.reason.clone())
                .unwrap_or_default();
            let reason = match artifact.install_reason {
                Why::Update => old_reason,
                Why::Group if old_reason == "explicit" => old_reason,
                Why::Explicit => "explicit".into(),
                Why::Dependency => "dependency".into(),
                Why::Group => "group".into(),
            };
            expected.insert(
                artifact.name.clone(),
                InstalledIdentity {
                    name: artifact.name.clone(),
                    version: artifact.target.clone(),
                    sha256: artifact.sha256.clone(),
                    reason,
                },
            );
        }
        expected.into_values().collect()
    }

    pub(crate) fn computed_identity(&self) -> String {
        let mut material = self.clone();
        material.identity.clear();
        let mut hash = Sha256::new();
        hash.update(b"nawa-approved-system-plan-v2\0");
        // Struct fields and vectors have a stable serialization order.
        hash.update(serde_json::to_vec(&material).expect("plan evidence is serializable"));
        format!("sp-{:x}", hash.finalize())
    }

    pub(crate) fn validate(&self) -> Result<(), String> {
        let definition = super::migration::builtin_faseela_runit_to_systemd();
        if self.migration_id != definition.id
            || self.definition_version != definition.version
            || self.from != definition.from.to_key()
            || self.to != definition.to.to_key()
        {
            return Err("approved migration definition changed or is unsupported".into());
        }
        for value in [
            &self.source_fingerprint,
            &self.index_sha256,
            &self.signer_sha256,
            &self.observed_sha256,
        ] {
            if !is_sha256(value) {
                return Err("invalid approval evidence digest".into());
            }
        }
        if self.identity != self.computed_identity() {
            return Err("SystemPlan identity does not match the ordered approval evidence".into());
        }
        let mut seen = std::collections::BTreeSet::new();
        for artifact in &self.artifacts {
            if !seen.insert(&artifact.name)
                || artifact.size == 0
                || !is_sha256(&artifact.sha256)
                || artifact
                    .download_url_sha256
                    .as_deref()
                    .is_some_and(|v| !is_sha256(v))
                || artifact.reason.is_empty()
            {
                return Err("invalid or duplicate approved artifact".into());
            }
            if artifact.archive.contains('/')
                || artifact.archive.contains('\\')
                || !artifact.archive.ends_with(".nawa.tar.zst")
            {
                return Err("invalid approved archive filename".into());
            }
            let current = self
                .installed_packages
                .iter()
                .find(|(name, _)| name == &artifact.name)
                .map(|(_, v)| v);
            match artifact.kind {
                PackageActionKind::Install if artifact.current.is_some() || current.is_some() => {
                    return Err("install action contradicts approved inventory".into())
                }
                PackageActionKind::Upgrade | PackageActionKind::Keep
                    if artifact.current.as_ref() != current || current.is_none() =>
                {
                    return Err("action contradicts approved current version".into())
                }
                PackageActionKind::Keep if artifact.current.as_ref() != Some(&artifact.target) => {
                    return Err("keep action changes the version".into())
                }
                _ => {}
            }
        }
        let mut previous = None;
        for ((name, version), identity) in self
            .installed_packages
            .iter()
            .zip(&self.installed_identities)
        {
            if previous.is_some_and(|p: &String| p >= name)
                || name != &identity.name
                || version != &identity.version
                || (!identity.sha256.is_empty() && !is_sha256(&identity.sha256))
            {
                return Err("invalid approved inventory identities or ordering".into());
            }
            previous = Some(name);
        }
        if self.installed_packages.len() != self.installed_identities.len()
            || !self
                .artifacts
                .iter()
                .any(|a| a.name == self.provider_package)
        {
            return Err("approval omits inventory or target provider evidence".into());
        }
        Ok(())
    }
}

#[cfg(test)]
pub(crate) fn fixture_pin() -> SystemPlanPin {
    let mut pin = SystemPlanPin {
        identity: String::new(),
        migration_id: "faseela-runit-to-systemd-v1".into(),
        definition_version: 1,
        from: "init-system=runit".into(),
        to: "init-system=systemd".into(),
        provider_package: "systemd".into(),
        source_fingerprint: "1".repeat(64),
        index_sha256: "2".repeat(64),
        signer_sha256: "3".repeat(64),
        observed_sha256: "4".repeat(64),
        installed_packages: Vec::new(),
        installed_identities: Vec::new(),
        artifacts: vec![ArtifactPin {
            kind: PackageActionKind::Install,
            source: PackageActionSource::AuthenticatedRepository,
            name: "systemd".into(),
            current: None,
            target: "1-1".into(),
            archive: "systemd-1-1.nawa.tar.zst".into(),
            sha256: "5".repeat(64),
            size: 123,
            download_url_sha256: None,
            reason: "required by target profile".into(),
            install_reason: Why::Explicit,
        }],
    };
    pin.identity = pin.computed_identity();
    pin
}
