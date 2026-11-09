//! Content-addressed cache, remote repository cache, and mirror handling.

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::fs::{File, OpenOptions};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use super::checksum::verify_sha256;
use super::http::{check_offline, download, offline};

pub fn digest_cache_dir(src_cache: &Path) -> PathBuf {
    src_cache.join("sha256")
}

/// Hold-open lock file: closing the descriptor (on drop or process exit)
/// releases the `flock`, the way `lock.rs` does it.
pub(crate) struct CacheLock {
    _file: File,
}

/// Take the advisory exclusive `flock` for one digest. Waiting (not
/// `try_*`): a concurrent build is *publishing* the entry we want, so
/// waiting is the fast path and a timeout cannot be reported by flock.
///
/// Shared with `fetchrun.rs` (§11): the ecosystem cache entries use the
/// same hold-open `flock` discipline on `<key>.lock`.
pub(crate) fn lock_digest_entry(path: &Path) -> Result<CacheLock> {
    use rustix::fs::{flock, FlockOperation};
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .read(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("open cache lock {}", path.display()))?;
    // flock(2) can return EINTR when a signal handler runs (nawa's own
    // interrupt handler); retry it like lock.rs does
    loop {
        match flock(&file, FlockOperation::LockExclusive) {
            Ok(()) => return Ok(CacheLock { _file: file }),
            Err(err) if err == rustix::io::Errno::INTR => continue,
            Err(err) => bail!("flock on the source cache lock failed: {err}"),
        }
    }
}

/// Unique transfer-side `.part` name (§11): unlike the legacy downloader —
/// which shares one `<file>.part` per destination — every new-form transfer
/// gets its own file name so two concurrent builds of the same digest can
/// never corrupt each other's partial bytes.
static PART_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub(crate) fn unique_part_path(dest: &Path) -> PathBuf {
    let seq = PART_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let base = dest
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "digest".to_string());
    dest.with_file_name(format!("{base}.{}-{seq}.part", std::process::id()))
}

/// A published cache entry must never be modified again (§11 "immutability"):
/// strip the write bits, best effort — the cache is re-verified before a hit
/// anyway, so a world-writable entry is a permissions smell, not a trust hole.
fn strip_write_permission(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::symlink_metadata(path).and_then(|md| {
        let mode = md.permissions().mode() & 0o777 & !0o222;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
    });
}

