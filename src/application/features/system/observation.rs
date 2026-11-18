//! Compose one immutable system snapshot for planning.
//!
//! The detector owns runtime/context/profile/filesystem observations. This
//! use case adds the installed package inventory from the selected target
//! root, preserving a read failure as an explicit Unknown state instead of
//! silently treating a corrupt database as empty.

use std::collections::HashMap;
use std::path::Path;

use crate::application::features::system::detect::{self, ObservedSystem};

/// Package inventory captured from the same target root as the system facts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PackageInventory {
    Available(HashMap<String, String>),
    Unavailable { reason: String },
}

/// Read-only snapshot consumed by system planning. All fields are collected
/// once; downstream planners must not independently re-read the package DB.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SystemSnapshot {
    pub(crate) observed: ObservedSystem,
    pub(crate) packages: PackageInventory,
}

pub(crate) fn observe(root: &Path, proc: &Path) -> SystemSnapshot {
    let observed = detect::observe(root, proc);
    let packages = match crate::adapters::db::Db::new(root).installed() {
        Ok(entries) => PackageInventory::Available(
            entries
                .into_iter()
                .map(|entry| {
                    (
                        entry.name,
                        crate::domain::planning::install::version_rel(&entry.version, entry.rel),
                    )
                })
                .collect(),
        ),
        Err(error) => PackageInventory::Unavailable {
            reason: format!("{error:#}"),
        },
    };
    SystemSnapshot { observed, packages }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn root(tag: &str) -> std::path::PathBuf {
        crate::testutil::tmpdir(tag)
    }

    #[test]
    fn missing_local_database_is_a_known_empty_inventory() {
        let root = root("system-snapshot-empty-db");
        let snapshot = observe(&root, Path::new("/proc"));
        assert!(matches!(
            snapshot.packages,
            PackageInventory::Available(ref packages) if packages.is_empty()
        ));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn installed_versions_are_captured_from_the_selected_target_root() {
        let root = root("system-snapshot-installed-db");
        let package = root.join("var/lib/nawa/local/example");
        fs::create_dir_all(&package).unwrap();
        fs::write(
            package.join("desc"),
            "name = \"example\"\nversion = \"2.4\"\nrel = 3\n",
        )
        .unwrap();
        let snapshot = observe(&root, Path::new("/proc"));
        match snapshot.packages {
            PackageInventory::Available(packages) => {
                assert_eq!(packages.get("example").map(String::as_str), Some("2.4-3"));
            }
            other => panic!("expected available inventory, got {other:?}"),
        }
        assert!(matches!(
            snapshot.observed.context,
            crate::domain::system::observation::Context::OfflineRoot { .. }
        ));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn corrupt_local_database_is_unknown_not_an_empty_inventory() {
        let root = root("system-snapshot-corrupt-db");
        let package = root.join("var/lib/nawa/local/example");
        fs::create_dir_all(&package).unwrap();
        fs::write(package.join("desc"), "this is not valid package metadata").unwrap();
        let snapshot = observe(&root, Path::new("/proc"));
        match snapshot.packages {
            PackageInventory::Unavailable { reason } => {
                assert!(reason.contains("corrupt db entry"), "{reason}");
            }
            other => panic!("expected unavailable inventory, got {other:?}"),
        }
        let _ = fs::remove_dir_all(root);
    }
}
