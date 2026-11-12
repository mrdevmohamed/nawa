//! Repository materialization, index authentication, and archive fetching.
//!
//! A repository is either a local directory or an http(s) URL cached under
//! `<root>/var/cache/nawa/remote/`. Opening one validates trust, parses and
//! authenticates the index, loads the signed group catalog, and selects the
//! archive authentication policy. Fetching an archive then verifies its
//! SHA-256, size, signature policy, and `.PKGINFO` identity against the index.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

use crate::fetch::{self, RepoRef};
use crate::repo;
use crate::sign;

use super::verify::{
    check_archive_matches_index, check_index_freshness, require_trust_for_remote,
    validate_entry_filename,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ArchiveAuth {
    /// Legacy v1 policy: each archive needs its own detached signature.
    DetachedSignature,
    /// V2 policy: a trusted signed index authenticates the archive SHA-256.
    SignedIndexSha256,
}

/// A repository materialized on disk: for a local repo its own directory,
/// for a remote one the per-URL cache holding the freshly fetched index.
pub(crate) struct OpenRepo {
    /// directory holding `index.toml` (and, locally, the archives)
    pub dir: PathBuf,
    /// `Some(url)` for a remote repository — archives are fetched on demand
    pub base_url: Option<String>,
    /// the parsed, signature-checked, freshness-checked index
    pub index: repo::Index,
    /// Authentication method selected only after validating the index signer.
    pub archive_auth: ArchiveAuth,
    /// The exact index bytes that were signature-verified (kept so installs
    /// can retain the proof bundle, roadmap §4.5).
    pub index_bytes: Vec<u8>,
    /// The signature sidecar bytes used for that verification, when one exists.
    pub index_signature: Option<Vec<u8>>,
    /// The public key bytes of the trusted signer, when the index was
    /// authenticated by a configured trusted key.
    pub signer_public: Option<Vec<u8>>,
    /// Parsed catalog whose exact bytes match the signed index digest.
    pub groups: Option<crate::groups::GroupCatalog>,
}

/// Fetch (remote) or locate (local) a repository, then parse and
/// authenticate its index: trust policy for remote repos, signature check,
/// rollback/freeze protection. Shared by `nawa upgrade` and `nawa install`.
pub(crate) fn open_repo(
    root: &Path,
    repo_ref: &RepoRef,
    allow_unsigned: bool,
    force: bool,
) -> Result<OpenRepo> {
    let (repo_dir, base_url) = materialize_repository(root, repo_ref, allow_unsigned)?;

    let index_path = repo_dir.join("index.toml");
    // Read once: parsing and signature verification must consume identical bytes.
    let index_bytes = std::fs::read(&index_path)
        .with_context(|| format!("read repository index {}", index_path.display()))?;
    let index = repo::load_index_bytes(&index_bytes)?;
    let index_trust = sign::verify_index_bytes(&index_bytes, &index_path, root, allow_unsigned)
        .map_err(|e| {
            anyhow::anyhow!("repository index signature is INVALID or untrusted: {e:#}")
        })?;
    let trusted = index_trust.is_trusted();
    let groups = load_groups(&repo_dir, base_url.as_deref(), &index, trusted)?;
    let archive_auth = select_archive_auth(&index, trusted)?;

    // Retain the exact proof bytes of the verification above so later
    // installs can persist them as artifact provenance (roadmap §4.5).
    let index_signature = std::fs::read(sign::sig_path_for(&index_path)).ok();
    let signer_public = match &index_trust {
        sign::IndexTrust::TrustedKey { key_name } => {
            std::fs::read(sign::trust_dir(root).join(key_name)).ok()
        }
        _ => None,
    };

    // Only an index authenticated by a configured trusted key advances the
    // newest-index marker used to prevent remote repository rollback.
    check_index_freshness(&repo_dir, &index, base_url.is_some(), force, trusted)?;
    Ok(OpenRepo {
        dir: repo_dir,
        base_url,
        index,
        archive_auth,
        index_bytes,
        index_signature,
        signer_public,
        groups,
    })
}

/// Materialize a local repository directory, or refresh/fall back to the
/// cache for a remote one. Returns the directory holding `index.toml` and the
/// base URL for remote archive fetches.
fn materialize_repository(
    root: &Path,
    repo_ref: &RepoRef,
    allow_unsigned: bool,
) -> Result<(PathBuf, Option<String>)> {
    match repo_ref {
        RepoRef::Local(dir) => {
            if !dir.is_dir() {
                bail!("repository directory not found: {}", dir.display());
            }
            Ok((dir.clone(), None))
        }
        RepoRef::Remote(url) => {
            require_trust_for_remote(root, url, allow_unsigned)?;
            let cache = fetch::remote_cache_dir(root, url);
            std::fs::create_dir_all(&cache)
                .with_context(|| format!("create cache {}", cache.display()))?;
            let index_refresh_unavailable = refresh_remote_index(url, &cache)?;
            refresh_remote_signature(url, &cache, index_refresh_unavailable)?;
            Ok((cache, Some(url.clone())))
        }
    }
}

/// Refresh `index.toml` in the remote cache, falling back to the cached copy
/// when the mirror is unreachable. A failed cache write is reported as such,
/// never disguised as a network failure.
fn refresh_remote_index(url: &str, cache: &Path) -> Result<bool> {
    let index_path = cache.join("index.toml");
    let index_url = format!("{url}/index.toml");
    if !index_path.exists() {
        crate::nawa_info!("fetching index from {index_url}");
        // land in a .part file first: an interrupted transfer must not
        // leave a truncated index.toml behind as the "cached" copy
        let part = cache.join("index.toml.part");
        fetch::download(&index_url, &part)?;
        std::fs::rename(&part, &index_path)?;
        return Ok(false);
    }

    match fetch::download_opt(&index_url, &cache.join("index.toml.part")) {
        Ok(true) => {
            std::fs::rename(cache.join("index.toml.part"), &index_path)?;
            Ok(false)
        }
        Ok(false) => bail!("remote index not available (HTTP error): {index_url}"),
        Err(e) if crate::diag::has_io_kind(&e, std::io::ErrorKind::PermissionDenied) => {
            let _ = std::fs::remove_file(cache.join("index.toml.part"));
            crate::nawa_warn!("repository cache is not writable; using cached index");
            Ok(true)
        }
        Err(e) => {
            let _ = std::fs::remove_file(cache.join("index.toml.part"));
            crate::nawa_warn!(
                "cannot reach {url} ({}); using cached index",
                e.root_cause()
            );
            Ok(true)
        }
    }
}

/// Refresh the optional index signature sidecar. A truncated cached `.sig`
/// would make every later upgrade fail verification, so downloads land in a
/// `.part` file and are activated atomically.
fn refresh_remote_signature(
    url: &str,
    cache: &Path,
    index_refresh_unavailable: bool,
) -> Result<()> {
    let sig_path = sign::sig_path_for(&cache.join("index.toml"));
    let sig_part = cache.join("index.toml.sig.part");
    match fetch::download_opt(&format!("{url}/index.toml.sig"), &sig_part) {
        Ok(true) => {
            std::fs::rename(&sig_part, &sig_path)?;
        }
        Ok(false) => {
            let _ = std::fs::remove_file(&sig_part);
            let _ = std::fs::remove_file(&sig_path);
            crate::nawa_note!("remote repo publishes no index signature");
        }
        Err(e) if sig_path.exists() => {
            let _ = std::fs::remove_file(&sig_part);
            if !index_refresh_unavailable {
                crate::nawa_warn!(
                    "cannot refresh repository signature; using cached signature ({})",
                    e.root_cause()
                );
            }
        }
        Err(e) => {
            let _ = std::fs::remove_file(&sig_part);
            if !index_refresh_unavailable {
                crate::nawa_note_stderr!("repository signature unavailable ({})", e.root_cause());
            }
        }
    }
    Ok(())
}

/// Group definitions are supplemental metadata: usable only when the index
/// itself was authenticated by a key in the configured trust store. The
/// signed index pins the exact catalog bytes by SHA-256.
fn load_groups(
    repo_dir: &Path,
    base_url: Option<&str>,
    index: &repo::Index,
    trusted: bool,
) -> Result<Option<crate::groups::GroupCatalog>> {
    let Some(entry) = &index.groups else {
        return Ok(None);
    };
    if !trusted {
        bail!("repository publishes package groups but its index is not authenticated by a trusted key");
    }
    let path = repo_dir.join(&entry.file);
    if let Some(url) = base_url {
        fetch::download_cached(&format!("{url}/{}", entry.file), &path, Some(&entry.sha256))
            .with_context(|| format!("fetch signed group catalog {}", entry.file))?;
    } else if !path.is_file() {
        bail!(
            "group catalog {} referenced by the signed index is missing",
            path.display()
        );
    }
    fetch::verify_sha256(&path, &entry.sha256)?;
    let actual_size = std::fs::metadata(&path)
        .with_context(|| format!("stat group catalog {}", path.display()))?
        .len();
    if actual_size != entry.size {
        bail!(
            "group catalog {} size mismatch: index says {} bytes, file has {} bytes",
            entry.file,
            entry.size,
            actual_size
        );
    }
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("read group catalog {}", path.display()))?;
    Ok(Some(
        crate::groups::GroupCatalog::from_toml(&raw)
            .with_context(|| format!("parse signed group catalog {}", path.display()))?,
    ))
}