/// Download one new-schema source into the content-addressed cache at
/// `dest = <srccache>/sha256/<digest>` and publish it only after the bytes
/// verify (§10.1: a mirror is an alternate transport for identical bytes).
///
/// Cache-first, like [`download_cached_with_mirrors`]: a hit is re-verified,
/// so a truncated/corrupted entry is transparently re-downloaded. Around the
/// miss path:
///
///  * [`lock_digest_entry`] on `<dest>.lock` serialises builds racing for
///    the same digest, followed by a runner-up cache recheck — the other
///    build may have published the entry while we waited;
///  * the §11 legacy fallback: `<srccache>/<sha16>-<url-basename>` is
///    verified against the full digest and *copied* (not hard-linked) into
///    the new path, keeping the legacy entry untouched and shareable;
///  * the transfer lands in a unique `.part` sibling and is renamed
///    atomically after verification; the published entry is marked
///    read-only.
pub fn download_content_addressed(
    url: &str,
    mirrors: &[String],
    sha256: &str,
    dest: &Path,
) -> Result<()> {
    // the digest doubles as a path component — refuse anything but the
    // recipe-parse shape (defence in depth: validate() already enforces it)
    if !crate::recipe::is_lower_hex64(sha256) {
        bail!("new-form source sha256 must be 64 lowercase hex, got `{sha256}`");
    }
    if dest.exists() {
        if verify_sha256(dest, sha256).is_ok() {
            return Ok(());
        }
        crate::nawa_info_stderr!("cached {} failed sha256 — re-downloading", dest.display());
    }
    // offline with a cold cache: one clean error here beats one per mirror
    // candidate (mirrors [download_cached_with_mirrors] for message shape)
    check_offline(url, offline())?;
    let dir = dest
        .parent()
        .context("content-addressed cache entry needs a parent directory")?;
    std::fs::create_dir_all(dir)
        .with_context(|| format!("create digest cache {}", dir.display()))?;
    let _lock = lock_digest_entry(&dest.with_file_name(format!("{}.lock", sha256)))?;

    // runner-up cache hit: another build published this digest while we
    // were waiting on the lock
    if dest.exists() && verify_sha256(dest, sha256).is_ok() {
        return Ok(());
    }

    // §11 migration: a legacy `<sha16>-<file>` entry whose bytes verify is
    // adopted into the new layout
    if let Some(parent) = dir.parent() {
        let fname = url.rsplit('/').next().unwrap_or("source.bin");
        let legacy = parent.join(format!("{}-{}", &sha256[..16], fname));
        if legacy.is_file() {
            match verify_sha256(&legacy, sha256) {
                Ok(()) => {
                    // same publication discipline as a download: copy to a
                    // unique `.part` sibling, then rename. A direct copy onto
                    // `dest` is not atomic and fails with EACCES when a
                    // corrupt, read-only entry (0444) already sits there.
                    let part = unique_part_path(dest);
                    let migrated = std::fs::copy(&legacy, &part)
                        .and_then(|_| std::fs::rename(&part, dest));
                    if let Err(e) = migrated {
                        let _ = std::fs::remove_file(&part);
                        return Err(anyhow::Error::new(e).context(format!(
                            "migrate legacy cache entry {}",
                            legacy.display()
                        )));
                    }
                    strip_write_permission(dest);
                    crate::nawa_info!(
                        "source cached (migrated legacy entry {}): {}",
                        legacy.display(),
                        dest.display()
                    );
                    return Ok(());
                }
                Err(_) => {
                    crate::nawa_info_stderr!(
                        "legacy cache entry {} failed sha256 — downloading",
                        legacy.display()
                    );
                }
            }
        }
    }

    // url, then built-in GNU mirrors, then the recipe's — every candidate
    // is checked against the digest, so a mirror serving wrong bytes lets
    // the next one speak
    let tmp = unique_part_path(dest);
    try_candidates(url, mirrors, &tmp, |tmp| verify_sha256(tmp, sha256))?;
    std::fs::rename(&tmp, dest)
        .with_context(|| format!("activate cached download {}", dest.display()))?;
    strip_write_permission(dest);
    Ok(())
}

// ---- package repositories ----------------------------------------------------

/// A package repository location: a local directory or an http(s) URL.
///
/// Remote repositories are materialized into a per-URL cache directory under
/// `<root>/var/cache/nawa/remote/` (index fetched fresh, archives cached and
/// sha256-verified), after which the normal upgrade path runs unchanged.
#[derive(Debug, Clone, PartialEq, Eq)]

// ---- package repositories ----------------------------------------------------
pub enum RepoRef {
    Local(PathBuf),
    /// Base URL without a trailing slash, e.g. `https://mirror/stable`
    Remote(String),
}

impl RepoRef {
    pub fn parse(s: &str) -> Result<Self> {
        let s = s.trim();
        if s.starts_with("http://") || s.starts_with("https://") {
            Ok(RepoRef::Remote(s.trim_end_matches('/').to_string()))
        } else if s.contains("://") {
            bail!("unsupported repository `{s}` — use http(s):// or a local path");
        } else {
            Ok(RepoRef::Local(PathBuf::from(s)))
        }
    }
}

/// Stable per-URL cache directory: `<root>/var/cache/nawa/remote/<key>`.
/// The key is the first 12 hex chars of sha256(base_url), so the same
/// repository always maps to the same cache and different URLs never clash.
pub fn remote_cache_dir(root: &Path, base_url: &str) -> PathBuf {
    let mut h = Sha256::new();
    h.update(base_url.as_bytes());
    let key = hex::encode(&h.finalize()[..6]);
    root.join("var/cache/nawa/remote").join(key)
}

// ---- mirrors ------------------------------------------------------------------

