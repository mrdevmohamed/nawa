//! Recover what installed packages declared (runtime dependencies, provides,
//! conflicts, replacements).
//!
//! Sources, in order:
//! 1. the `.PKGINFO` copy kept in the package's own database entry;
//! 2. the exact, verified archive in the object store (`var/cache`), for
//!    entries installed before that copy existed — the cache may be empty;
//! 3. `upgrade::installed_backfill`, for a caller that holds the signed
//!    repository index: an entry installed from exactly the archive the index
//!    lists carries exactly the metadata the index lists.
//!
//! Legacy `Entry.run = []` cannot distinguish absent from explicitly empty,
//! so a package no source can speak for stays [`PkgMetadata::unknown`].

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{bail, Result};

use crate::domain::package::metadata::PkgMetadata;
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
}
