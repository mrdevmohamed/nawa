//! Shared rendering for SystemPlan and ordinary repository upgrade assessment.

use crate::domain::system::package_assessment::{PackageAssessment, Severity};

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
    for finding in &report.findings {
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
    if report.reboot_recommended {
        lines.push("    Reboot recommended after validation; retain a tested bootable fallback and rescue path.".into());
    }
    lines
}
