//! Content-addressed archive object store (Phase 0).
//!
//! The version-named cache (`<root>/var/cache/nawa/gcc-15.1-1.nawa.tar.zst`)
//! is useful to humans but says nothing about the bytes inside: two different
//! archives can carry the same package filename across a republication or a
//! rebuild. This module stores verified archive bytes under their full
//! SHA-256 digest, published immutably:
//!
//! ```text
//! <root>/var/cache/nawa/
//! └── objects/sha256/<xx>/<full-digest>.nawa.tar.zst
//! ```
//!
//! Publication rules (see the roadmap, §4.4):
//! 1. bytes land in a unique temporary file in the target filesystem;
//! 2. the digest is computed from the stored bytes themselves;
//! 3. an expected digest mismatch is rejected;
//! 4. publication never overwrites a different existing object — a no-clobber
//!    hard link (`link(2)`) publishes the file, so a racing publisher either
//!    wins or finds its identical object already in place;
//! 5. an already-published object with the same digest is verified before the
//!    temporary duplicate is discarded;
//! 6. symlinks supplied by untrusted cache entries are never followed (the
//!    object path is built from a validated hex digest and opened without
//!    following a final-component symlink).
//!
//! Legacy version-named cache entries remain untouched and readable.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// `<root>/var/cache/nawa/objects/sha256` — the immutable object store root.
pub fn store_dir(root: &Path) -> PathBuf {
    root.join("var/cache/nawa/objects/sha256")
}

/// The immutable object path for a full (validated) SHA-256 digest.
pub fn object_path(root: &Path, sha256: &str) -> Result<PathBuf> {
    validate_digest(sha256)?;
    let fan = &sha256[..2];
    Ok(store_dir(root).join(fan).join(format!("{sha256}.nawa.tar.zst")))
}

/// A digest is 64 lowercase hex characters; it becomes path components, so
/// anything else is rejected outright.
pub fn validate_digest(sha256: &str) -> Result<()> {
    let ok = sha256.len() == 64
        && sha256.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    if !ok {
        bail!("invalid object digest {sha256:?} (expected 64 lowercase hex characters)");
    }
    Ok(())
}

/// Normalize a digest recorded elsewhere (trust-store style uppercase is
/// tolerated on input, always stored lowercase).
pub fn normalize_digest(sha256: &str) -> Result<String> {
    validate_digest(sha256)?;
    Ok(sha256.to_ascii_lowercase())
}

fn random_suffix() -> String {
    // /dev/urandom is always present on Linux; the fallback only needs to be
    // unique per-process, which the pid+counter already provides.
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut buf = [0u8; 8];
    let rand = std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .map(|()| u64::from_be_bytes(buf))
        .unwrap_or(0);
    format!("{}-{}-{}", std::process::id(), rand, n)
}

/// An open, exclusive-create handle on a temporary file inside the object
/// store's filesystem. Removed on drop unless [`Self::publish`] succeeded.
pub struct StagedObject {
    root: PathBuf,
    tmp: PathBuf,
    file: Option<File>,
    digest: Option<String>,
}

impl Drop for StagedObject {
    fn drop(&mut self) {
        // an unpublished temporary file is garbage; never a published object
        let _ = std::fs::remove_file(&self.tmp);
    }
}

