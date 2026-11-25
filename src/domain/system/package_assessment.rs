//! Pure assessment of an already selected package operation. This never solves,
//! authorizes a migration, or infers ABI compatibility from a version number.

use std::collections::{BTreeMap, BTreeSet};

use crate::domain::package::metadata::{PkgMetadata, RunDeps};
use crate::domain::package::relations::Requirement;
use crate::domain::system::capability::Provides;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PackageState {
    pub(crate) version: String,
    pub(crate) metadata: PkgMetadata,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PackageChange {
    pub(crate) name: String,
    pub(crate) current: Option<String>,
    pub(crate) target: PackageState,
    pub(crate) archive: String,
    pub(crate) sha256: String,
    pub(crate) size: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Severity {
    Unknown,
    Review,
    Incompatible,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Finding {
    pub(crate) severity: Severity,
    pub(crate) package: String,
    pub(crate) detail: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct PackageAssessment {
    /// Dependency order, including exact archive identity, comes from the solver.
    pub(crate) changes: Vec<PackageChange>,
    pub(crate) findings: Vec<Finding>,
    pub(crate) reboot_recommended: bool,
}

impl PackageAssessment {
    pub(crate) fn blocked(&self) -> bool {
        self.findings
            .iter()
            .any(|f| f.severity == Severity::Incompatible)
    }
}

fn role(name: &str, metadata: &PkgMetadata) -> Option<(&'static str, bool)> {
    // Names are review policy, never provider evidence or an ABI guarantee.
    if let Provides::Declared(caps) = &metadata.provides {
        if caps
            .iter()
            .any(|c| matches!(c.domain.as_str(), "init-system" | "service-manager"))
        {
            return Some((
                "init/service manager: review service state and boot recovery",
                true,
            ));
        }
    }
    match name {
        "glibc" | "musl" => Some((
            "C runtime: ABI compatibility and process restarts need validation",
            false,
        )),
        "linux" => Some((
            "kernel: validate modules, initramfs, bootloader and a bootable fallback",
            true,
        )),
        "systemd" | "systemd-units" | "runit" | "runit-services" | "openrc" | "sysvinit" => Some((
            "init/service manager: review service state and boot recovery",
            true,
        )),
        "openssl" => Some((
            "TLS runtime: validate library consumers and restart affected services",
            false,
        )),
        "gcc" | "binutils" | "llvm" | "clang" | "rust" | "linux-headers" => Some((
            "toolchain: validate compiled consumers and rebuild requirements",
            false,
        )),
        "python" | "python3" | "nodejs" | "golang" | "ruby" | "perl" | "php" | "openjdk" => Some((
            "language runtime: validate extensions, consumers and service restarts",
            false,
        )),
        _ => None,
    }
}

pub(crate) fn assess(
    installed: &BTreeMap<String, PackageState>,
    changes: Vec<PackageChange>,
    removals: &[String],
    resolution_error: Option<&str>,
) -> PackageAssessment {
    let mut report = PackageAssessment {
        changes,
        ..PackageAssessment::default()
    };
    let mut findings = BTreeSet::new();
    let mut add = |severity, package: &str, detail: String| {
        findings.insert(Finding {
            severity,
            package: package.into(),
            detail,
        });
    };
    if let Some(reason) = resolution_error {
        add(Severity::Incompatible, "resolution", reason.into());
    }
    for name in removals {
        add(Severity::Incompatible, name, "replacement requires an explicitly reviewed package operation; init retirement is never automatic".into());
    }
    let mut final_state = installed.clone();
    for name in removals {
        final_state.remove(name);
    }
    for change in &report.changes {
        final_state.insert(change.name.clone(), change.target.clone());
    }
    let changed: BTreeSet<_> = report.changes.iter().map(|c| c.name.as_str()).collect();
    for change in &report.changes {
        let name = &change.name;
        if let Some((detail, reboot)) = role(name, &change.target.metadata) {
            add(Severity::Review, name, detail.into());
            report.reboot_recommended |= reboot;
            add(
                Severity::Unknown,
                name,
                "declared package relations do not establish ABI, runtime or boot compatibility"
                    .into(),
            );
        }
        if matches!(change.target.metadata.rundeps, RunDeps::Unknown) {
            add(
                Severity::Unknown,
                name,
                "candidate runtime dependencies are unknown".into(),
            );
        }
        if change.target.metadata.relations.is_none() {
            add(
                Severity::Unknown,
                name,
                "candidate conflicts and replacements are unknown".into(),
            );
        }
        if let Some(before) = installed.get(name) {
            match (&before.metadata.rundeps, &change.target.metadata.rundeps) {
                (RunDeps::Declared(old), RunDeps::Declared(new)) => {
                    let old: BTreeSet<_> = old.iter().collect();
                    let new: BTreeSet<_> = new.iter().collect();
                    for dep in new.difference(&old) {
                        add(Severity::Review, name, format!("runtime requirement added or changed: {dep}"));
                    }
                    for dep in old.difference(&new) {
                        add(Severity::Review, name, format!("runtime requirement removed or changed: {dep}"));
                    }
                }
                _ => add(Severity::Unknown, name, "installed or candidate dependency declaration is unavailable; dependency changes cannot be established".into()),
            }
        }
    }
    // When the resolver could not build a consistent target set, `final_state`
    // is only the proposed upgrades without the dependencies it would have
    // added, so every unmet requirement would just repeat the resolution error
    // once per package. That error is the finding; the rest is not evidence.
    if !report.changes.is_empty() || !removals.is_empty() {
        for (name, state) in &final_state {
            match &state.metadata.rundeps {
                RunDeps::Unknown => add(Severity::Unknown, name, "runtime dependencies are unknown; reverse dependency compatibility is unresolved".into()),
                RunDeps::Declared(deps) => for raw in deps {
                    match Requirement::parse(raw) {
                        Ok(requirement) => {
                            let satisfied = final_state.get(&requirement.name).is_some_and(|p| requirement.matches(&p.version));
                            // Assess all new requirements and only affected retained edges.
                            if !satisfied && resolution_error.is_none() && (changed.contains(name.as_str()) || changed.contains(requirement.name.as_str()) || removals.contains(&requirement.name)) {
                                add(Severity::Incompatible, name, format!("resulting package set does not satisfy runtime requirement {}", requirement.canonical()));
                            }
                        }
                        Err(reason) => add(Severity::Incompatible, name, reason),
                    }
                },
            }
            match &state.metadata.relations {
                None => add(
                    Severity::Unknown,
                    name,
                    "package relations are unknown; conflict compatibility is unresolved".into(),
                ),
                Some(relations) => {
                    for raw in relations.conflicts.iter().chain(&relations.replaces) {
                        match Requirement::parse(raw) {
                            Ok(requirement)
                                if (changed.contains(name.as_str())
                                    || changed.contains(requirement.name.as_str()))
                                    && final_state
                                        .get(&requirement.name)
                                        .is_some_and(|p| requirement.matches(&p.version)) =>
                            {
                                add(Severity::Incompatible, name, format!("resulting package set still matches conflict/replacement {}", requirement.canonical()));
                            }
                            Err(reason) => add(Severity::Incompatible, name, reason),
                            _ => {}
                        }
                    }
                }
            }
        }
    }
    report.findings = findings.into_iter().collect();
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::package::relations::PackageRelations;

    fn state(version: &str, deps: &[&str]) -> PackageState {
        PackageState {
            version: version.into(),
            metadata: PkgMetadata {
                provides: Provides::Declared(vec![]),
                rundeps: RunDeps::Declared(deps.iter().map(|s| (*s).into()).collect()),
                relations: Some(PackageRelations {
                    schema: 1,
                    conflicts: vec![],
                    replaces: vec![],
                }),
            },
        }
    }
    fn change(name: &str, target: PackageState) -> PackageChange {
        PackageChange {
            name: name.into(),
            current: Some("1-1".into()),
            target,
            archive: format!("{name}-2-1.nawa.tar.zst"),
            sha256: "a".repeat(64),
            size: 100,
        }
    }
    #[test]
    fn normal_and_critical_changes_have_distinct_evidence() {
        let normal = assess(
            &BTreeMap::new(),
            vec![change("editor", state("2-1", &[]))],
            &[],
            None,
        );
        assert!(normal.findings.is_empty());
        for name in [
            "glibc", "linux", "systemd", "openssl", "python", "rust", "gcc",
        ] {
            let report = assess(
                &BTreeMap::new(),
                vec![change(name, state("2-1", &[]))],
                &[],
                None,
            );
            assert!(!report.blocked());
            assert!(
                report
                    .findings
                    .iter()
                    .any(|f| f.severity == Severity::Review),
                "{name}"
            );
            assert!(
                report
                    .findings
                    .iter()
                    .any(|f| f.severity == Severity::Unknown),
                "{name}"
            );
            assert_eq!(
                report.reboot_recommended,
                matches!(name, "linux" | "systemd")
            );
        }
    }
    #[test]
    fn retained_reverse_dependencies_and_conflicts_block_the_operation() {
        let mut consumer = state("1-1", &["openssl<2-1"]);
        consumer
            .metadata
            .relations
            .as_mut()
            .unwrap()
            .conflicts
            .push("openssl>=2-1".into());
        let installed = BTreeMap::from([
            ("consumer".into(), consumer),
            ("openssl".into(), state("1-1", &[])),
        ]);
        let report = assess(
            &installed,
            vec![change("openssl", state("2-1", &[]))],
            &[],
            None,
        );
        assert!(report.blocked());
        assert!(report
            .findings
            .iter()
            .any(|f| f.detail.contains("does not satisfy")));
        assert!(report
            .findings
            .iter()
            .any(|f| f.detail.contains("still matches")));
    }
    #[test]
    fn unknown_is_never_an_empty_declaration_and_findings_are_deterministic() {
        let installed = BTreeMap::from([(
            "app".into(),
            PackageState {
                version: "1-1".into(),
                metadata: PkgMetadata::unknown(),
            },
        )]);
        let changes = vec![change("app", state("2-1", &["new-lib"]))];
        let first = assess(&installed, changes.clone(), &[], None);
        assert_eq!(first, assess(&installed, changes, &[], None));
        assert!(first
            .findings
            .iter()
            .any(|f| f.severity == Severity::Unknown));
        assert!(first.blocked());
    }
    #[test]
    fn replacements_and_resolution_failure_cannot_be_approved_by_force() {
        let report = assess(
            &BTreeMap::new(),
            vec![],
            &["old-init".into()],
            Some("dependency cycle"),
        );
        assert!(report.blocked());
        assert_eq!(report.findings.len(), 2);
    }
    #[test]
    fn a_failed_resolution_is_not_repeated_once_per_unmet_requirement() {
        let installed: BTreeMap<_, _> = [
            ("app".to_string(), state("1-1", &["libfoo"])),
            ("tool".to_string(), state("1-1", &["libfoo"])),
        ]
        .into();
        let changes = vec![change("app", state("1-2", &["libfoo"]))];
        let failed = assess(&installed, changes.clone(), &[], Some("replacement by x"));
        assert!(
            failed
                .findings
                .iter()
                .all(|f| !f.detail.contains("does not satisfy")),
            "{:#?}",
            failed.findings
        );
        assert!(failed.blocked());
        // the same plan without a resolution failure still reports the unmet edge
        let resolved = assess(&installed, changes, &[], None);
        assert!(resolved.findings.iter().any(|f| f
            .detail
            .contains("does not satisfy runtime requirement libfoo")));
    }
}