/// Select archive authentication from the index schema and signer trust.
/// Distributed per-package URLs are rejected outright unless authenticated.
fn select_archive_auth(index: &repo::Index, trusted: bool) -> Result<ArchiveAuth> {
    let distributed_locations = index
        .repository
        .as_ref()
        .map(|meta| meta.archive_location == repo::ARCHIVE_LOCATION_PER_PACKAGE_URL)
        .unwrap_or(false);
    if distributed_locations && !trusted {
        bail!("distributed schema-v3 repositories require a trusted signing key; refusing to use per-package URLs from an unauthenticated index");
    }
    if index.repository.is_some() && trusted {
        crate::nawa_info!(
            "repository schema v{}: package SHA-256 values authenticated by trusted index",
            index
                .repository
                .as_ref()
                .map(|meta| meta.schema)
                .unwrap_or(1)
        );
        Ok(ArchiveAuth::SignedIndexSha256)
    } else {
        if index.repository.is_some() {
            crate::nawa_note_stderr!("v2 index is not authenticated by a configured trusted key; using legacy per-archive signature policy");
        }
        Ok(ArchiveAuth::DetachedSignature)
    }
}

/// Resolve an archive URL while keeping legacy base-URL repositories unchanged.
pub(crate) fn archive_download_url(
    base_url: &str,
    index: &repo::Index,
    file: &str,
    download_url: Option<&str>,
) -> Result<String> {
    let distributed = index
        .repository
        .as_ref()
        .map(|meta| meta.archive_location == repo::ARCHIVE_LOCATION_PER_PACKAGE_URL)
        .unwrap_or(false);
    if distributed {
        Ok(download_url
            .context("schema-v3 archive entry is missing download_url")?
            .to_string())
    } else {
        Ok(format!("{base_url}/{file}"))
    }
}