impl StagedObject {
    /// Start staging an object for `root`'s store.
    pub fn create(root: &Path) -> Result<Self> {
        let dir = store_dir(root);
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("create object store {}", dir.display()))?;
        for _ in 0..64 {
            let tmp = dir.join(format!(".obj-{}.tmp", random_suffix()));
            match OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&tmp)
            {
                Ok(file) => {
                    return Ok(StagedObject { root: root.to_path_buf(), tmp, file: Some(file), digest: None })
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e).with_context(|| format!("stage object {}", tmp.display())),
            }
        }
        bail!("could not allocate a unique object staging file in {}", dir.display());
    }

    /// Copy `source` into the staging file, hashing the bytes that will be
    /// stored. Rejects an expected-digest mismatch before anything is
    /// published.
    pub fn absorb(&mut self, source: &Path, expected: Option<&str>) -> Result<String> {
        let mut src = File::open(source)
            .with_context(|| format!("open {}", source.display()))?;
        let (mut file, mut hasher) = (self.file.take().expect("staging file consumed twice"), Sha256::new());
        let result = (|| -> Result<()> {
            let mut buf = [0u8; 128 * 1024];
            loop {
                let n = src.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                file.write_all(&buf[..n])?;
                hasher.update(&buf[..n]);
            }
            file.sync_data()
                .with_context(|| format!("flush staged object {}", self.tmp.display()))?;
            Ok(())
        })();
        match result {
            Ok(()) => {
                self.file = Some(file);
                let digest = hex::encode(hasher.finalize());
                if let Some(expected) = expected {
                    let want = normalize_digest(expected)?;
                    if digest != want {
                        bail!(
                            "refusing to publish object: digest mismatch\n  expected: {want}\n  got:      {digest}"
                        );
                    }
                }
                self.digest = Some(digest.clone());
                Ok(digest)
            }
            Err(e) => Err(e),
        }
    }

    /// Publish the staged bytes as an immutable object. Returns the final
    /// object path. A hard link publishes without clobbering: if the object
    /// already exists, the existing file is kept (never overwritten) and the
    /// staged duplicate is dropped after verifying the existing object.
    pub fn publish(self) -> Result<PathBuf> {
        let digest = self.digest.clone().expect("publish before absorb");
        let dest = object_path(&self.root, &digest)?;
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create object fan {}", parent.display()))?;
        }
        // The staging file must survive until the hard link is made; Drop
        // removes it, so publish works on the raw paths, not on self.
        let tmp = self.tmp.clone();
        let published = std::fs::hard_link(&tmp, &dest);
        match published {
            Ok(()) => {
                // the store takes over: make it world-readable like the rest
                // of the package cache, then remove the staging link
                if let Err(e) = set_readonly(&dest) {
                    let _ = std::fs::remove_file(&dest);
                    return Err(e);
                }
                std::fs::remove_file(&tmp)
                    .with_context(|| format!("cleanup staged object {}", tmp.display()))?;
                Ok(dest)
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                // another process (or an earlier run) published this digest:
                // verify the existing object before trusting it
                verify_object_file(&dest, &digest)?;
                Ok(dest)
            }
            Err(e) => Err(e).with_context(|| format!("publish object {}", dest.display())),
        }
    }
}

fn set_readonly(path: &Path) -> Result<()> {
    let mut perms = std::fs::metadata(path)
        .with_context(|| format!("stat {}", path.display()))?
        .permissions();
    perms.set_mode(0o644);
    std::fs::set_permissions(path, perms)
        .with_context(|| format!("secure object {}", path.display()))
}

/// Verify that `path` contains exactly the bytes of `digest` and is a regular
/// file (not a symlink someone dropped into the store).
pub fn verify_object_file(path: &Path, digest: &str) -> Result<()> {
    let digest = normalize_digest(digest)?;
    let md = std::fs::symlink_metadata(path)
        .with_context(|| format!("object {digest} is not present in the store"))?;
    if md.file_type().is_symlink() || !md.is_file() {
        bail!("object {digest} is not a regular file — refusing to follow it");
    }
    let got = crate::fetch::sha256_of_file(path)?;
    if got != digest {
        bail!(
            "cached object {digest} is corrupt (actual digest {got}) — refusing to use it"
        );
    }
    Ok(())
}

/// Stage + absorb + publish in one call. Used by the installer right after it
/// has verified the archive bytes: the object becomes eligible for history
/// references and later restoration.
pub fn store_verified_copy(root: &Path, source: &Path, expected: Option<&str>) -> Result<(PathBuf, String)> {
    let mut staged = StagedObject::create(root)?;
    let digest = staged.absorb(source, expected)?;
    let path = staged.publish()?;
    Ok((path, digest))
}

/// Import a legacy version-named cache archive into the object store after
/// verifying it (roadmap §4.4 compatibility lookup). The legacy file is left
/// in place. Returns the digest when the import succeeded.
pub fn import_legacy(root: &Path, legacy_path: &Path) -> Result<Option<String>> {
    let digest = match crate::fetch::sha256_of_file(legacy_path) {
        Ok(d) => d,
        Err(_) => return Ok(None), // unreadable legacy entry: leave it alone
    };
    if validate_digest(&digest).is_err() {
        return Ok(None);
    }
    let dest = object_path(root, &digest)?;
    if dest.exists() {
        return Ok(Some(digest)); // already imported
    }
    match store_verified_copy(root, legacy_path, Some(&digest)) {
        Ok((_, d)) => Ok(Some(d)),
        Err(_) => Ok(None),
    }
}

