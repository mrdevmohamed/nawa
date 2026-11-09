//! Index validation and loading.

#[allow(unused_imports)]
use anyhow::{bail, Context, Result};
use std::collections::HashSet;
#[cfg(test)]
use std::path::Path;

use super::model::*;
use super::pack::validate_rev;

/// Load and validate an index. An index without [repository] remains legacy v1.
#[cfg(test)]
pub(crate) fn load_index(path: &Path) -> Result<Index> {
    let raw = std::fs::read(path)
        .with_context(|| format!("read index {}", path.display()))?;
    load_index_bytes(&raw).with_context(|| format!("parse index {}", path.display()))
}

/// Parse and validate exactly the byte sequence whose signature was checked.
pub(crate) fn load_index_bytes(raw: &[u8]) -> Result<Index> {
    let text = std::str::from_utf8(raw).context("repository index is not valid UTF-8")?;
    let idx: Index = toml::from_str(text).context("parse repository index TOML")?;
    idx.validate()?;
    Ok(idx)
}

impl Index {
    pub fn find(&self, name: &str) -> Option<&RepoEntry> {
        self.packages.iter().find(|p| p.name == name)
    }

    /// Validate schema and the stronger invariants required by v2.
    pub fn validate(&self) -> Result<()> {
        let meta = match &self.repository {
            None => {
                if self.groups.is_some() {
                    bail!("group metadata requires a signed schema-v2/v3 repository index");
                }
                return Ok(()); // v1: retain the historical parser/policy
            }
            Some(meta) => meta,
        };
        if !matches!(meta.schema, INDEX_SCHEMA_V2 | INDEX_SCHEMA_V3) {
            bail!("unsupported repository index schema {}; this Nawa supports schemas 2 and 3", meta.schema);
        }
        let per_package_url = meta.schema == INDEX_SCHEMA_V3;
        let expected_location = if per_package_url {
            ARCHIVE_LOCATION_PER_PACKAGE_URL
        } else {
            ARCHIVE_LOCATION_BASE_URL
        };
        if meta.archive_location != expected_location {
            bail!(
                "schema-v{} archive_location {:?} is unsupported; expected {:?}",
                meta.schema, meta.archive_location, expected_location
            );
        }
        if meta.archive_auth != ARCHIVE_AUTH_SIGNED_INDEX {
            bail!("unsupported repository archive_auth {:?}; expected {:?}", meta.archive_auth, ARCHIVE_AUTH_SIGNED_INDEX);
        }
        if meta.hash_algorithm != HASH_ALGORITHM_SHA256 {
            bail!("unsupported repository hash_algorithm {:?}; expected sha256", meta.hash_algorithm);
        }
        if meta.index_signature != INDEX_SIGNATURE_NAWA_SIG_V1 {
            bail!("unsupported repository index_signature {:?}; expected nawa-sig-v1", meta.index_signature);
        }

        let generated = self.generated.context("schema-v2 index is missing generated")?;
        let expires = self.expires.context("schema-v2 index is missing expires")?;
        if generated == 0 || expires <= generated {
            bail!("schema-v2 index has invalid generated/expires timestamps");
        }

        if let Some(groups) = &self.groups {
            if groups.schema != 1 {
                bail!("unsupported group catalog schema {}; expected 1", groups.schema);
            }
            validate_plain_filename(&groups.file)?;
            if !groups.file.ends_with(".toml") {
                bail!("group catalog has an unexpected filename {:?}", groups.file);
            }
            validate_sha256(&groups.sha256, "group catalog")?;
            if groups.size == 0 {
                bail!("group catalog has no size");
            }
        }

        let mut names = HashSet::new();
        let mut files = HashSet::new();
        for p in &self.packages {
            if p.name.trim().is_empty() || p.version.trim().is_empty() || p.arch.trim().is_empty() {
                bail!("schema-v2/v3 package entry has an empty name, version, or arch");
            }
            if p.rel == 0 {
                bail!("schema-v2 package {} has invalid rel 0", p.name);
            }
            validate_archive_filename(&p.file)?;
            validate_sha256(&p.sha256, &format!("package {}", p.name))?;
            if p.size == 0 {
                bail!("schema-v{} package {} has no archive size", meta.schema, p.name);
            }
            if per_package_url {
                let url = p.download_url.as_deref().with_context(|| {
                    format!("schema-v3 package {} has no download_url", p.name)
                })?;
                validate_download_url(url, &format!("package {}", p.name))?;
            } else if p.download_url.is_some() {
                bail!("schema-v2 package {} unexpectedly has download_url", p.name);
            }
            if !names.insert(p.name.as_str()) {
                bail!("schema-v2/v3 index contains duplicate package name {:?}", p.name);
            }
            if !files.insert(p.file.as_str()) {
                bail!("schema-v2/v3 index contains duplicate archive filename {:?}", p.file);
            }
        }
        if let Some(r) = &self.recipes {
            validate_plain_filename(&r.file)?;
            if !r.file.ends_with(RECIPES_SUFFIX) {
                bail!("schema-v2/v3 recipe archive has an unexpected filename {:?}", r.file);
            }
            validate_sha256(&r.sha256, "recipes archive")?;
            if per_package_url {
                let url = r.download_url.as_deref().context("schema-v3 recipes entry has no download_url")?;
                validate_download_url(url, "recipes archive")?;
            } else if r.download_url.is_some() {
                bail!("schema-v2/v3 recipes entry unexpectedly has download_url");
            }
            validate_rev(&r.rev)?;
        }
        Ok(())
    }
}

pub(crate) fn validate_download_url(value: &str, what: &str) -> Result<()> {
    let parsed = url::Url::parse(value)
        .with_context(|| format!("{what} has an invalid download_url"))?;
    if parsed.scheme() != "https"
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.fragment().is_some()
    {
        bail!("{what} download_url must be an absolute HTTPS URL without credentials or fragment");
    }
    Ok(())
}

pub(crate) fn validate_sha256(value: &str, what: &str) -> Result<()> {
    if value.len() != 64 || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("{what} has an invalid SHA-256 digest (expected 64 hexadecimal characters)");
    }
    Ok(())
}

pub(crate) fn validate_plain_filename(file: &str) -> Result<()> {
    if file.is_empty()
        || file.starts_with('.')
        || file.contains('/')
        || file.contains('\\')
        || file.contains("..")
    {
        bail!("index entry has an unsafe filename: {file:?}");
    }
    Ok(())
}

pub(crate) fn validate_archive_filename(file: &str) -> Result<()> {
    validate_plain_filename(file)?;
    if !file.ends_with(".nawa.tar.zst") {
        bail!("schema-v2 package archive has an unexpected filename: {file:?}");
    }
    Ok(())
}

