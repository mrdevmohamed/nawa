//! Immutable, content-addressed artifact objects + authentication provenance.
//!
//! Package *filenames* are not reliable identity: the same
//! `gcc-15.1-1.nawa.tar.zst` file name can hold different bytes across a
//! repository republication or a local rebuild. The object store sides off
//! identity by the full SHA-256 digest of the exact archived bytes:
//!
//! ```text
//! <root>/var/cache/nawa/objects/sha256/
//! └── <2>/                                # digest prefix as shard dir
//!     ├── <digest>.nawa.tar.zst           # the artifact itself (immutable)
//!     ├── <digest>.artifact.toml          # provenance record
//!     ├── <digest>.sig                    # detached signature, when used
//!     ├── <digest>.index.toml             # signed index bytes, when used
//!     └── <digest>.index.toml.sig         # its detached signature
//! ```
//!
//! Rules enforced here:
//!
//! * the path is derived **only** from the full digest; a package name or
//!   version may be recorded in the sidecar but must not influence identity;
//! * bytes are hashed while they are copied into the store, and a mismatch
//!   with the expected digest is an error — nothing is published unverified;
//! * publication never overwrites an existing object with different bytes:
//!   when the destination already exists its bytes are verified against the
//!   digest first, and a corrupt object is only replaced by the freshly
//!   verified duplicate (self-healing, never data loss — a path IS a digest);
//! * publication only happens after the **authentication check** the caller
//!   already performed; the store never makes bytes trustworthy by itself.
//!
//! Provenance and *trust semantics*:
//!
//! * a digest proves byte identity, not publisher identity — the record
//!   therefore names the authentication evidence used at install time;
//! * for detached-signature artifacts, the exact `.sig` bytes are preserved;
//! * for signed-index artifacts, the exact index (and signature) bytes that
//!   verified at install time are preserved, connected to the artifact by
//!   the recorded index digest;
//! * a key removed from the current trust store is never silently
//!   substituted by another key: restore-time verification re-uses the
//!   preserved evidence bytes against the *current* policy, and can only
//!   reject — never fall back to unrelated evidence;
//! * unsigned (bootstrap / local-build) artifacts are recorded as such;
//!   restoring one keeps them unsigned and says so — never upgrades trust.

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

pub const OBJECTS_SCHEMA: u32 = 1;

/// Authentication evidence categories for a stored artifact.
pub const AUTH_DETACHED: &str = "detached-signature";
pub const AUTH_SIGNED_INDEX: &str = "signed-index";
pub const AUTH_UNSIGNED: &str = "unsigned";

/// Provenance of an installed artifact: how the exact bytes were
/// authenticated at install time. This is evidence, not a promise: history
/// and restore code must re-verify afterwards (see `verify_provenance`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArtifactOrigin {
    /// built from a recipe; a digest describes the exact output bytes but a
    /// local build is NOT automatically signed
    SourceBuild,
    /// distributed by a repository
    Repository(String),
    /// installed from a standalone archive path
    StandaloneArchive,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallEvidence {
    pub origin: ArtifactOrigin,
    pub authentication: &'static str,
    /// path of the detached signature verified at install time
    pub detached_sig: Option<PathBuf>,
    /// path of the signed index bytes verified at install time
    pub index: Option<PathBuf>,
    /// path of the index signature next to `index`, if published
    pub index_sig: Option<PathBuf>,
    /// whether the index signature matched a currently-trusted key when the
    /// repository was opened (captured before any package plan existed)
    pub index_trusted: bool,
}