// ---------------------------------------------------------------------------
// provenance record (roadmap §4.5)
// ---------------------------------------------------------------------------

/// How the bytes of an installed archive were authenticated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Authentication {
    /// Trusted v2/v3 repository index: the signed index authenticated the
    /// archive digest (and the index bytes + signature are retained as proof).
    SignedIndex,
    /// Detached Ed25519 signature verified against a trusted (or explicitly
    /// pinned) key; the signature bytes and key fingerprint are retained.
    DetachedSignature,
    /// Accepted without a trusted signature in bootstrap mode. This is *not*
    /// an authenticated artifact and must never be described as one.
    UnsignedBootstrap,
    /// Produced by a local source build. A digest alone is not a signature:
    /// the archive is trusted because this machine built it from recipes.
    LocalBuild,
}

impl Authentication {
    /// True when the artifact carries cryptographic publisher proof.
    #[allow(dead_code)] // used by history/undo reporting in later phases
    pub fn is_authenticated(self) -> bool {
        matches!(self, Authentication::SignedIndex | Authentication::DetachedSignature)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Authentication::SignedIndex => "signed-index",
            Authentication::DetachedSignature => "detached-signature",
            Authentication::UnsignedBootstrap => "unsigned-bootstrap",
            Authentication::LocalBuild => "local-build",
        }
    }

    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "signed-index" => Ok(Authentication::SignedIndex),
            "detached-signature" => Ok(Authentication::DetachedSignature),
            "unsigned-bootstrap" => Ok(Authentication::UnsignedBootstrap),
            "local-build" => Ok(Authentication::LocalBuild),
            other => bail!("unknown authentication kind {other:?}"),
        }
    }
}

