//! Recover installed declarations only from the exact, verified stored archive.
//! Legacy `Entry.run = []` cannot distinguish absent from explicitly empty.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{bail, Result};

use crate::domain::package::metadata::PkgMetadata;
use crate::domain::system::package_assessment::PackageState;

pub(crate) fn capture(root: &Path, entries: &[super::db::Entry]) -> BTreeMap<String, PackageState> {
    entries
        .iter()
        .map(|entry| {
            let metadata = read(root, entry).unwrap_or_else(|_| PkgMetadata::unknown());
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

fn read(root: &Path, entry: &super::db::Entry) -> Result<PkgMetadata> {
    let path = super::artifact::object_path(root, &entry.artifact_sha256)?;
    super::artifact::verify_object_file(&path, &entry.artifact_sha256)?;
    let raw = super::archive::read_pkginfo(&path)?;
    let info = super::archive::parse_pkginfo(&raw)?;
    if info.get("pkgname") != Some(&entry.name)
        || info.get("pkgver") != Some(&entry.version)
        || info.get("pkgrel").and_then(|s| s.parse::<u32>().ok()) != Some(entry.rel)
        || info.get("arch") != Some(&entry.arch)
    {
        bail!("stored archive identity differs from installed record");
    }
    PkgMetadata::from_pkginfo_map(&info).map_err(anyhow::Error::msg)
}