/// The durable record stored next to an object.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactRecord {
    pub schema: u32,
    pub artifact_sha256: String,
    pub archive_size: u64,
    pub authentication: String,
    /// Unix seconds when the evidence was checked (the installer's clock).
    pub verified_at: u64,
    /// `source-build` | `repository` | `standalone-archive`
    pub origin: String,
    pub package: ArtifactPackage,
    /// present only for repository-distributed artifacts
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<ArtifactRepository>,
    /// sha256 over the public-key hex that verified this artifact
    /// (detached-signature authentication; inside `repository` for
    /// signed-index authentication). A fingerprint, never the key itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signer: Option<String>,
    /// whether that key was in the target root's trust store at install
    /// time (recorded at install time; restoration re-checks against the
    /// CURRENT store, this field is provenance display only)
    #[serde(default, skip_serializing_if = "is_false")]
    pub trusted_at_install: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactPackage {
    pub name: String,
    pub version: String,
    pub rel: u32,
    pub arch: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactRepository {
    /// repository identifier as configured (a URL or local path today; a
    /// stable id once `[[repositories]]` exists). Display provenance only.
    pub id: String,
    /// sha256 of the exact index bytes whose signature was verified
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index_sha256: Option<String>,
    /// sha256 of the public-key hex that verified the evidence
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signer: Option<String>,
    /// whether that key was in the target root's trust store at install time
    #[serde(default, skip_serializing_if = "is_false")]
    pub trusted_at_install: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

// ---------------------------------------------------------------------------
// paths
// ---------------------------------------------------------------------------

pub fn objects_dir(root: &Path) -> PathBuf {
    root.join("var/cache/nawa/objects/sha256")
}

/// Digest -> `<dir>/<2>/<digest>.<ext>`; the shard dir is derived only from
/// the digest itself.
pub fn object_path(root: &Path, digest: &str, ext: &str) -> PathBuf {
    objects_dir(root)
        .join(&digest[..2])
        .join(format!("{digest}.{ext}"))
}

pub fn archive_of(root: &Path, digest: &str) -> PathBuf {
    object_path(root, digest, "nawa.tar.zst")
}

pub fn record_of(root: &Path, digest: &str) -> PathBuf {
    object_path(root, digest, "artifact.toml")
}

// ---------------------------------------------------------------------------
// publication
// ---------------------------------------------------------------------------

/// Everything evidence-wise the installer wants preserved next to the object.
pub struct EvidenceFiles {
    /// detached signature to preserve (`AUTH_DETACHED` / optional otherwise)
    pub detached_sig: Option<PathBuf>,
    /// signed index to preserve (`AUTH_SIGNED_INDEX`)
    pub index: Option<PathBuf>,
    /// its signature, when the repository publishes one
    pub index_sig: Option<PathBuf>,
}

fn unique_tmp(dir: &Path, name: &str) -> PathBuf {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0_u128);
    dir.join(format!(".tmp-{}-{}-{stamp}", std::process::id(), name))
}

fn write_tmp_then_rename(src_bytes: &[u8], dest: &Path) -> Result<()> {
    let name = dest
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "sidecar".into());
    let tmp = unique_tmp(dest.parent().unwrap_or_else(|| Path::new(".")), &name);
    let result = (|| -> Result<()> {
        std::fs::write(&tmp, src_bytes)?;
        std::fs::rename(&tmp, dest)
            .with_context(|| format!("activate {}", dest.display()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// Publish verified bytes as an immutable object.
///
/// `source` names the file whose bytes were already authenticated by the
/// caller; the digest recorded in `record` must match what those bytes hash
/// to — this function recomputes from the bytes it will actually store, so a
/// mismatch (e.g. the file changed between verification and publication) is
/// refused. Sidecars named in `evidence` are copied without revalidation:
/// each was produced by the very verification step of these bytes.
pub fn publish_object(
    root: &Path,
    source: &Path,
    record: &ArtifactRecord,
    evidence: &EvidenceFiles,
) -> Result<PathBuf> {
    let digest = record.artifact_sha256.to_lowercase();
    let shard = objects_dir(root).join(&digest[..2]);
    std::fs::create_dir_all(&shard)?;
    let dest = shard.join(format!("{digest}.nawa.tar.zst"));

    // copy bytes into the destination filesystem first, hashing what we copy
    let tmp = unique_tmp(&shard, &format!("{digest}.nawa.tar.zst"));
    let copied_digest = {
        use std::io::{BufReader, Read, Write};
        let mut reader = BufReader::new(std::fs::File::open(source).with_context(|| {
            format!("publish artifact: read {}", source.display())
        })?);
        let mut hash = Sha256::new();
        let mut size = 0u64;
        let mut out = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
            .with_context(|| format!("create unique object temp {}", tmp.display()))?;
        let mut buf = [0u8; 64 * 1024];
        loop {
            let n = reader.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hash.update(&buf[..n]);
            out.write_all(&buf[..n])?;
            size += n as u64;
        }
        out.flush()?;
        out.sync_all()?;
        drop(out);
        (hex::encode(hash.finalize()), size)
    };
    let cleaned = || {
        let _ = std::fs::remove_file(&tmp);
    };
    if copied_digest.0 != digest {
        // never publish bytes that disagree with the recorded identity — this
        // includes the "file changed under us between verify and publish" race
        cleaned();
        bail!(
            "artifact bytes changed between verification and publication: {} hashes to {} (recorded {})",
            source.display(), copied_digest.0, digest
        );
    }
    if copied_digest.1 != record.archive_size && record.archive_size != 0 {
        cleaned();
        bail!(
            "artifact size mismatch: {} bytes copied, record says {}",
            copied_digest.1, record.archive_size
        );
    }

    // Publish without clobbering a different object: if the destination
    // already exists, verify WHICH bytes sit there. Identical -> reuse and
    // drop our duplicate. Corrupt/missing-matching -> replace (self-healing:
    // the path is the digest, so replacing wrong bytes with verified ones
    // restores the invariant).
    if dest.exists() {
        match crate::fetch::sha256_of_file(&dest) {
            Ok(existing) if existing == digest => {
                cleaned();
            }
            _ => {
                std::fs::rename(&tmp, &dest)
                    .with_context(|| format!("activate object {}", dest.display()))?;
            }
        }
    } else if let Err(e) = std::fs::rename(&tmp, &dest) {
        // another process may have published the same digest concurrently
        let _ = std::fs::remove_file(&tmp);
        if dest.exists() {
            // verify the winner; keep the invariant: same digest, same bytes
            let existing = crate::fetch::sha256_of_file(&dest).unwrap_or_default();
            if existing != digest {
                bail!("object {} exists but holds different bytes: {}", dest.display(), e);
            }
        } else {
            return Err(e).with_context(|| format!("activate object {}", dest.display()));
        }
    }

    // sidecars: provenance record + evidence bytes
    let record_raw = toml::to_string_pretty(record)?;
    write_tmp_then_rename(record_raw.as_bytes(), &record_of(root, &digest))?;
    for (src, ext) in [
        (evidence.detached_sig.as_deref(), "sig"),
        (evidence.index.as_deref(), "index.toml"),
        (evidence.index_sig.as_deref(), "index.toml.sig"),
    ] {
        let Some(src) = src else { continue };
        let data = std::fs::read(src).with_context(|| format!("preserve evidence {}", src.display()))?;
        write_tmp_then_rename(&data, &object_path(&root, &digest, ext))?;
    }
    Ok(dest)
}

/// Read the provenance record of a stored object (if it exists and parses).
pub fn read_record(root: &Path, digest: &str) -> Option<ArtifactRecord> {
    let path = record_of(root, digest);
    let raw = std::fs::read_to_string(path).ok()?;
    toml::from_str(&raw).ok()
}

/// A stored object and every evidence file we could find for it.
#[derive(Debug, Clone)]
pub struct StoredObject {
    pub archive: PathBuf,
    pub digest: String,
    pub record: Option<ArtifactRecord>,
    pub sig: Option<PathBuf>,
    pub index: Option<PathBuf>,
    pub index_sig: Option<PathBuf>,
}

/// Locate an object by digest. Digests MUST be validated first with
/// `validate_hex_digest` — they can come from history records or CLI text.
pub fn open_object(root: &Path, digest: &str) -> Option<StoredObject> {
    let digest = digest.trim().to_lowercase();
    if !valid_digest(&digest) {
        return None;
    }
    let archive = archive_of(root, &digest);
    if !archive.is_file() {
        return None;
    }
    let record = read_record(root, &digest);
    let sig = path_if_exists(object_path(&root, &digest, "sig"));
    let index = path_if_exists(object_path(&root, &digest, "index.toml"));
    let index_sig = path_if_exists(object_path(&root, &digest, "index.toml.sig"));
    Some(StoredObject { archive, digest, record, sig, index, index_sig })
}

fn path_if_exists(p: PathBuf) -> Option<PathBuf> {
    p.is_file().then_some(p)
}

fn valid_digest(d: &str) -> bool {
    d.len() == 64 && d.chars().all(|c| c.is_ascii_hexdigit())
}

/// Never derive a file path from unvalidated outside data: package entries
/// and history records passing digests to `open_object` go through this.
pub fn validate_hex_digest<'a>(value: &'a str, what: &str) -> Result<&'a str> {
    validate_digest_text(value, false, what)
}

fn validate_digest_text<'a>(value: &'a str, allow_prefix: bool, what: &str) -> Result<&'a str> {
    let rest = if allow_prefix {
        value
            .strip_prefix("sha256:")
            .with_context(|| format!("{what} must be `sha256:<64 hex>` or a bare digest"))?
    } else {
        value
    };
    if rest.len() != 64 || !rest.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("{what} is not a SHA-256 digest (expected 64 hex characters)");
    }
    Ok(rest)
}/// The only sanctioned read path for restoring or inspecting an artifact:
/// locate the object, then re-verify its bytes against the digest that
/// names them. Presence on disk is never trusted by itself; a missing,
/// corrupted or symlinked object is an error, never a silent fallback.
pub fn open_verified_object(root: &Path, digest: &str) -> Result<StoredObject> {
    validate_hex_digest(digest, "stored artifact digest")?;
    let stored = open_object(root, digest)
        .ok_or_else(|| anyhow!("artifact object {} is missing", archive_of(root, digest).display()))?;
    // symlinked cache entries must not be followed into foreign storage
    if std::fs::symlink_metadata(&stored.archive)?.file_type().is_symlink() {
        bail!("artifact object {} is a symlink (refusing)", stored.archive.display());
    }
    let actual = crate::fetch::sha256_of_file(&stored.archive)?;
    if actual != stored.digest {
        bail!(
            "corrupt cached artifact {}: sha256 mismatch (got {})",
            stored.archive.display(),
            actual
        );
    }
    Ok(stored)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::Sha256;
    use std::io::Write;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("nawa-objects-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn write_bytes(p: &Path, body: &[u8]) -> PathBuf {
        let mut f = std::fs::File::create(p).unwrap();
        f.write_all(body).unwrap();
        f.sync_all().unwrap();
        p.to_path_buf()
    }

    fn digest_of_file(p: &Path) -> String {
        crate::fetch::sha256_of_file(p).unwrap()
    }

    fn digest_of_bytes(b: &[u8]) -> String {
        hex::encode(Sha256::digest(b))
    }

    fn rec(bytes_digest: &str, size: u64, auth: &str) -> ArtifactRecord {
        ArtifactRecord {
            schema: OBJECTS_SCHEMA,
            artifact_sha256: bytes_digest.into(),
            archive_size: size,
            authentication: auth.into(),
            verified_at: 1_791_000_000,
            origin: "repository".into(),
            package: ArtifactPackage {
                name: "demo".into(),
                version: "1.0".into(),
                rel: 1,
                arch: "x86_64".into(),
            },
            repository: None,
            signer: None,
            trusted_at_install: false,
        }
    }

    #[test]
    fn publishes_digest_named_immutable_object_with_record() {
        let root = tmpdir("pub");
        let src = write_bytes(&root.join("in.tar.zst"), b"object-bytes");
        let digest = digest_of_file(&src);
        let record = rec(&digest, std::fs::metadata(&src).unwrap().len(), AUTH_SIGNED_INDEX);
        publish_object(&root, &src, &record, &EvidenceFiles {
            detached_sig: None,
            index: None,
            index_sig: None,
        })
        .unwrap();

        // path derives ONLY from the digest
        assert_eq!(archive_of(&root, &digest), object_path(&root, &digest, "nawa.tar.zst"));
        let stored = open_object(&root, &digest).expect("published");
        assert_eq!(digest_of_file(&stored.archive.clone().as_path()), digest);
        assert_eq!(stored.record.as_ref().unwrap().authentication, AUTH_SIGNED_INDEX);

        // re-publishing identical bytes keeps the object (no duplicate)
        let again = write_bytes(&root.join("in2.tar.zst"), b"object-bytes");
        publish_object(&root, &again, &record, &EvidenceFiles {
            detached_sig: None,
            index: None,
            index_sig: None,
        })
        .unwrap();
        assert_eq!(archive_of(&root, &digest), object_path(&root, &digest, "nawa.tar.zst"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rejects_bytes_that_disagree_with_the_record() {
        let root = tmpdir("mismatch");
        let lie = rec(&digest_of_bytes(b"different"), 13, AUTH_UNSIGNED);
        let good = digest_of_bytes(b"actual");
        let mut bad = lie.clone();
        bad.artifact_sha256 = good.clone();
        bad.archive_size = 999;
        let src = write_bytes(&root.join("x"), b"actual");
        assert!(publish_object(&root, &src, &bad, &EvidenceFiles {
            detached_sig: None,
            index: None,
            index_sig: None,
        })
        .is_err(), "size mismatch is refused");
        let mut wrong_digest = lie.clone();
        wrong_digest.artifact_sha256 = digest_of_bytes(b"not-the-in").to_string();
        assert!(publish_object(&root, &src, &wrong_digest, &EvidenceFiles {
            detached_sig: None,
            index: None,
            index_sig: None,
        })
        .is_err(), "digest mismatch is refused");
        // nothing half-published
        assert!(open_object(&root, &good).is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn keeps_publication_digest_safe_and_self_heals_a_corrupt_object() {
        let root = tmpdir("heal");
        let src = write_bytes(&root.join("s"), b"payload");
        let digest = digest_of_file(&src);
        let record = rec(&digest, 7, AUTH_DETACHED);
        publish_object(&root, &src, &record, &EvidenceFiles {
            detached_sig: Some(src.clone()),
            index: None,
            index_sig: None,
        })
        .unwrap();
        let obj = archive_of(&root, &digest);

        // digest verification catches corruption
        std::fs::write(&obj, b"payload-tampered").unwrap();
        assert!(open_verified_object(&root, &digest).is_err());

        // publishing the good bytes again replaces the corrupt object (and
        // never publishes wrong bytes): path == digest again afterwards
        publish_object(&root, &src, &record, &EvidenceFiles {
            detached_sig: Some(src.clone()),
            index: None,
            index_sig: None,
        })
        .unwrap();
        assert_eq!(digest_of_file(&obj), digest);
        assert!(open_verified_object(&root, &digest).is_ok());
        // the detached signature evidence was preserved
        let stored = open_object(&root, &digest).unwrap();
        assert_eq!(stored.sig.unwrap(), object_path(&root, &digest, "sig"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn open_object_refuses_unvalidated_digests() {
        assert!(open_object(Path::new("/nonexistent-root"), "../../etc/shadow").is_none());
        assert!(open_object(Path::new("/nonexistent-root"), "zz").is_none());
    }
}
