//! Local group membership records (roadmap §9.3).
//!
//! The functions here are the storage layer for the group commands (Phase 5
//! CLI); unit tests exercise them directly.
//!
//! `<root>/var/lib/nawa/groups/installed/<group-id>.toml` remembers the
//! package set the user actually chose when installing the group. It is the
//! source of truth for what an installed group owns — a later remote
//! definition change never rewrites it. It is NOT a second package database:
//! identity and file ownership still come from `local/<name>/desc|files`.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// A recorded group installation.
#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct MembershipRecord {
    pub schema: u32,
    pub group_id: String,
    /// Revision/digest of the definition used at install time, when known.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub definition_revision: String,
    /// Unix seconds.
    pub installed_at: u64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub source_repository: String,
    /// The exact package set the user selected (including optional picks),
    /// so a future metadata change does not retroactively alter the choice.
    pub packages: Vec<String>,
}

pub(crate) fn groups_dir(root: &Path) -> PathBuf {
    root.join("var/lib/nawa/groups")
}

fn installed_dir(root: &Path) -> PathBuf {
    groups_dir(root).join("installed")
}

fn record_path(root: &Path, group_id: &str) -> Result<PathBuf> {
    super::check_group_id(group_id)?;
    Ok(installed_dir(root).join(format!("{group_id}.toml")))
}

/// Persist a membership record atomically.
#[allow(dead_code)]
pub(crate) fn save(root: &Path, record: &MembershipRecord) -> Result<()> {
    if record.schema != 1 {
        anyhow::bail!("unsupported membership schema {}", record.schema);
    }
    super::check_group_id(&record.group_id)?;
    for p in &record.packages {
        crate::db::check_name(p)
            .with_context(|| format!("group {} has an invalid member", record.group_id))?;
    }
    let path = record_path(root, &record.group_id)?;
    let bytes = toml::to_string_pretty(record)?;
    crate::transaction::atomic::write_atomic(&path, bytes.as_bytes(), 0o644)
        .with_context(|| format!("publish membership {}", path.display()))
}

/// Load one group's membership, when the group is installed locally.
pub(crate) fn load(root: &Path, group_id: &str) -> Result<Option<MembershipRecord>> {
    let path = record_path(root, group_id)?;
    if !path.exists() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("read membership {}", path.display()))?;
    let record: MembershipRecord =
        toml::from_str(&raw).with_context(|| format!("corrupt membership for {group_id}"))?;
    if record.schema != 1 {
        anyhow::bail!("unsupported membership schema {} for {group_id}", record.schema);
    }
    Ok(Some(record))
}

/// Remove a membership record (the group is no longer installed). The
/// packages themselves are removed by the group-remove plan, not here.
pub(crate) fn remove(root: &Path, group_id: &str) -> Result<()> {
    let path = record_path(root, group_id)?;
    if path.exists() {
        std::fs::remove_file(&path)
            .with_context(|| format!("remove membership {}", path.display()))?;
    }
    Ok(())
}

/// Every installed group's membership, ordered by group id.
pub(crate) fn load_all(root: &Path) -> Result<Vec<MembershipRecord>> {
    let dir = installed_dir(root);
    let mut out = Vec::new();
    let rd = match std::fs::read_dir(&dir) {
        Ok(rd) => rd,
        Err(_) => return Ok(out),
    };
    let mut ids: Vec<String> = rd
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let n = e.file_name().to_string_lossy().to_string();
            n.strip_suffix(".toml").map(str::to_string)
        })
        .collect();
    ids.sort();
    for id in ids {
        if let Some(r) = load(root, &id)? {
            out.push(r);
        }
    }
    Ok(out)
}

/// Every package still owned by at least one installed group (after
/// excluding `exclude`, used while removing one group).
pub(crate) fn group_owned_packages(root: &Path, exclude: Option<&str>) -> Result<BTreeSet<String>> {
    let mut out = BTreeSet::new();
    for r in load_all(root)? {
        if Some(r.group_id.as_str()) == exclude {
            continue;
        }
        out.extend(r.packages);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::tmpdir;

    fn sample(group: &str, pkgs: &[&str]) -> MembershipRecord {
        MembershipRecord {
            schema: 1,
            group_id: group.into(),
            definition_revision: "abc123".into(),
            installed_at: 1_793_910_000,
            source_repository: "faseela".into(),
            packages: pkgs.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn save_load_remove_and_ownership() {
        let base = tmpdir("io");
        let root = base.join("root");
        save(&root, &sample("development", &["gcc", "make", "git"])).unwrap();
        save(&root, &sample("desktop", &["firefox", "pipewire"])).unwrap();

        let all = load_all(&root).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].group_id, "desktop");

        // ownership across groups, with an exclusion
        let owned = group_owned_packages(&root, None).unwrap();
        assert!(owned.contains("gcc") && owned.contains("pipewire"));
        let without_dev = group_owned_packages(&root, Some("development")).unwrap();
        assert!(!without_dev.contains("gcc"));
        assert!(without_dev.contains("pipewire"));

        remove(&root, "desktop").unwrap();
        assert!(load(&root, "desktop").unwrap().is_none());
        assert_eq!(load_all(&root).unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn path_like_ids_and_members_are_rejected() {
        let base = tmpdir("evil");
        let root = base.join("root");
        assert!(save(&root, &sample("../evil", &["a"])).is_err());
        let mut bad = sample("ok", &["a"]);
        bad.packages.push("../escape".into());
        assert!(save(&root, &bad).is_err());
        let _ = std::fs::remove_dir_all(&base);
    }
}