/// The URL itself, then equivalent mirrors for well-known GNU hosts, so one
/// unreachable mirror does not fail the whole build. Content is always
/// checked against the recipe's pinned sha256, so mirrors need no trust.
pub fn candidate_urls(url: &str) -> Vec<String> {
    let mut v = vec![url.to_string()];
    for prefix in [
        "https://mirrors.kernel.org/gnu/",
        "https://ftp.gnu.org/gnu/",
        "https://ftpmirror.gnu.org/",
        "https://mirror.us-midwest-1.nexcess.net/gnu/",
        "https://mirrors.ocf.berkeley.edu/gnu/",
    ] {
        if let Some(rest) = url.strip_prefix(prefix) {
            for alt in [
                "https://ftpmirror.gnu.org/",
                "https://ftp.gnu.org/gnu/",
                "https://mirrors.kernel.org/gnu/",
                "https://mirrors.ocf.berkeley.edu/gnu/",
            ] {
                let u = format!("{alt}{rest}");
                if !v.contains(&u) {
                    v.push(u);
                }
            }
            break;
        }
    }
    v
}

/// [`candidate_urls`] plus the recipe's own `mirrors`, appended in order
/// (duplicates dropped). Built-in GNU fallbacks come first, then the recipe's.
pub fn candidate_urls_with(url: &str, extra: &[String]) -> Vec<String> {
    let mut v = candidate_urls(url);
    for m in extra {
        if !v.contains(m) {
            v.push(m.clone());
        }
    }
    v
}

/// Download `url` (then every candidate mirror) into `tmp`, running
/// `verify` on each transfer. Shared by the legacy and
/// content-addressed paths so mirror/verify policy (log, tmp
/// cleanup, last error wins) cannot drift. On success `tmp` holds
/// the verified bytes; the caller renames it into place.
fn try_candidates(
    url: &str,
    mirrors: &[String],
    tmp: &Path,
    verify: impl Fn(&Path) -> Result<()>,
) -> Result<()> {
    let mut last_err = None;
    for cand in candidate_urls_with(url, mirrors) {
        if cand != url {
            crate::nawa_info_stderr!("trying alternate mirror {cand}");
        }
        let res = download(&cand, tmp).and_then(|_| verify(tmp));
        match res {
            Ok(()) => {
                last_err = None;
                break;
            }
            Err(e) => {
                crate::nawa_info_stderr!("{e}");
                let _ = std::fs::remove_file(tmp);
                last_err = Some(e);
            }
        }
    }
    if let Some(e) = last_err {
        return Err(e);
    }
    Ok(())
}

/// Download `url` into `dest` unless a copy is already cached.
/// With `expect_sha256`, a cached copy is reused only after re-verifying it,
/// so a corrupted or truncated cache entry is transparently re-downloaded.
/// The transfer lands in a `.part` sibling first and is renamed atomically.
pub fn download_cached(url: &str, dest: &Path, expect_sha256: Option<&str>) -> Result<()> {
    download_cached_with_mirrors(url, &[], dest, expect_sha256)
}

/// Like [`download_cached`], but after `url` (and the built-in GNU mirrors)
/// fails it also tries `mirrors` — the recipe's `package.mirrors`. Every
/// candidate is checked against `expect_sha256`, so a mirror serving the
/// wrong bytes is rejected and the next one is tried.
pub fn download_cached_with_mirrors(
    url: &str,
    mirrors: &[String],
    dest: &Path,
    expect_sha256: Option<&str>,
) -> Result<()> {
    if dest.exists() {
        match expect_sha256 {
            Some(expected) => {
                if verify_sha256(dest, expected).is_ok() {
                    return Ok(()); // cache hit
                }
                crate::nawa_info_stderr!("cached {} failed sha256 — re-downloading", dest.display());
            }
            None => return Ok(()), // unpinned cache hit (index: signature checked separately)
        }
    }
    // offline with a cold cache: one clean error here beats one per
    // mirror candidate (the download/download_opt guards would fire for
    // each candidate and the last error would name a fallback mirror)
    check_offline(url, offline())?;
    let tmp = unique_part_path(dest);
    try_candidates(url, mirrors, &tmp, |tmp| match expect_sha256 {
        Some(e) => verify_sha256(tmp, e),
        None => Ok(()),
    })?;
    std::fs::rename(&tmp, dest)
        .with_context(|| format!("activate cached download {}", dest.display()))?;
    Ok(())
}

// ---- source tarballs (native extraction, replaces the external `tar`) --------
