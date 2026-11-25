//! Fill in installed packages whose declarations no local source could
//! recover, from the signed repository index the upgrade already holds.

use std::collections::BTreeMap;

use crate::adapters::db::Entry;
use crate::domain::package::metadata::{metadata_from_repository, PkgMetadata};
use crate::domain::system::package_assessment::PackageState;

/// Fill in packages whose declarations are still unknown from the repository
/// index. Only an exact match counts: same name, version and release, and the
/// archive the entry was installed from is the archive the index lists. An
/// index entry for another build of the package says nothing about this one.
pub(crate) fn backfill_from_index(
    states: &mut BTreeMap<String, PackageState>,
    entries: &[Entry],
    index: &crate::repo::Index,
) {
    for entry in entries {
        let Some(state) = states.get_mut(&entry.name) else {
            continue;
        };
        if state.metadata != PkgMetadata::unknown() || entry.artifact_sha256.is_empty() {
            continue;
        }
        let Some(listed) = index.packages.iter().find(|p| {
            p.name == entry.name
                && p.version == entry.version
                && p.rel == entry.rel
                && p.sha256 == entry.artifact_sha256
        }) else {
            continue;
        };
        if let Ok(metadata) = metadata_from_repository(
            listed.rundeps.as_ref(),
            listed.provides.as_ref(),
            listed.relations.as_ref(),
        ) {
            state.metadata = metadata;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::db::Db;
    use crate::adapters::installed_metadata::capture;
    use crate::repo::{Index, RepoEntry};
    use std::collections::HashMap;

    const SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn entry(name: &str, sha: &str) -> Entry {
        Entry {
            name: name.into(),
            version: "1.0".into(),
            rel: 2,
            arch: "x86_64".into(),
            reason: String::new(),
            run: vec![],
            install_date: 0,
            size: 0,
            artifact_sha256: sha.into(),
        }
    }

    fn root(tag: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!("nawa-backfill-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn listed(name: &str, rel: u32, sha: &str) -> RepoEntry {
        RepoEntry {
            name: name.into(),
            version: "1.0".into(),
            rel,
            arch: "x86_64".into(),
            file: format!("{name}.nawa.tar.zst"),
            sha256: sha.into(),
            download_url: None,
            size: 0,
            installed_size: 0,
            rundeps: Some(vec!["libfoo".into()]),
            provides: Some(vec![]),
            relations: None,
        }
    }

    fn index(packages: Vec<RepoEntry>) -> Index {
        Index {
            generated: None,
            expires: None,
            repository: None,
            recipes: None,
            groups: None,
            packages,
        }
    }

    #[test]
    fn backfill_uses_the_index_only_for_the_exact_installed_archive() {
        let root = root("backfill");
        let exact = entry("curl", SHA);
        let other_sha = entry("zlib", SHA);
        let other_rel = entry("bash", SHA);
        let no_sha = entry("acl", "");
        let entries = vec![exact, other_sha, other_rel, no_sha];
        let db = Db::new(&root);
        for e in &entries {
            db.add(e, &[], &HashMap::new()).unwrap();
        }
        let mut states = capture(&root, &entries);
        let other = "b".repeat(64);
        backfill_from_index(
            &mut states,
            &entries,
            &index(vec![
                listed("curl", 2, SHA),
                listed("zlib", 2, &other),
                listed("bash", 3, SHA),
                listed("acl", 2, SHA),
            ]),
        );
        assert_ne!(states["curl"].metadata, PkgMetadata::unknown());
        for name in ["zlib", "bash", "acl"] {
            assert_eq!(states[name].metadata, PkgMetadata::unknown(), "{name}");
        }
    }

    #[test]
    fn backfill_leaves_metadata_that_is_already_known_alone() {
        let root = root("known");
        let e = entry("curl", SHA);
        let db = Db::new(&root);
        db.add(&e, &[], &HashMap::new()).unwrap();
        db.save_pkginfo(
            "curl",
            "pkgname = curl\npkgver = 1.0\npkgrel = 2\narch = x86_64\nrundeps = libbar\nprovides = \n",
        )
        .unwrap();
        let entries = vec![e];
        let mut states = capture(&root, &entries);
        let before = states["curl"].metadata.clone();
        backfill_from_index(&mut states, &entries, &index(vec![listed("curl", 2, SHA)]));
        assert_eq!(states["curl"].metadata, before);
    }
}
