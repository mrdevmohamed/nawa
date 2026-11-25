//! Shared rendering for SystemPlan and ordinary repository upgrade assessment.

use std::collections::{BTreeMap, BTreeSet};

use crate::domain::system::package_assessment::{PackageAssessment, Severity};

/// How many package names a collapsed line quotes.
const EXAMPLES: usize = 3;

pub(crate) fn render(report: &PackageAssessment) -> Vec<String> {
    let mut lines = vec![format!(
        "  Package assessment: {} selected change(s); {}",
        report.changes.len(),
        if report.blocked() {
            "blocked by incompatible package state"
        } else {
            "no proven package incompatibility"
        }
    )];
    for change in &report.changes {
        lines.push(format!(
            "    {}: {} -> {} (archive {}; sha256 {}; {} bytes)",
            change.name,
            change.current.as_deref().unwrap_or("not installed"),
            change.target.version,
            change.archive,
            change.sha256,
            change.size
        ));
    }
    // A package that is not being changed and whose declarations are unknown
    // says the same thing as hundreds of its neighbours: one line per reason
    // carries it. Findings about the packages being changed, and every
    // review/incompatible finding, stay one per line.
    let changed: BTreeSet<&str> = report.changes.iter().map(|c| c.name.as_str()).collect();
    let mut retained: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for finding in &report.findings {
        if matches!(finding.severity, Severity::Unknown)
            && !changed.contains(finding.package.as_str())
        {
            retained
                .entry(finding.detail.as_str())
                .or_default()
                .push(finding.package.as_str());
            continue;
        }
        let severity = match finding.severity {
            Severity::Unknown => "unknown",
            Severity::Review => "review",
            Severity::Incompatible => "incompatible",
        };
        lines.push(format!(
            "    {severity}: {}: {}",
            finding.package, finding.detail
        ));
    }
    for (detail, names) in retained {
        let shown = names
            .iter()
            .take(EXAMPLES)
            .copied()
            .collect::<Vec<_>>()
            .join(", ");
        let more = if names.len() > EXAMPLES { ", …" } else { "" };
        lines.push(format!(
            "    unknown: {} unchanged package(s) ({shown}{more}): {detail}",
            names.len()
        ));
    }
    if report.reboot_recommended {
        lines.push("    Reboot recommended after validation; retain a tested bootable fallback and rescue path.".into());
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::system::package_assessment::Finding;

    fn finding(severity: Severity, package: &str, detail: &str) -> Finding {
        Finding {
            severity,
            package: package.into(),
            detail: detail.into(),
        }
    }

    #[test]
    fn unknowns_of_unchanged_packages_collapse_to_one_line_per_reason() {
        let report = PackageAssessment {
            findings: ["acl", "attr", "bash", "bc", "bzip2"]
                .iter()
                .flat_map(|name| {
                    [
                        finding(Severity::Unknown, name, "package relations are unknown"),
                        finding(Severity::Unknown, name, "runtime dependencies are unknown"),
                    ]
                })
                .collect(),
            ..PackageAssessment::default()
        };
        let lines = render(&report);
        assert_eq!(lines.len(), 3, "{lines:#?}");
        assert!(lines[1].contains("5 unchanged package(s) (acl, attr, bash, …): package relations"));
        assert!(lines[2].contains("5 unchanged package(s)"));
    }

    #[test]
    fn findings_about_changed_packages_and_non_unknown_findings_stay_individual() {
        let report = PackageAssessment {
            changes: vec![crate::domain::system::package_assessment::PackageChange {
                name: "systemd-units".into(),
                current: Some("0.5-4".into()),
                target: crate::domain::system::package_assessment::PackageState {
                    version: "0.5-5".into(),
                    metadata: crate::domain::package::metadata::PkgMetadata::unknown(),
                },
                archive: "systemd-units-0.5-5.nawa.tar.zst".into(),
                sha256: "a".repeat(64),
                size: 1,
            }],
            findings: vec![
                finding(
                    Severity::Unknown,
                    "systemd-units",
                    "candidate runtime dependencies are unknown",
                ),
                finding(Severity::Review, "systemd-units", "review service state"),
                finding(
                    Severity::Incompatible,
                    "acl",
                    "resulting package set does not satisfy attr",
                ),
                finding(Severity::Unknown, "acl", "package relations are unknown"),
            ],
            ..PackageAssessment::default()
        };
        let text = render(&report).join("\n");
        assert!(text.contains("unknown: systemd-units: candidate runtime dependencies are unknown"));
        assert!(text.contains("review: systemd-units: review service state"));
        assert!(text.contains("incompatible: acl: resulting package set does not satisfy attr"));
        assert!(
            text.contains("unknown: 1 unchanged package(s) (acl): package relations are unknown")
        );
    }
}
