//! Recover what installed packages declared (runtime dependencies, provides,
//! conflicts, replacements).
//!
//! Sources, in order:
//! 1. the `.PKGINFO` copy kept in the package's own database entry;
//! 2. the exact, verified archive in the object store (`var/cache`), for
//!    entries installed before that copy existed — the cache may be empty;
//! 3. [`backfill_from_index`], for a caller that holds the signed repository
//!    index: an entry that was installed from exactly the archive the index
//!    lists carries exactly the metadata the index lists.
//!
//! Legacy `Entry.run = []` cannot distinguish absent from explicitly empty,
//! so a package no source can speak for stays [`PkgMetadata::unknown`].

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{bail, Result};

use crate::domain::package::metadata::{metadata_from_repository, PkgMetadata};
use crate::domain::system::package_assessment::PackageState;

pub(crate) fn capture(root: &Path, entries: &[super::db::Entry]) -> BTreeMap<String, PackageState> {
    let db = super::db::Db::new(root);
    entries
        .iter()
        .map(|entry| {
            let metadata = read_db_copy(&db, entry)
                .or_else(|_| read_object(root, entry))
                .unwrap_or_else(|_| PkgMetadata::unknown());
            (
                entry.name.clone(),
                PackageState {
                    version: format!("{}-{}", entry.version, entry.rel),
                    metadata,
                },
            )
        })
        .collect()
}

/// Fill in packages whose declarations are still unknown from the repository
/// index. Only an exact match counts: same name, version and release, and the
/// archive the entry was installed from is the archive the index lists. An
/// index entry for another build of the package says nothing about this one.
pub(crate) fn backfill_from_index(
    states: &mut BTreeMap<String, PackageState>,
    entries: &[super::db::Entry],
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

fn read_db_copy(db: &super::db::Db, entry: &super::db::Entry) -> Result<PkgMetadata> {
    match db.pkginfo(&entry.name) {
        Some(raw) => from_pkginfo(entry, &raw),
        None => bail!("no metadata copy in the database entry"),
    }
}

fn read_object(root: &Path, entry: &super::db::Entry) -> Result<PkgMetadata> {
    let path = super::artifact::object_path(root, &entry.artifact_sha256)?;
    super::artifact::verify_object_file(&path, &entry.artifact_sha256)?;
    from_pkginfo(entry, &super::archive::read_pkginfo(&path)?)
}

fn from_pkginfo(entry: &super::db::Entry, raw: &str) -> Result<PkgMetadata> {
    let info = super::archive::parse_pkginfo(raw)?;
    if info.get("pkgname") != Some(&entry.name)
        || info.get("pkgver") != Some(&entry.version)
        || info.get("pkgrel").and_then(|s| s.parse::<u32>().ok()) != Some(entry.rel)
        || info.get("arch") != Some(&entry.arch)
    {
        bail!("stored metadata identity differs from installed record");
    }
    PkgMetadata::from_pkginfo_map(&info).map_err(anyhow::Error::msg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::db::{Db, Entry};
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
        let root = std::env::temp_dir().join(format!("nawa-meta-{tag}-{}", std::process::id()));
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
    fn metadata_kept_in_the_db_entry_survives_an_empty_package_cache() {
        let root = root("dbcopy");
        let e = entry("curl", SHA);
        let db = Db::new(&root);
        db.add(&e, &[], &HashMap::new()).unwrap();
        db.save_pkginfo(
            "curl",
            "pkgname = curl\npkgver = 1.0\npkgrel = 2\narch = x86_64\nrundeps = libfoo\nprovides = \n",
        )
        .unwrap();
        // No var/cache/nawa/objects at all, as on the live images.
        let states = capture(&root, &[e]);
        let metadata = &states["curl"].metadata;
        assert_ne!(*metadata, PkgMetadata::unknown());
        assert_eq!(states["curl"].version, "1.0-2");
    }

    #[test]
    fn a_stored_copy_for_another_build_is_not_trusted() {
        let root = root("identity");
        let e = entry("curl", SHA);
        let db = Db::new(&root);
        db.add(&e, &[], &HashMap::new()).unwrap();
        db.save_pkginfo(
            "curl",
            "pkgname = curl\npkgver = 1.0\npkgrel = 1\narch = x86_64\nrundeps = libfoo\nprovides = \n",
        )
        .unwrap();
        let states = capture(&root, &[e]);
        assert_eq!(states["curl"].metadata, PkgMetadata::unknown());
    }

    #[test]
    fn entries_without_any_copy_stay_unknown() {
        let root = root("none");
        let e = entry("curl", SHA);
        Db::new(&root).add(&e, &[], &HashMap::new()).unwrap();
        assert_eq!(
            capture(&root, &[e])["curl"].metadata,
            PkgMetadata::unknown()
        );
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
