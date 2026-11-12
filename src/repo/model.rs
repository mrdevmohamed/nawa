//! Repository data model.

//! Repository index creation and validation.
use serde::{Deserialize, Serialize};


#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RepoEntry {
    pub name: String,
    pub version: String,
    pub rel: u32,
    pub arch: String,
    pub file: String,
    pub sha256: String,
    /// Absolute HTTPS URL for distributed repositories (schema v3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub download_url: Option<String>,
    /// archive size in bytes (download size); 0 = unknown in legacy indexes
    #[serde(default, skip_serializing_if = "is_zero")]
    pub size: u64,
    /// installed size in bytes, from the archive's size field; 0 = unknown
    #[serde(default, skip_serializing_if = "is_zero")]
    pub installed_size: u64,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

fn default_archive_location() -> String {
    ARCHIVE_LOCATION_BASE_URL.to_string()
}

fn is_base_url(value: &String) -> bool {
    value == ARCHIVE_LOCATION_BASE_URL
}

/// Authentication metadata for a schema-v2 repository.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RepositoryMeta {
    pub schema: u32,
    pub archive_auth: String,
    pub hash_algorithm: String,
    pub index_signature: String,
    /// How clients locate package and recipe archives.
    #[serde(default = "default_archive_location", skip_serializing_if = "is_base_url")]
    pub archive_location: String,
}

impl RepositoryMeta {
    pub fn v2() -> Self {
        Self {
            schema: INDEX_SCHEMA_V2,
            archive_auth: ARCHIVE_AUTH_SIGNED_INDEX.to_string(),
            hash_algorithm: HASH_ALGORITHM_SHA256.to_string(),
            index_signature: INDEX_SIGNATURE_NAWA_SIG_V1.to_string(),
            archive_location: ARCHIVE_LOCATION_BASE_URL.to_string(),
        }
    }

    pub fn v3_per_package_url() -> Self {
        Self {
            schema: INDEX_SCHEMA_V3,
            archive_auth: ARCHIVE_AUTH_SIGNED_INDEX.to_string(),
            hash_algorithm: HASH_ALGORITHM_SHA256.to_string(),
            index_signature: INDEX_SIGNATURE_NAWA_SIG_V1.to_string(),
            archive_location: ARCHIVE_LOCATION_PER_PACKAGE_URL.to_string(),
        }
    }
}

pub(crate) const INDEX_SCHEMA_V2: u32 = 2;
pub(crate) const INDEX_SCHEMA_V3: u32 = 3;
pub(crate) const ARCHIVE_LOCATION_BASE_URL: &str = "base-url";
pub(crate) const ARCHIVE_LOCATION_PER_PACKAGE_URL: &str = "per-package-url";
pub(crate) const ARCHIVE_AUTH_SIGNED_INDEX: &str = "signed-index";
pub(crate) const HASH_ALGORITHM_SHA256: &str = "sha256";
pub(crate) const INDEX_SIGNATURE_NAWA_SIG_V1: &str = "nawa-sig-v1";

/// The recipe tree published next to the packages (nawa sync). It lives in
/// the signed index, so the signature covers the revision and the tarball hash.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RecipesEntry {
    pub rev: String,
    pub file: String,
    pub sha256: String,
    /// Absolute HTTPS URL for distributed recipe storage (schema v3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub download_url: Option<String>,
}

/// Authenticated reference to the standalone package-group catalog.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GroupsEntry {
    pub schema: u32,
    pub file: String,
    pub sha256: String,
    pub size: u64,
}

pub(crate) const RECIPES_SUFFIX: &str = ".recipes.tar.zst";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Index {
    /// Unix time the index was generated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generated: Option<u64>,
    /// Unix time after which clients must not trust this index.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires: Option<u64>,
    /// Present only for schema-v2 repositories. Absence means legacy v1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<RepositoryMeta>,
    /// Recipe tree for nawa sync.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recipes: Option<RecipesEntry>,
    /// Package groups, authenticated by this signed index.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub groups: Option<GroupsEntry>,
    #[serde(rename = "package")]
    pub packages: Vec<RepoEntry>,
}

pub(crate) const INDEX_VALIDITY_SECS: u64 = 90 * 24 * 3600;