/// Versioned provenance sidecar describing how one archive was verified.
/// Stored next to its object (and referenced by digest from package
/// descriptions and history records).
///
/// TOML layout (schema 1):
///
/// ```toml
/// schema = 1
/// artifact_sha256 = "…"
/// archive_size = 1234567
/// authentication = "signed-index"
/// verified_at = 1793910000
///
/// [package]
/// name = "openssl"
/// version = "3.5.0"
/// rel = 1
/// arch = "x86_64"
///
/// [repository]
/// id = "faseela"
/// index_sha256 = "…"
/// index_signature_sha256 = "…"
/// signer_fingerprint = "…"
/// trusted_at_install = true
/// ```
///
/// For detached signatures the `[signature]` table records the detached
/// signature bytes' digest and the verifying key fingerprint. For unsigned
/// bootstrap artifacts and local builds both tables are absent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Provenance {
    pub schema: u32,
    pub artifact_sha256: String,
    pub archive_size: u64,
    pub authentication: String,
    /// Unix seconds.
    pub verified_at: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub package: Option<PackageIdentity>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repository: Option<RepositoryProof>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<SignatureProof>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackageIdentity {
    pub name: String,
    pub version: String,
    pub rel: u32,
    #[serde(default)]
    pub arch: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepositoryProof {
    /// Stable repository identifier (or the cache-derived id of a legacy
    /// single-repo configuration).
    pub id: String,
    /// SHA-256 of the exact index bytes that were signature-verified.
    pub index_sha256: String,
    /// SHA-256 of the signature sidecar used for that verification.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index_signature_sha256: Option<String>,
    /// Stable fingerprint of the verifying key.
    pub signer_fingerprint: String,
    #[serde(default)]
    pub trusted_at_install: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignatureProof {
    /// SHA-256 of the detached signature bytes retained in the proof bundle.
    pub signature_sha256: String,
    /// Stable fingerprint of the verifying key.
    pub signer_fingerprint: String,
    #[serde(default)]
    pub trusted_at_install: bool,
}

impl Provenance {
    #[allow(dead_code)] // used by history/undo reporting in later phases
    pub fn auth(&self) -> Result<Authentication> {
        Authentication::parse(&self.authentication)
    }

    pub fn new(auth: Authentication, digest: &str, size: u64) -> Self {
        Provenance {
            schema: 1,
            artifact_sha256: digest.to_string(),
            archive_size: size,
            authentication: auth.as_str().to_string(),
            verified_at: now_secs(),
            package: None,
            repository: None,
            signature: None,
        }
    }

    pub fn to_toml(&self) -> Result<String> {
        Ok(toml::to_string_pretty(self)?)
    }

    pub fn from_toml(raw: &str) -> Result<Self> {
        let p: Provenance = toml::from_str(raw).context("corrupt provenance record")?;
        if p.schema != 1 {
            bail!("unsupported provenance schema {} (expected 1)", p.schema);
        }
        validate_digest(&p.artifact_sha256)?;
        Authentication::parse(&p.authentication)?;
        Ok(p)
    }
}

pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Stable fingerprint for an Ed25519 verifying key: the first 16 bytes of
/// SHA-256 over the compressed Ristretto/Ed25519 point encoding, shown as
/// hex. Used to tie a proof bundle to a key without embedding the key bytes.
pub fn key_fingerprint(public_key: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(public_key);
    hex::encode(&h.finalize()[..16])
}

/// Where the provenance sidecar of an object lives.
pub fn provenance_path(root: &Path, digest: &str) -> Result<PathBuf> {
    Ok(object_path(root, digest)?
        .parent()
        .expect("object path always has a fan-out parent")
        .to_path_buf()
        .join(format!("{digest}.artifact.toml")))
}

/// Persist a provenance record next to its (already published) object.
/// Idempotent: an existing identical record is left alone; a conflicting one
/// is an error (the object store must never rewrite history).
pub fn save_provenance(root: &Path, prov: &Provenance) -> Result<()> {
    let path = provenance_path(root, &prov.artifact_sha256)?;
    if path.exists() {
        let existing = std::fs::read_to_string(&path)
            .with_context(|| format!("read provenance {}", path.display()))?;
        let existing = Provenance::from_toml(&existing)?;
        if existing.artifact_sha256 == prov.artifact_sha256
            && existing.authentication == prov.authentication
        {
            return Ok(());
        }
        bail!(
            "provenance record for {} already exists with different content — refusing to rewrite it",
            prov.artifact_sha256
        );
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_file_name(format!(".{}.{}.tmp", path.file_name().unwrap_or_default().to_string_lossy(), random_suffix()));
    let write = (|| -> Result<()> {
        std::fs::write(&tmp, prov.to_toml()?)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    })();
    if write.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    write
}

/// Load the provenance record for a digest, if one was persisted.
pub fn load_provenance(root: &Path, digest: &str) -> Result<Option<Provenance>> {
    let path = provenance_path(root, digest)?;
    if !path.exists() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("read provenance {}", path.display()))?;
    Ok(Some(Provenance::from_toml(&raw)?))
}

/// Retain the detached-signature bytes of an object in its proof bundle and
/// record their digest inside the provenance record.
pub fn retain_detached_signature(
    root: &Path,
    digest: &str,
    signature_source: &Path,
    fingerprint: &str,
    trusted: bool,
) -> Result<()> {
    let Some(prov) = load_provenance(root, digest)? else {
        bail!("no provenance record for {digest} — cannot attach a signature proof");
    };
    if prov.authentication != Authentication::DetachedSignature.as_str() {
        bail!(
            "provenance for {digest} records authentication {:?}, not a detached signature",
            prov.authentication
        );
    }
    let sig_dest = object_path(root, digest)?
        .parent()
        .expect("object fan dir")
        .join(format!("{digest}.detached.sig"));
    std::fs::copy(signature_source, &sig_dest)
        .with_context(|| format!("retain detached signature {}", signature_source.display()))?;
    let sig_sha = crate::fetch::sha256_of_file(&sig_dest)?;
    let mut updated = prov;
    updated.signature = Some(SignatureProof {
        signature_sha256: sig_sha,
        signer_fingerprint: fingerprint.to_string(),
        trusted_at_install: trusted,
    });
    // replace-in-place is allowed here: the record itself said
    // detached-signature and only gained its evidence table
    let path = provenance_path(root, digest)?;
    std::fs::write(&path, updated.to_toml()?)
        .with_context(|| format!("update provenance {}", path.display()))?;
    Ok(())
}

/// Retain the signed index + signature + signer key of a repository in an
/// object's proof bundle (`<digest>.index.toml`, `<digest>.index.toml.sig`,
/// `<digest>.signer.pub` — shared bytes per digest, no per-package
/// duplication beyond what the filesystem deduplicates by content).
pub fn retain_repository_proof(
    root: &Path,
    digest: &str,
    index_bytes: &[u8],
    index_signature: Option<&[u8]>,
    signer_public: &[u8],
    repo_id: &str,
    trusted: bool,
) -> Result<()> {
    let Some(prov) = load_provenance(root, digest)? else {
        bail!("no provenance record for {digest} — cannot attach a repository proof");
    };
    if prov.authentication != Authentication::SignedIndex.as_str() {
        bail!(
            "provenance for {digest} records authentication {:?}, not a signed index",
            prov.authentication
        );
    }
    let dir = object_path(root, digest)?
        .parent()
        .expect("object fan dir")
        .to_path_buf();
    std::fs::write(dir.join(format!("{digest}.index.toml")), index_bytes)
        .with_context(|| "retain index bytes")?;
    if let Some(sig) = index_signature {
        std::fs::write(dir.join(format!("{digest}.index.toml.sig")), sig)
            .with_context(|| "retain index signature")?;
    }
    std::fs::write(dir.join(format!("{digest}.signer.pub")), signer_public)
        .with_context(|| "retain signer key")?;
    let index_sha = {
        let mut h = Sha256::new();
        h.update(index_bytes);
        hex::encode(h.finalize())
    };
    let sig_sha = match index_signature {
        Some(sig) => {
            let mut h = Sha256::new();
            h.update(sig);
            Some(hex::encode(h.finalize()))
        }
        None => None,
    };
    let mut updated = prov;
    updated.repository = Some(RepositoryProof {
        id: repo_id.to_string(),
        index_sha256: index_sha,
        index_signature_sha256: sig_sha,
        signer_fingerprint: key_fingerprint(signer_public),
        trusted_at_install: trusted,
    });
    let path = provenance_path(root, digest)?;
    std::fs::write(&path, updated.to_toml()?)
        .with_context(|| format!("update provenance {}", path.display()))?;
    Ok(())
}

/// Re-verify a stored object against its provenance record: exact bytes
/// (digest) plus, when the record says so, the retained signature/index
/// proof. Used by rollback and history undo before any restoration.
pub fn verify_stored(root: &Path, digest: &str) -> Result<PathBuf> {
    let digest = normalize_digest(digest)?;
    let path = object_path(root, &digest)?;
    verify_object_file(&path, &digest)?;
    if let Some(prov) = load_provenance(root, &digest)? {
        let size = std::fs::metadata(&path)?.len();
        if prov.archive_size != 0 && prov.archive_size != size {
            bail!(
                "object {digest} size {} does not match its provenance record ({})",
                size, prov.archive_size
            );
        }
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::tmpdir;

    fn write_source(dir: &Path, body: &[u8]) -> PathBuf {
        let p = dir.join("src.nawa.tar.zst");
        std::fs::write(&p, body).unwrap();
        p
    }

    #[test]
    fn digests_are_validated_before_becoming_paths() {
        assert!(validate_digest(&"a".repeat(64)).is_ok());
        for bad in ["", "xyz", &"A".repeat(64), &"g".repeat(64), "../evil"] {
            assert!(validate_digest(bad).is_err(), "{bad:?}");
            assert!(object_path(Path::new("/r"), bad).is_err());
        }
    }

    #[test]
    fn publish_is_immutable_and_duplicate_safe() {
        let base = tmpdir("publish");
        let root = base.join("root");
        let src = write_source(&base, b"archive-bytes-v1");
        let digest = crate::fetch::sha256_of_file(&src).unwrap();

        let (path1, d1) = store_verified_copy(&root, &src, None).unwrap();
        assert_eq!(d1, digest);
        assert!(path1.exists());
        // publishing the same bytes again verifies and keeps one object
        let (path2, _) = store_verified_copy(&root, &src, Some(&digest)).unwrap();
        assert_eq!(path1, path2);
        assert!(verify_stored(&root, &digest).is_ok());

        // a different archive with the SAME digest claim is rejected
        let src2 = write_source(&base, b"archive-bytes-v2");
        let mut staged = StagedObject::create(&root).unwrap();
        assert!(staged.absorb(&src2, Some(&digest)).is_err());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn corrupt_object_is_detected() {
        let base = tmpdir("corrupt");
        let root = base.join("root");
        let src = write_source(&base, b"real bytes");
        let (path, digest) = store_verified_copy(&root, &src, None).unwrap();
        // tamper after publication
        std::fs::write(&path, b"tampered").unwrap();
        assert!(verify_stored(&root, &digest).is_err());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn symlinked_object_is_refused() {
        let base = tmpdir("symlink");
        let root = base.join("root");
        let src = write_source(&base, b"payload");
        let (_, digest) = store_verified_copy(&root, &src, None).unwrap();
        let path = object_path(&root, &digest).unwrap();
        let outside = base.join("outside");
        std::fs::write(&outside, b"outside").unwrap();
        std::fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink(&outside, &path).unwrap();
        assert!(verify_stored(&root, &digest).is_err(), "must not follow a symlinked object");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn provenance_roundtrip_and_schemas() {
        let mut prov = Provenance::new(Authentication::SignedIndex, &"b".repeat(64), 1234);
        prov.package = Some(PackageIdentity {
            name: "openssl".into(),
            version: "3.5.0".into(),
            rel: 1,
            arch: "x86_64".into(),
        });
        prov.repository = Some(RepositoryProof {
            id: "faseela".into(),
            index_sha256: "c".repeat(64),
            index_signature_sha256: Some("d".repeat(64)),
            signer_fingerprint: "abcd".repeat(4),
            trusted_at_install: true,
        });
        let raw = prov.to_toml().unwrap();
        let back = Provenance::from_toml(&raw).unwrap();
        assert_eq!(back.auth().unwrap(), Authentication::SignedIndex);
        assert!(back.auth().unwrap().is_authenticated());
        assert_eq!(back.repository.unwrap().id, "faseela");

        // unsupported schema is rejected
        let bad = raw.replace("schema = 1", "schema = 2");
        assert!(Provenance::from_toml(&bad).is_err());
        // unknown authentication is rejected
        let bad = raw.replace("signed-index", "magic");
        assert!(Provenance::from_toml(&bad).is_err());

        let unsigned = Provenance::new(Authentication::UnsignedBootstrap, &"b".repeat(64), 1);
        assert!(!unsigned.auth().unwrap().is_authenticated());
    }

    #[test]
    fn provenance_persistence_roundtrip() {
        let base = tmpdir("prov-io");
        let root = base.join("root");
        let src = write_source(&base, b"xyz");
        let (_, digest) = store_verified_copy(&root, &src, None).unwrap();
        let mut prov = Provenance::new(Authentication::LocalBuild, &digest, 3);
        prov.package = Some(PackageIdentity {
            name: "demo".into(),
            version: "1.0".into(),
            rel: 1,
            arch: String::new(),
        });
        save_provenance(&root, &prov).unwrap();
        let loaded = load_provenance(&root, &digest).unwrap().unwrap();
        assert_eq!(loaded.authentication, "local-build");
        assert_eq!(loaded.package.unwrap().name, "demo");
        // a conflicting record is refused
        let mut other = prov.clone();
        other.authentication = "signed-index".into();
        assert!(save_provenance(&root, &other).is_err());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn legacy_import_verifies_and_keeps_original() {
        let base = tmpdir("legacy");
        let root = base.join("root");
        let src = write_source(&base, b"legacy payload");
        let digest = crate::fetch::sha256_of_file(&src).unwrap();
        let imported = import_legacy(&root, &src).unwrap();
        assert_eq!(imported.as_deref(), Some(digest.as_str()));
        assert!(object_path(&root, &digest).unwrap().exists());
        assert!(src.exists(), "the legacy file itself is never removed");
        // corrupt legacy entry is not imported
        let bad = base.join("bad.nawa.tar.zst");
        std::fs::write(&bad, b"junk").unwrap();
        std::fs::write(bad.with_extension("sha256"), "not-a-digest").unwrap();
        let _ = bad;
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn key_fingerprint_is_stable() {
        let a = key_fingerprint(b"some-public-key-bytes");
        let b = key_fingerprint(b"some-public-key-bytes");
        let c = key_fingerprint(b"other-key");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(a.len(), 32);
    }
}