/// Make one index entry's archive available locally and fully verified:
/// downloaded into the cache (remote) or located in the repo dir (local),
/// then sha256 against the index, signature policy, and the `.PKGINFO`
/// must match what the index promised.
pub(crate) fn fetch_entry(
    root: &Path,
    open: &OpenRepo,
    re: &repo::RepoEntry,
    allow_unsigned: bool,
) -> Result<PathBuf> {
    let repo_dir = &open.dir;
    let base_url = &open.base_url;
    // a malicious or compromised index must not be able to escape the
    // repo/cache directory through crafted archive paths (local indexes
    // are validated exactly like remote ones — repo-create only ever
    // writes plain file names, so nothing legitimate is excluded)
    validate_entry_filename(&re.file)?;
    let src = match base_url {
        None if open
            .index
            .repository
            .as_ref()
            .map(|m| m.archive_location.as_str())
            == Some(repo::ARCHIVE_LOCATION_PER_PACKAGE_URL) =>
        {
            let source_url =
                archive_download_url("", &open.index, &re.file, re.download_url.as_deref())?;
            let cache = fetch::remote_cache_dir(root, &source_url);
            std::fs::create_dir_all(&cache)
                .with_context(|| format!("create archive cache {}", cache.display()))?;
            let dest = cache.join(&re.file);
            fetch::download_cached(&source_url, &dest, Some(&re.sha256))?;
            dest
        }
        None => {
            let p = repo_dir.join(&re.file);
            if !p.exists() {
                bail!("archive {} listed in the index is missing", p.display());
            }
            p
        }
        Some(url) => {
            let dest = repo_dir.join(&re.file);
            let source_url =
                archive_download_url(url, &open.index, &re.file, re.download_url.as_deref())?;
            fetch::download_cached(&source_url, &dest, Some(&re.sha256))?;
            if open.archive_auth == ArchiveAuth::DetachedSignature {
                // Legacy repositories still require the per-archive sidecar.
                let sig_dest = sign::sig_path_for(&dest);
                let sig_part = sig_dest.with_file_name(format!(
                    "{}.part",
                    sig_dest.file_name().unwrap_or_default().to_string_lossy()
                ));
                match fetch::download_opt(&format!("{url}/{}.sig", re.file), &sig_part) {
                    Ok(true) => {
                        std::fs::rename(&sig_part, &sig_dest)
                            .with_context(|| format!("activate {}", sig_dest.display()))?;
                    }
                    Ok(false) => {
                        let _ = std::fs::remove_file(&sig_part);
                        let _ = std::fs::remove_file(&sig_dest);
                    }
                    Err(e) => {
                        let _ = std::fs::remove_file(&sig_part);
                        if !sig_dest.exists() {
                            return Err(e).with_context(|| {
                                format!("could not fetch signature for {}", re.file)
                            });
                        }
                        crate::nawa_warn!(
                            "could not refresh signature for {} ({e:#}); using cached signature",
                            re.file
                        );
                    }
                }
            }
            dest
        }
    };
    // Integrity is always checked against the index, regardless of schema.
    fetch::verify_sha256(&src, &re.sha256)?;
    if re.size > 0 {
        let actual = std::fs::metadata(&src)
            .with_context(|| format!("stat repository archive {}", src.display()))?
            .len();
        if actual != re.size {
            bail!(
                "archive {} size mismatch: index says {} bytes, downloaded file has {} bytes",
                re.file,
                re.size,
                actual
            );
        }
    }
    // Only a trusted v2 index can replace per-archive signatures.
    if open.archive_auth == ArchiveAuth::DetachedSignature {
        sign::verify_for_install(&src, root, None, allow_unsigned)?;
    }
    // the archive must be the package/version the index promised
    // (blocks replaying an older signed archive as an "upgrade")
    check_archive_matches_index(&src, re, open.index.repository.is_some())?;
    Ok(src)
}
