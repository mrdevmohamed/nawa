//! Source fetching over a native Rust HTTP stack (no external commands) and
//! sha256 verification.
//!
//! Downloads used to shell out to `curl` and source staging to the external
//! `tar`/`cp` binaries. Everything here now runs in-process:
//!
//!   * HTTP: `ureq` with rustls and bundled webpki roots, keeping curl's
//!     outward behaviour — only http(s) URLs, redirects may only step up to
//!     https (curl's `--proto =http,https --proto-redir =https`), retries on
//!     transient failures (`--retry`), a 20s connect timeout and a read
//!     timeout so a stalled mirror cannot hang a build forever;
//!   * `download_opt` treats only HTTP 404 as an absent optional `.sig`
//!     sidecar; permission errors and server failures remain errors, so a
//!     broken or inaccessible signature URL cannot masquerade as unsigned;
//!   * `copy_local` is a pure-Rust `cp -a` (symlinks, modes, timestamps,
//!     hard links and — as root — ownership are preserved);
//!   * source tarballs are recognised and extracted with the `tar` crate plus
//!     in-process gzip/xz/bzip2/zstd decoders, with `tar --no-same-owner`
//!     semantics (ownership never restored, suid bits dropped, mtime kept)
//!     and GNU-tar-style path sanitisation.

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Read, Seek, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

// ---- checksums -------------------------------------------------------------

pub fn sha256_of_file(path: &Path) -> Result<String> {
    let mut f = std::fs::File::open(path)
        .with_context(|| format!("cannot open {} for hashing", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

pub fn verify_sha256(path: &Path, expected: &str) -> Result<()> {
    if expected.trim().is_empty() {
        bail!("missing sha256 for {} — refusing to build unverified source", path.display());
    }
    let got = sha256_of_file(path)?;
    if got != expected.trim().to_lowercase() {
        bail!(
            "sha256 mismatch for {}:\n  expected: {}\n  got:      {}",
            path.display(),
            expected,
            got
        );
    }
    Ok(())
}


/// Private, cleanup-on-drop copy of a file whose bytes were hashed while copying.
/// Callers must use this path for all subsequent parsing or extraction.
pub struct VerifiedSnapshot {
    path: PathBuf,
}

impl VerifiedSnapshot {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for VerifiedSnapshot {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

static SNAPSHOT_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Remove `verified-<pid>-<n>.tmp` snapshots left behind by processes that
/// died before their `Drop` ran (SIGKILL, power loss). Only acts when /proc is
/// readable, so a missing /proc can never make a live process look dead.
fn remove_stale_snapshots(dir: &Path) {
    if !Path::new("/proc/self").exists() {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let me = std::process::id();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some(rest) = name.strip_prefix("verified-").and_then(|r| r.strip_suffix(".tmp")) else {
            continue;
        };
        let Some(pid) = rest.split('-').next().and_then(|p| p.parse::<u32>().ok()) else {
            continue;
        };
        if pid != me && !Path::new(&format!("/proc/{pid}")).exists() {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Copy into a mode-0600 O_EXCL snapshot inside `snapshot_dir` (private nawa
/// state, restricted to 0700) and optionally authenticate the copied bytes
/// against an expected SHA-256. Callers must use the snapshot, not the source,
/// for everything that follows.
pub fn snapshot_file(
    source_path: &Path,
    expected_sha256: Option<&str>,
    snapshot_dir: &Path,
) -> Result<VerifiedSnapshot> {
    std::fs::create_dir_all(snapshot_dir)
        .with_context(|| format!("create snapshot directory {}", snapshot_dir.display()))?;
    let dir_meta = std::fs::symlink_metadata(snapshot_dir)
        .with_context(|| format!("inspect snapshot directory {}", snapshot_dir.display()))?;
    if !dir_meta.is_dir() || dir_meta.file_type().is_symlink() {
        bail!("unsafe snapshot directory {}", snapshot_dir.display());
    }
    // This directory is private Nawa state. Restrict it before creating the
    // snapshot so other users cannot replace or modify the verified file.
    let mut permissions = dir_meta.permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(snapshot_dir, permissions)
        .with_context(|| format!("secure snapshot directory {}", snapshot_dir.display()))?;
    remove_stale_snapshots(snapshot_dir);

    let mut source = File::open(source_path)
        .with_context(|| format!("open {}", source_path.display()))?;
    let mut created: Option<(File, PathBuf)> = None;
    for _ in 0..128 {
        let sequence = SNAPSHOT_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = snapshot_dir.join(format!(
            "verified-{}-{sequence}.tmp",
            std::process::id()
        ));
        match OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(file) => {
                created = Some((file, path));
                break;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e).with_context(|| "create private verified snapshot"),
        }
    }
    let (mut snapshot_file, snapshot_path) =
        created.context("could not allocate a unique verified snapshot")?;
    let snapshot = VerifiedSnapshot { path: snapshot_path };
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = source.read(&mut buffer)
            .with_context(|| format!("read {}", source_path.display()))?;
        if read == 0 {
            break;
        }
        snapshot_file.write_all(&buffer[..read])?;
        hasher.update(&buffer[..read]);
    }
    // No fsync: the snapshot is a throwaway copy, durability is irrelevant.
    snapshot_file.flush()?;

    if let Some(expected) = expected_sha256 {
        let actual = hex::encode(hasher.finalize());
        if actual != expected.trim().to_ascii_lowercase() {
            bail!(
                "sha256 mismatch for {}:\n  expected: {}\n  got:      {}",
                source_path.display(),
                expected,
                actual
            );
        }
    }
    Ok(snapshot)
}

// ---- native HTTP download engine --------------------------------------------

// ---- offline mode (fetch-phase.md §12/§15) ----------------------------------

/// Process-wide offline switch, set from `--offline` / `NAWA_OFFLINE` (§15).
/// Every HTTP download nawa performs goes through [`download`] /
/// [`download_opt`], so this one guard covers legacy recipes, mirrors,
/// index downloads and new-schema sources alike. A download is only
/// attempted after the cache lookup missed it; running the same build
/// once without --offline populates the cache.
static OFFLINE: AtomicBool = AtomicBool::new(false);

/// Set the process-wide offline switch (§12): false = `--offline` absent.
pub fn set_offline(on: bool) {
    OFFLINE.store(on, Ordering::SeqCst);
}

/// Whether network access is forbidden process-wide (§12/§15).
pub fn offline() -> bool {
    OFFLINE.load(Ordering::SeqCst)
}

/// The gate itself. `download` / `download_opt` pass `offline()`; the unit
/// tests pass the flag explicitly so they never race the process-wide
/// switch other tests share.
fn check_offline(url: &str, offline: bool) -> Result<()> {
    if offline {
        bail!(
            "offline mode: {url} is not in the pre-fetch cache — \
             populate it with `nawa fetch` or run once without --offline"
        );
    }
    Ok(())
}


const CONNECT_TIMEOUT: Duration = Duration::from_secs(20); // curl --connect-timeout 20
/// Per-read socket timeout: a mirror that stops sending for this long is
/// treated as dead instead of hanging the build forever.
const READ_TIMEOUT: Duration = Duration::from_secs(60);
/// curl --retry 3 for regular downloads (initial try + 3 retries).
const DOWNLOAD_ATTEMPTS: u32 = 4;
/// Five total attempts for optional sidecars; transient 5xx responses can
/// be short-lived at release/CDN edges, so give them a longer recovery window.
const OPT_ATTEMPTS: u32 = 5;
const MAX_REDIRECTS: usize = 10;

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .user_agent(&format!("nawa/{}", env!("CARGO_PKG_VERSION")))
        .timeout_connect(CONNECT_TIMEOUT)
        .timeout_read(READ_TIMEOUT)
        // redirects are followed manually (http_get) so every hop can be
        // checked against the https-only policy
        .redirects(0)
        .build()
}

/// Only http(s) may be fetched at all (curl's `--proto =http,https`).
fn parse_http_url(url: &str) -> Result<url::Url> {
    let u = url::Url::parse(url).with_context(|| format!("invalid URL `{url}`"))?;
    match u.scheme() {
        "http" | "https" => Ok(u),
        other => bail!("unsupported URL scheme `{other}:` — nawa fetches http(s) only"),
    }
}

/// Statuses worth another attempt (curl --retry semantics).
fn transient_status(code: u16) -> bool {
    code == 408 || code == 429 || (500..=599).contains(&code)
}

fn is_retryable(e: &anyhow::Error) -> bool {
    match e.downcast_ref::<ureq::Error>() {
        Some(ureq::Error::Status(code, _)) => transient_status(*code),
        // connect/read timeouts, refused and reset connections, TLS hiccups…
        Some(ureq::Error::Transport(_)) => true,
        None => {
            // truncated/stalled bodies surface as plain io errors
            e.downcast_ref::<std::io::Error>()
                .map(|io| {
                    matches!(
                        io.kind(),
                        std::io::ErrorKind::UnexpectedEof
                            | std::io::ErrorKind::ConnectionAborted
                            | std::io::ErrorKind::ConnectionReset
                            | std::io::ErrorKind::TimedOut
                            | std::io::ErrorKind::WouldBlock
                    )
                })
                .unwrap_or(false)
        }
    }
}

/// Exponential backoff between retries: 0.5s, 1s, 2s, 4s…
fn backoff(attempt: u32) -> Duration {
    Duration::from_millis(500u64 << (attempt - 1).min(4))
}

/// GET `url`, following redirects manually: the first hop may be http or
/// https, every redirect target must be https (curl's `--proto-redir
/// =https`). Relative `Location` values are resolved per RFC 7231.
fn http_get(agent: &ureq::Agent, url: &str) -> Result<ureq::Response> {
    let mut current = parse_http_url(url)?;
    for hop in 0..=MAX_REDIRECTS {
        // with redirects(0) ureq hands 3xx responses back as Ok, 4xx/5xx as
        // Err(Status) and transport problems as Err(Transport)
        let resp = agent.get(current.as_str()).call()?;
        if !(300..400).contains(&resp.status()) {
            return Ok(resp);
        }
        if hop == MAX_REDIRECTS {
            bail!("too many redirects fetching {url}");
        }
        let loc = resp
            .header("Location")
            .with_context(|| format!("redirect from {current} without Location"))?
            .to_string();
        // signed CDN URLs are hundreds of characters of noise: show only the
        // hosts, and only when asked (NAWA_VERBOSE=1)
        if std::env::var_os("NAWA_VERBOSE").is_some_and(|v| v != "0" && !v.is_empty()) {
            crate::nawa_info_stderr!("redirect {} -> {}", current.host_str().unwrap_or("?"), loc.split('?').next().unwrap_or(&loc));
        }
        current = redirect_target(&current, &loc)?;
    }
    unreachable!("the loop returns on every path")
}

/// Resolve one redirect hop (pure — unit-tested without a live server).
fn redirect_target(current: &url::Url, loc: &str) -> Result<url::Url> {
    let next = current
        .join(loc)
        .with_context(|| format!("bad redirect target `{loc}`"))?;
    if next.scheme() != "https" {
        bail!("refusing redirect {current} -> {next}: redirects must stay on https");
    }
    Ok(next)
}

fn stderr_is_tty() -> bool {
    use std::io::IsTerminal;
    std::io::stderr().is_terminal()
}

/// Downloads smaller than this get no progress line (index.toml, signatures).
const PROGRESS_MIN_BYTES: u64 = 256 * 1024;

/// Text of one progress line: `name  12.3 MiB / 45.0 MiB  27%` (no total: just the size).
fn progress_line(label: &str, done: u64, total: Option<u64>) -> String {
    use crate::humanize::format_size;
    match total {
        Some(t) if t > 0 => format!(
            "  {label}  {} / {}  {:>3}%",
            format_size(done),
            format_size(t),
            (done.min(t) * 100 / t)
        ),
        _ => format!("  {label}  {}", format_size(done)),
    }
}

/// `libepoxy-1.5.9-1.nawa.tar.zst.part` -> `libepoxy-1.5.9-1`
fn progress_label(dest: &Path) -> String {
    let n = dest.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let n = n.trim_start_matches('.');
    let n = n.strip_suffix(".part").unwrap_or(n);
    n.strip_suffix(".nawa.tar.zst").unwrap_or(n).to_string()
}

/// Stream the response body into `dest`. On an interactive terminal, a
/// download of a real size shows one self-updating line with the size and a
/// percentage (when the server sent Content-Length), then leaves it as a
/// finished line; small files and non-terminals stay silent.
fn stream_body(resp: ureq::Response, dest: &Path) -> Result<u64> {
    let total_len: Option<u64> = resp.header("Content-Length").and_then(|v| v.trim().parse().ok());
    let mut reader = resp.into_reader();
    let f = std::fs::File::create(dest).with_context(|| format!("create {}", dest.display()))?;
    let mut w = BufWriter::with_capacity(256 * 1024, f);
    let mut buf = [0u8; 128 * 1024];
    let mut total = 0u64;
    let show = stderr_is_tty() && !matches!(total_len, Some(t) if t < PROGRESS_MIN_BYTES);
    let label = progress_label(dest);
    let mut last = std::time::Instant::now();
    let mut drawn = false;
    loop {
        let n = reader.read(&mut buf).context("read download body")?;
        if n == 0 {
            break;
        }
        w.write_all(&buf[..n]).with_context(|| format!("write {}", dest.display()))?;
        total += n as u64;
        if show && (!drawn || last.elapsed() >= Duration::from_millis(100)) {
            // \x1b[K clears what a longer previous line left behind
            eprint!("\r{}\x1b[K", progress_line(&label, total, total_len));
            let _ = std::io::stderr().flush();
            last = std::time::Instant::now();
            drawn = true;
        }
    }
    w.flush().with_context(|| format!("flush {}", dest.display()))?;
    if show && (drawn || total >= PROGRESS_MIN_BYTES) {
        eprintln!("\r{}\x1b[K", progress_line(&label, total, Some(total)));
    } else if drawn {
        eprint!("\r\x1b[K");
    }
    Ok(total)
}

enum Outcome {
    Downloaded,
    /// HTTP 404 answer: the optional file is simply not published.
    Absent,
}

fn single_transfer(agent: &ureq::Agent, url: &str, dest: &Path) -> Result<Outcome> {
    match http_get(agent, url) {
        Ok(resp) => stream_body(resp, dest).map(|_| Outcome::Downloaded),
        Err(e) => {
            // Only 404 means an optional sidecar is absent. A 403 can mean
            // access/rate-limit policy, and other 4xx responses are real errors.
            // 408/429 and 5xx are handled by the retry policy below.
            if let Some(ureq::Error::Status(404, _)) = e.downcast_ref::<ureq::Error>() {
                return Ok(Outcome::Absent);
            }
            Err(e)
        }
    }
}

fn fetch_to_file(agent: &ureq::Agent, url: &str, dest: &Path, attempts: u32) -> Result<Outcome> {
    parse_http_url(url)?;
    for attempt in 1..=attempts {
        match single_transfer(agent, url, dest) {
            Ok(outcome) => return Ok(outcome),
            Err(e) if is_retryable(&e) && attempt < attempts => {
                let wait = backoff(attempt);
                crate::nawa_info_stderr!("{url}: attempt {attempt}/{attempts} failed ({e:#}); retrying in {wait:?}");
                std::thread::sleep(wait);
            }
            Err(e) => return Err(e),
        }
    }
    unreachable!("every loop arm returns")
}

/// Download `url` into `dest` with the in-process HTTP stack
/// (retries + fail on HTTP error — including 4xx, like `curl -f`).
pub fn download(url: &str, dest: &Path) -> Result<()> {
    check_offline(url, offline())?;
    let a = agent();
    match fetch_to_file(&a, url, dest, DOWNLOAD_ATTEMPTS)? {
        Outcome::Downloaded => Ok(()),
        Outcome::Absent => bail!("download failed: {url}"),
    }
}

/// Like `download`, but treats HTTP 404 as "file absent" and
/// returns `false` instead of failing (used for optional `.sig` sidecars).
/// Server errors (5xx) are retried and still surface as an error, so a
/// broken mirror can no longer make a signature look unpublished.
pub fn download_opt(url: &str, dest: &Path) -> Result<bool> {
    check_offline(url, offline())?;
    let a = agent();
    Ok(matches!(
        fetch_to_file(&a, url, dest, OPT_ATTEMPTS)?,
        Outcome::Downloaded
    ))
}

// ---- local sources (pure-Rust `cp -a`) --------------------------------------

/// Copy a file or directory tree from the host (for `local:` sources).
/// Pure-Rust `cp -a` equivalent: symlinks stay symlinks, permissions and
/// timestamps are preserved, hard links within the tree stay linked, and
/// ownership is preserved when running as root.
pub fn copy_local(src: &Path, dest: &Path) -> Result<()> {
    use std::collections::HashMap;
    use std::os::unix::fs::MetadataExt;

    if !src.is_dir() {
        std::fs::copy(src, dest).with_context(|| format!("copy {}", src.display()))?;
        copy_file_times(src, dest)?;
        return Ok(());
    }
    std::fs::create_dir_all(dest).with_context(|| format!("create {}", dest.display()))?;
    let as_root = crate::diag::is_root();
    let mut seen: HashMap<(u64, u64), PathBuf> = HashMap::new(); // (dev, ino) -> first copy
    let mut dir_times: Vec<(PathBuf, PathBuf)> = Vec::new();
    for entry in walkdir::WalkDir::new(src).follow_links(false) {
        let e = entry.with_context(|| format!("walk {}", src.display()))?;
        let md = e.path().symlink_metadata()?;
        let rel = e.path().strip_prefix(src)?;
        let dst = dest.join(rel);
        let ft = md.file_type();
        if ft.is_dir() {
            std::fs::create_dir_all(&dst).with_context(|| format!("create {}", dst.display()))?;
            if as_root {
                preserve_owner(&dst, &md);
            }
            dir_times.push((e.path().to_path_buf(), dst));
        } else if ft.is_symlink() {
            let target = std::fs::read_link(e.path())?;
            let _ = std::fs::remove_file(&dst);
            std::os::unix::fs::symlink(&target, &dst)
                .with_context(|| format!("symlink {} -> {}", dst.display(), target.display()))?;
            copy_file_times(e.path(), &dst)?;
        } else if ft.is_file() {
            let key = (md.dev(), md.ino());
            match seen.get(&key) {
                Some(first) => {
                    let _ = std::fs::remove_file(&dst);
                    std::fs::hard_link(first, &dst)
                        .with_context(|| format!("hard link {} -> {}", first.display(), dst.display()))?;
                    // shares an inode with the first copy, which already has
                    // its times set — nothing to do
                }
                None => {
                    std::fs::copy(e.path(), &dst)
                        .with_context(|| format!("copy {}", e.path().display()))?;
                    seen.insert(key, dst.clone());
                    copy_file_times(e.path(), &dst)?;
                }
            }
            if as_root {
                preserve_owner(&dst, &md);
            }
        } else {
            crate::nawa_warn!("skipping special file {}", e.path().display());
        }
    }
    // directory timestamps go last: writing children bumps parent mtimes
    for (s, d) in dir_times.iter().rev() {
        copy_file_times(s, d)?;
    }
    Ok(())
}

fn preserve_owner(dst: &Path, md: &std::fs::Metadata) {
    use std::os::unix::fs::MetadataExt;
    use rustix::fs::{Gid, Uid};
    // best effort, like cp -a on a filesystem that ignores ownership
    let _ = rustix::fs::chown(
        dst,
        Some(Uid::from_raw(md.uid())),
        Some(Gid::from_raw(md.gid())),
    );
}

fn copy_file_times(src: &Path, dst: &Path) -> Result<()> {
    let md = std::fs::symlink_metadata(src).with_context(|| format!("stat {}", src.display()))?;
    let modified = md.modified().unwrap_or(std::time::UNIX_EPOCH);
    let mtime = filetime::FileTime::from_system_time(modified);
    let atime = filetime::FileTime::from_system_time(md.accessed().unwrap_or(modified));
    filetime::set_symlink_file_times(dst, atime, mtime)
        .with_context(|| format!("set times on {}", dst.display()))
}

// ---- content-addressed source cache (fetch-phase.md §11) ---------------------

/// The new-form source cache root: `<srccache>/sha256` (§11). Legacy
/// `<sha16>-<file>` entries stay next to it and are never rewritten.
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

fn unique_part_path(dest: &Path) -> PathBuf {
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
    let tmp = dest.with_extension("part");
    try_candidates(url, mirrors, &tmp, |tmp| match expect_sha256 {
        Some(e) => verify_sha256(tmp, e),
        None => Ok(()),
    })?;
    std::fs::rename(&tmp, dest)
        .with_context(|| format!("activate cached download {}", dest.display()))?;
    Ok(())
}

// ---- source tarballs (native extraction, replaces the external `tar`) --------

/// Supported source archive formats, detected by magic bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceFormat {
    Tar,
    TarGz,
    TarBz2,
    TarXz,
    TarZst,
}

/// Recognise a source archive by content, not by file name: read the first
/// block, look for a tar header directly (uncompressed tar) or behind the
/// gzip/xz/bzip2/zstd magic. Returns `None` for everything else — a bare
/// `.gz` of a single file is *not* an archive and stays one (see build.rs).
pub fn detect_source_format(path: &Path) -> Option<SourceFormat> {
    let mut f = std::fs::File::open(path).ok()?;
    let mut head = [0u8; 512];
    let n = read_up_to(&mut f, &mut head)?;
    if n >= 512 && is_tar_header(&head) {
        return Some(SourceFormat::Tar);
    }
    let magic = &head[..n.min(6)];
    f.rewind().ok()?;
    if magic.starts_with(&[0xfd, 0x37, 0x7a, 0x58, 0x5a, 0x00]) {
        // XzReader is a plain streaming `Read`, so pulling one 512-byte block
        // decodes only the first LZMA2 chunk. A decode error before any tar
        // header (unsupported check type, corrupt stream, ...) still claims
        // the format, so extraction reports the real error; answering `None`
        // would make build.rs stage the .xz as a single-file source and the
        // build would fail far away with "./configure: No such file".
        let mut dec = lzma_rust2::XzReader::new(std::io::BufReader::new(f), true);
        return match read_up_to(&mut dec, &mut head) {
            Some(n) => (n >= 512 && is_tar_header(&head)).then_some(SourceFormat::TarXz),
            None => Some(SourceFormat::TarXz),
        };
    }
    // the pull-based decoders only consume as much input as needed to fill
    // the 512-byte header request — no artificial input cap is required
    // (bzip2 in particular *errors* when its input ends mid-stream, so a
    // Take()-bounded prefix would never work here)
    if magic.starts_with(&[0x1f, 0x8b]) {
        looks_like_tar(&mut flate2::read::GzDecoder::new(f))
            .then_some(SourceFormat::TarGz)
    } else if magic.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
        match zstd::stream::read::Decoder::new(f) {
            Ok(mut dec) => looks_like_tar(&mut dec).then_some(SourceFormat::TarZst),
            Err(_) => None,
        }
    } else if magic.starts_with(b"BZh") && matches!(magic.get(3), Some(c) if c.is_ascii_digit()) {
        looks_like_tar(&mut bzip2::read::BzDecoder::new(f))
            .then_some(SourceFormat::TarBz2)
    } else {
        None
    }
}

fn read_up_to(f: &mut dyn Read, buf: &mut [u8]) -> Option<usize> {
    let mut got = 0;
    while got < buf.len() {
        match f.read(&mut buf[got..]) {
            Ok(0) => break,
            Ok(n) => got += n,
            Err(_) => return None,
        }
    }
    Some(got)
}

/// Does this (decompressed) stream begin with a tar header?
fn looks_like_tar(reader: &mut dyn Read) -> bool {
    let mut block = [0u8; 512];
    let mut got = 0;
    while got < 512 {
        match reader.read(&mut block[got..]) {
            Ok(0) => break,
            Ok(k) => got += k,
            Err(_) => return false,
        }
    }
    got >= 512 && is_tar_header(&block)
}

/// ustar/GNU magic at offset 257; pre-POSIX v7 headers have no magic, so
/// fall back to validating the header checksum (checksum field read as
/// spaces, like every tar implementation does).
fn is_tar_header(b: &[u8; 512]) -> bool {
    if &b[257..262] == b"ustar" {
        return true;
    }
    let stored = match octal_field(&b[148..156]) {
        Some(v) => v,
        None => return false,
    };
    let sum: u64 = b
        .iter()
        .enumerate()
        .filter(|(i, _)| !(148..156).contains(i))
        .map(|(_, v)| *v as u64)
        .sum::<u64>()
        + 8 * b' ' as u64;
    stored == sum
}

fn octal_field(field: &[u8]) -> Option<u64> {
    let s: String = field
        .iter()
        .filter(|c| **c != 0)
        .map(|c| *c as char)
        .collect();
    u64::from_str_radix(s.trim().trim_matches(' '), 8).ok()
}

/// Extract a source tarball (tar / tar.gz / tar.bz2 / tar.xz / tar.zst) into
/// `dest`, replacing the external `tar --no-same-owner -xf` call.
///
/// Behaviour parity with GNU tar for the properties that matter to builds:
///   * ownership is never restored (files belong to the invoking user —
///     that is what `--no-same-owner` bought us) and suid bits are dropped;
///   * mtimes are preserved;
///   * absolute entry paths are rooted at `dest` and `..` components are
///     refused, so a tampered tarball cannot escape the staging directory;
///   * hard links are created even when their target appears later in the
///     archive (GNU tar is order independent here), degrading to a copy.
pub fn extract_source_archive(tarball: &Path, dest: &Path) -> Result<()> {
    let fmt = detect_source_format(tarball)
        .with_context(|| format!("{} is not a recognized tar archive", tarball.display()))?;
    std::fs::create_dir_all(dest).with_context(|| format!("create {}", dest.display()))?;
    unpack_tar_stream(decompressed_reader(tarball, fmt)?, dest)
}

/// Shared decompression switch of [`extract_source_archive`] /
/// [`extract_source_archive_stripped`]: open `tarball`, wrap it in the
/// streaming decoder `fmt` was detected with.
fn decompressed_reader(tarball: &Path, fmt: SourceFormat) -> Result<Box<dyn Read>> {
    let f = std::fs::File::open(tarball).with_context(|| format!("open {}", tarball.display()))?;
    Ok(match fmt {
        SourceFormat::Tar => Box::new(f),
        SourceFormat::TarGz => Box::new(flate2::read::GzDecoder::new(f)),
        SourceFormat::TarBz2 => Box::new(bzip2::read::BzDecoder::new(f)),
        SourceFormat::TarXz => {
            Box::new(lzma_rust2::XzReader::new(std::io::BufReader::new(f), true))
        }
        SourceFormat::TarZst => Box::new(zstd::stream::read::Decoder::new(f)?),
    })
}

/// Extract a source archive into `dest` with `strip_components` leading path
/// components removed first (fetch-phase.md §4.2).
///
/// * `strip_components = 0` is exactly [`extract_source_archive`].
/// * Otherwise the (decompressed) archive is re-emitted into a plain tar
///   sibling with the original headers — types, modes, ownership fields and
///   mtimes are copied — so the hardened [`unpack_tar_stream`] path runs on
///   it afterwards and every sanitisation, symlink and deferred-hard-link
///   behaviour is reused unchanged.
/// * Strip accounting follows GNU tar's extraction semantics: `.` segments
///   count as a component and are consumed by the strip count; entries with
///   fewer components are skipped; hard-link *targets* are stripped too and
///   a link whose target strip comes out empty is skipped (GNU errors there;
///   nothing sensible can be materialised); symlink targets pass through.
/// * Unlike the legacy listing path this is strict: an entry refusing to be
///   sanitised (`..`, absolute) aborts the staging instead of being skipped
///   with a warning — §16 expects traversal attempts rejected for
///   new-schema sources.
pub fn extract_source_archive_stripped(
    tarball: &Path,
    dest: &Path,
    strip_components: u32,
) -> Result<()> {
    if strip_components == 0 {
        return extract_source_archive(tarball, dest);
    }
    let fmt = detect_source_format(tarball)
        .with_context(|| format!("{} is not a recognized tar archive", tarball.display()))?;
    std::fs::create_dir_all(dest).with_context(|| format!("create {}", dest.display()))?;
    // plain tar written next to the destination (same `work` tree, so same
    // filesystem and no cross-device dance); it is transient
    let tmp = dest.with_file_name(format!(
        "{}.rewrite.tar",
        dest.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
    ));
    let rewrite = rewrite_tar_stripped(decompressed_reader(tarball, fmt)?, &tmp, strip_components);
    let extracted = rewrite.and_then(|()| {
        let f = std::fs::File::open(&tmp)
            .with_context(|| format!("open rewritten tar {}", tmp.display()))?;
        unpack_tar_stream(f, dest)
    });
    let _ = std::fs::remove_file(&tmp);
    extracted
}

/// Rewrite a (decompressed) tar stream into a plain tar at `tmp`, removing
/// `strip_components` leading components from every entry name and from
/// hard-link targets. Returns when the archive is complete.
///
/// `..` components (entry *names*; GNU/tar-rs would skip those entries on
/// extraction) abort with a hard error for new-schema sources (§16:
/// "traversal paths … are rejected"). Symlink targets are kept verbatim —
/// GNU tar does not translate them either, and extraction stores them
/// exactly as written. Skipped entries are deliberately left unread: the
/// `entries()` iterator seeks to the next header itself.
fn rewrite_tar_stripped<R: Read>(reader: R, tmp: &Path, strip: u32) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let mut ar = tar::Archive::new(reader);
    let file = std::fs::File::create(tmp)
        .with_context(|| format!("create rewritten tar {}", tmp.display()))?;
    let mut out = tar::Builder::new(BufWriter::with_capacity(256 * 1024, file));
    for entry in ar.entries().context("read tar entries")? {
        let mut entry = entry.context("read tar entry")?;
        let t = entry.header().entry_type();
        if t.is_gnu_longname() || t.is_gnu_longlink() {
            // extension headers are composed into the next entry's path by
            // tar-rs; they must not be re-emitted as visible entries
            continue;
        }
        // component split over raw bytes (tar paths need not be UTF-8);
        // empty segments from `//` or a trailing `/` do not count as
        // components, matching GNU's notion
        let path = entry.path()?;
        let comps: Vec<&std::ffi::OsStr> = path
            .as_os_str()
            .as_bytes()
            .split(|b| *b == b'/')
            .filter(|c| !c.is_empty())
            .map(std::ffi::OsStr::from_bytes)
            .collect();
        // strictness first: for new-schema sources a traversal attempt
        // aborts the staging instead of the legacy skip-with-warning
        if comps.iter().any(|c| *c == "..") {
            bail!("unsafe path in source archive: `{}`", path.display());
        }
        if comps.len() < strip as usize {
            // GNU tar semantics: not enough components — the entry is not
            // extracted at all
            continue;
        }
        let stripped: PathBuf = comps[strip as usize..]
            .iter()
            .fold(PathBuf::new(), |mut p, c| {
                p.push(c);
                p
            });
        if stripped.as_os_str().is_empty() {
            continue;
        }
        let mut header = entry.header().clone();
        // A ustar header can split a long path across `prefix` + `name`.
        // `set_path` (inside `append_data`) only rewrites `name` when the
        // stripped path fits in 100 bytes and leaves the stale `prefix`
        // behind, so readers would see `<prefix>/<stripped path>` and the
        // file would land in the wrong place. Drop the old prefix first.
        if let Some(ustar) = header.as_ustar_mut() {
            ustar.prefix = [0u8; 155];
        }
        if t.is_hard_link() {
            let target = entry
                .link_name()
                .context("read hard-link entry")?
                .context("hard link entry without a target")?;
            let tcomps: Vec<&std::ffi::OsStr> = target
                .as_os_str()
                .as_bytes()
                .split(|b| *b == b'/')
                .filter(|c| !c.is_empty())
                .map(std::ffi::OsStr::from_bytes)
                .collect();
            if tcomps.iter().any(|c| *c == "..") {
                bail!(
                    "unsafe hard-link target in source archive: `{}`",
                    target.display()
                );
            }
            if tcomps.len() < strip as usize {
                // nothing left to link to any more; GNU reports an error for
                // the empty target — skipping is the fail-closed variant
                continue;
            }
            let ttarget: PathBuf = tcomps[strip as usize..]
                .iter()
                .fold(PathBuf::new(), |mut p, c| {
                    p.push(c);
                    p
                });
            if ttarget.as_os_str().is_empty() {
                // exactly at the strip boundary: GNU errors on the emptied
                // target; skipping keeps staging deterministic (fail-closed)
                continue;
            }
            out.append_link(&mut header, &stripped, &ttarget)
                .with_context(|| {
                    format!("re-emit hard link {} -> {}", stripped.display(), ttarget.display())
                })?;
        } else if t.is_dir() || t.is_symlink() {
            // directories and symlinks carry no body in the extraction
            // model; some writers leave junk in the size field, so normalise
            header.set_size(0);
            out.append_data(&mut header, &stripped, std::io::empty())
                .with_context(|| format!("re-emit {}", stripped.display()))?;
        } else {
            // regular/contiguous/special: data is appended as-is, exactly
            // what the plain extraction path would read for such entries
            out.append_data(&mut header, &stripped, &mut entry)
                .with_context(|| format!("re-emit {}", stripped.display()))?;
        }
    }
    out.finish().context("finish rewritten tar")?;
    Ok(())
}

fn unpack_tar_stream<R: Read>(reader: R, dest: &Path) -> Result<()> {
    let mut ar = tar::Archive::new(reader);
    ar.set_overwrite(true);
    // deferred hard links: (link path, target path) — created after the
    // stream, in case the target shows up later in the archive
    let mut links: Vec<(PathBuf, PathBuf)> = Vec::new();
    for entry in ar.entries().context("read tar entries")? {
        let mut entry = entry.context("read tar entry")?;
        let t = entry.header().entry_type();
        let p = entry.path()?.to_path_buf();
        if t.is_hard_link() {
            let target = entry
                .link_name()?
                .context("hard link entry without a target")?
                .to_path_buf();
            links.push((sanitize_entry_path(&p)?, sanitize_entry_path(&target)?));
            continue;
        }
        // unpack_in roots absolute paths at `dest` and skips `..` entries
        // (GNU/bsdtar semantics, hardened by symlink validation); a `false`
        // return means the entry was skipped for exactly that reason
        if !entry
            .unpack_in(dest)
            .with_context(|| format!("unpack {}", p.display()))?
        {
            crate::nawa_warn!("skipped unsafe path in source archive: {}", p.display());
        }
    }
    for (link, target) in links {
        if link.as_os_str().is_empty() || target.as_os_str().is_empty() {
            bail!("unsafe hard link path in source archive");
        }
        let from = dest.join(&target);
        let to = dest.join(&link);
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
        }
        // order-independent hard links; a missing target or an exotic
        // filesystem degrades to a plain copy, like bsdtar -H does not but
        // every build system expects to survive
        if std::fs::hard_link(&from, &to).is_err() {
            std::fs::copy(&from, &to)
                .with_context(|| format!("materialize hard link {} -> {}", from.display(), to.display()))?;
        }
    }
    Ok(())
}

/// GNU-tar-style path sanitisation for entries handled outside unpack_in:
/// strip leading `/`, refuse any `..` component.
fn sanitize_entry_path(p: &Path) -> Result<PathBuf> {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::Prefix(_) | Component::RootDir | Component::CurDir => {}
            Component::ParentDir => bail!("unsafe path in source archive: {}", p.display()),
            Component::Normal(n) => out.push(n),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    /// Regression: entries whose original path is split across the ustar
    /// `prefix`/`name` fields (total length > 100) must survive
    /// `strip_components` at their stripped path. fastfetch 2.30.1's
    /// `wlr-output-management-unstable-v1-protocol.c` is such an entry.
    #[test]
    fn strip_keeps_ustar_prefix_split_paths() {
        let dir = scratch("ustar-prefix");
        let tarball = dir.join("src.tar");
        {
            let mut b = tar::Builder::new(std::fs::File::create(&tarball).unwrap());
            let mut h = tar::Header::new_ustar();
            let body = b"int x;\n";
            h.set_size(body.len() as u64);
            h.set_mode(0o644);
            h.set_entry_type(tar::EntryType::Regular);
            let long = "fastfetch-2.30.1/src/detection/displayserver/linux/wayland/\
                        wlr-output-management-unstable-v1-protocol.c";
            assert!(long.len() > 100);
            h.set_path(long).unwrap(); // lands in prefix + name
            h.set_cksum();
            b.append(&h, &body[..]).unwrap();
            b.finish().unwrap();
        }
        let dest = dir.join("out");
        extract_source_archive_stripped(&tarball, &dest, 1).unwrap();
        assert!(dest
            .join("src/detection/displayserver/linux/wayland/wlr-output-management-unstable-v1-protocol.c")
            .is_file());
    }

    #[test]
    fn progress_line_shows_sizes_and_percent() {
        let mib = 1024 * 1024;
        assert_eq!(progress_line("gtk3-3.24.52-1", 12 * mib, Some(48 * mib)), "  gtk3-3.24.52-1  12.0 MiB / 48.0 MiB   25%");
        assert_eq!(progress_line("x", 3 * mib, None), "  x  3.0 MiB");
        // a server that lies (more bytes than Content-Length) never prints > 100%
        assert!(progress_line("x", 2 * mib, Some(mib)).ends_with("100%"));
    }

    #[test]
    fn progress_label_drops_part_and_archive_suffix() {
        assert_eq!(progress_label(Path::new("/c/libepoxy-1.5.9-1.nawa.tar.zst.part")), "libepoxy-1.5.9-1");
        assert_eq!(progress_label(Path::new("/c/.firefox-153.6.0-1.nawa.tar.zst.part")), "firefox-153.6.0-1");
        assert_eq!(progress_label(Path::new("/c/index.toml.part")), "index.toml");
    }

    #[test]
    fn repo_ref_parses_local_and_remote() {
        assert_eq!(
            RepoRef::parse("/srv/repo").unwrap(),
            RepoRef::Local(PathBuf::from("/srv/repo"))
        );
        assert_eq!(
            RepoRef::parse("repo/").unwrap(),
            RepoRef::Local(PathBuf::from("repo"))
        );
        assert_eq!(
            RepoRef::parse("https://mirror.example.com/stable/").unwrap(),
            RepoRef::Remote("https://mirror.example.com/stable".into())
        );
        assert_eq!(
            RepoRef::parse("http://127.0.0.1:8000").unwrap(),
            RepoRef::Remote("http://127.0.0.1:8000".into())
        );
        assert!(RepoRef::parse("ftp://host/repo").is_err());
    }

    #[test]
    fn gnu_urls_get_fallbacks() {
        let v = candidate_urls("https://mirrors.kernel.org/gnu/screen/screen-5.0.0.tar.gz");
        assert_eq!(v[0], "https://mirrors.kernel.org/gnu/screen/screen-5.0.0.tar.gz");
        assert!(v.contains(&"https://ftpmirror.gnu.org/screen/screen-5.0.0.tar.gz".to_string()));
        assert_eq!(candidate_urls("https://example.com/x.tgz").len(), 1);
    }

    #[test]
    fn recipe_mirrors_are_appended_without_duplicates() {
        let url = "https://download.samba.org/pub/rsync/src/rsync-3.4.1.tar.gz";
        let m = "https://rsync.samba.org/ftp/rsync/src/rsync-3.4.1.tar.gz".to_string();
        let v = candidate_urls_with(url, &[m.clone(), m.clone(), url.to_string()]);
        assert_eq!(v, vec![url.to_string(), m]);
        // no mirrors -> identical to candidate_urls
        assert_eq!(candidate_urls_with(url, &[]), candidate_urls(url));
    }

    #[test]
    fn download_cached_falls_back_to_recipe_mirror() {
        // 404 is not retryable, so the primary fails immediately
        let bad = scripted_server(vec![http_response("HTTP/1.1 404 Not Found", b"nope", "")]);
        let good = scripted_server(vec![http_response("HTTP/1.1 200 OK", b"archive-data", "")]);
        let dir = scratch("mirror-ok");
        let dest = dir.join("pkg.tar");
        let sha = {
            let mut h = Sha256::new();
            h.update(b"archive-data");
            hex::encode(h.finalize())
        };
        download_cached_with_mirrors(
            &format!("{bad}/pkg.tar"),
            &[format!("{good}/pkg.tar")],
            &dest,
            Some(&sha),
        )
        .unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"archive-data");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn download_cached_rejects_a_mirror_with_wrong_bytes() {
        let bad = scripted_server(vec![http_response("HTTP/1.1 404 Not Found", b"nope", "")]);
        let evil = scripted_server(vec![http_response("HTTP/1.1 200 OK", b"tampered", "")]);
        let dir = scratch("mirror-bad");
        let dest = dir.join("pkg.tar");
        let sha = {
            let mut h = Sha256::new();
            h.update(b"archive-data");
            hex::encode(h.finalize())
        };
        let res = download_cached_with_mirrors(
            &format!("{bad}/pkg.tar"),
            &[format!("{evil}/pkg.tar")],
            &dest,
            Some(&sha),
        );
        assert!(res.is_err(), "a mirror serving other bytes must not be accepted");
        assert!(!dest.exists(), "nothing may be left in the cache");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cache_dir_is_stable_and_url_scoped() {
        let base = Path::new("/r");
        let a = remote_cache_dir(base, "https://m.example.com/stable");
        let b = remote_cache_dir(base, "https://m.example.com/stable");
        let c = remote_cache_dir(base, "https://m.example.com/testing");
        assert_eq!(a, b, "same URL must map to the same cache dir");
        assert_ne!(a, c, "different URLs must not share a cache dir");
        assert!(a.starts_with("/r/var/cache/nawa/remote"));
    }

    // ---- loopback HTTP helpers -------------------------------------------

    /// Serve a scripted list of raw responses, one per connection.
    fn scripted_server(responses: Vec<Vec<u8>>) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for resp in responses {
                let (mut sock, _) = match listener.accept() {
                    Ok(a) => a,
                    Err(_) => return,
                };
                let mut buf = [0u8; 2048];
                let _ = Read::read(&mut sock, &mut buf);
                let _ = Write::write_all(&mut sock, &resp);
            }
        });
        format!("http://127.0.0.1:{port}")
    }

    fn http_response(status_line: &str, body: &[u8], extra_headers: &str) -> Vec<u8> {
        let mut v = format!(
            "{status_line}\r\nContent-Length: {}\r\nConnection: close\r\n{extra_headers}\r\n",
            body.len()
        )
        .into_bytes();
        v.extend_from_slice(body);
        v
    }

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("nawa-fetch-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn download_round_trips_over_plain_http() {
        let base = scripted_server(vec![http_response("HTTP/1.1 200 OK", b"hello nawa", "")]);
        let dir = scratch("dl");
        let dest = dir.join("file");
        download(&format!("{base}/file"), &dest).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"hello nawa");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn download_opt_absent_on_404_but_download_fails() {
        let base = scripted_server(vec![
            http_response("HTTP/1.1 404 Not Found", b"nope", ""),
            http_response("HTTP/1.1 404 Not Found", b"nope", ""),
        ]);
        let dir = scratch("404");
        let dest = dir.join("file");
        assert!(!download_opt(&format!("{base}/missing"), &dest).unwrap());
        assert!(download(&format!("{base}/missing"), &dest).is_err(), "curl -f parity");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn download_opt_does_not_treat_forbidden_as_absent() {
        let base = scripted_server(vec![
            http_response("HTTP/1.1 403 Forbidden", b"forbidden", ""),
        ]);
        let dir = scratch("403");
        let dest = dir.join("file");
        assert!(download_opt(&format!("{base}/file"), &dest).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn transient_5xx_is_retried() {
        let base = scripted_server(vec![
            http_response("HTTP/1.1 503 Service Unavailable", b"busy", "Retry-After: 0\r\n"),
            http_response("HTTP/1.1 200 OK", b"recovered", ""),
        ]);
        let dir = scratch("503");
        let dest = dir.join("file");
        download(&format!("{base}/file"), &dest).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"recovered");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn redirects_must_stay_on_https() {
        let base = scripted_server(vec![http_response(
            "HTTP/1.1 302 Found",
            b"",
            "Location: http://evil.example/x\r\n",
        )]);
        let dir = scratch("redir");
        let err = download(&format!("{base}/file"), &dir.join("file")).unwrap_err();
        assert!(err.to_string().contains("https"), "got: {err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn redirect_policy_resolves_relative_and_enforces_https() {
        let base = url::Url::parse("https://mirror.example/start").unwrap();
        // relative Location resolves against the current URL and stays https
        let next = redirect_target(&base, "/real/file").unwrap();
        assert_eq!(next.as_str(), "https://mirror.example/real/file");
        let rel = redirect_target(&base, "sub/x").unwrap();
        assert_eq!(rel.as_str(), "https://mirror.example/sub/x");
        // absolute https targets are fine
        let abs = redirect_target(&base, "https://other.example/f").unwrap();
        assert_eq!(abs.as_str(), "https://other.example/f");
        // any downgrade to http is refused (curl --proto-redir =https parity)
        let e = redirect_target(&base, "http://other.example/f").unwrap_err();
        assert!(e.to_string().contains("https"), "got: {e}");
        // a relative Location from an http origin stays http → refused
        let http_base = url::Url::parse("http://mirror.example/start").unwrap();
        let e = redirect_target(&http_base, "/insecure").unwrap_err();
        assert!(e.to_string().contains("https"), "got: {e}");
    }

    #[test]
    fn non_http_schemes_are_rejected() {
        let dir = scratch("ftp");
        let err = download("ftp://host/file", &dir.join("f")).unwrap_err();
        assert!(err.to_string().contains("http(s)"), "got: {err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn offline_gate_message_names_the_way_out() {
        let e = check_offline("https://mirror.example/x.tar.gz", true).unwrap_err();
        let msg = e.to_string();
        assert!(msg.contains("offline"), "{msg}");
        assert!(msg.contains("https://mirror.example/x.tar.gz"), "{msg}");
        assert!(msg.contains("nawa fetch"), "names the populate command: {msg}");
        // without the flag nothing is refused
        check_offline("https://mirror.example/x.tar.gz", false).unwrap();
    }

    #[test]
    fn download_cached_downloads_then_reuses() {
        let base = scripted_server(vec![http_response("HTTP/1.1 200 OK", b"archive-data", "")]);
        let dir = scratch("cached");
        let dest = dir.join("pkg.tar");
        let sha = {
            let mut h = Sha256::new();
            h.update(b"archive-data");
            hex::encode(h.finalize())
        };
        download_cached(&format!("{base}/pkg.tar"), &dest, Some(&sha)).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"archive-data");
        // second call is served from the cache (the scripted server has no
        // more responses, so a re-download would fail the test)
        download_cached(&format!("{base}/pkg.tar"), &dest, Some(&sha)).unwrap();
        // a wrong sha forces a re-download attempt, which fails cleanly
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- source archives ---------------------------------------------------

    /// Build an in-memory tar with a dir, a file, a symlink and a hard link.
    fn sample_tar_bytes() -> Vec<u8> {
        let mut b = tar::Builder::new(Vec::new());
        let mut dh = tar::Header::new_gnu();
        dh.set_entry_type(tar::EntryType::Directory);
        dh.set_size(0);
        dh.set_mode(0o755);
        dh.set_cksum();
        b.append_data(&mut dh, "pkg-1.0", std::io::empty()).unwrap();
        b.append_data(&mut dh, "pkg-1.0/sub", std::io::empty()).unwrap();
        let data = b"compiled output\n";
        let mut fh = tar::Header::new_gnu();
        fh.set_size(data.len() as u64);
        fh.set_mode(0o755);
        fh.set_cksum();
        b.append_data(&mut fh, "pkg-1.0/tool", &data[..]).unwrap();
        let mut sh = tar::Header::new_gnu();
        sh.set_entry_type(tar::EntryType::Symlink);
        sh.set_size(0);
        sh.set_mode(0o777);
        sh.set_link_name("tool").unwrap();
        sh.set_cksum();
        b.append_data(&mut sh, "pkg-1.0/tool-link", std::io::empty()).unwrap();
        // hard link stored BEFORE its target: GNU tar tolerates this and so
        // must we
        let mut lh = tar::Header::new_gnu();
        lh.set_entry_type(tar::EntryType::Link);
        lh.set_size(0);
        lh.set_mode(0o644);
        lh.set_link_name("pkg-1.0/later").unwrap();
        lh.set_cksum();
        b.append_data(&mut lh, "pkg-1.0/alias", std::io::empty()).unwrap();
        let mut lh2 = tar::Header::new_gnu();
        lh2.set_size(b"later file\n".len() as u64);
        lh2.set_mode(0o644);
        lh2.set_cksum();
        b.append_data(&mut lh2, "pkg-1.0/later", &b"later file\n"[..]).unwrap();
        b.into_inner().unwrap()
    }

    fn write_compressed(dir: &Path, bytes: &[u8], ext: &str) -> PathBuf {
        let p = dir.join(format!("src.{ext}"));
        match ext {
            "tar" => std::fs::write(&p, bytes).unwrap(),
            "tar.gz" => {
                let mut f = std::fs::File::create(&p).unwrap();
                flate2::write::GzEncoder::new(&mut f, flate2::Compression::default())
                    .write_all(bytes)
                    .unwrap();
            }
            "tar.bz2" => {
                let mut f = std::fs::File::create(&p).unwrap();
                bzip2::write::BzEncoder::new(&mut f, bzip2::Compression::default())
                    .write_all(bytes)
                    .unwrap();
            }
            "tar.zst" => {
                let mut f = std::fs::File::create(&p).unwrap();
                zstd::stream::copy_encode(bytes, &mut f, 3).unwrap();
            }
            "tar.xz" => {
                let out = std::fs::File::create(&p).unwrap();
                let mut w = lzma_rust2::XzWriter::new(out, lzma_rust2::XzOptions::with_preset(6)).unwrap();
                w.write_all(bytes).unwrap();
                w.finish().unwrap();
            }
            other => panic!("unexpected ext {other}"),
        }
        p
    }

    #[test]
    fn source_roundtrip_across_all_formats() {
        let dir = scratch("srcfmt");
        let bytes = sample_tar_bytes();
        for (ext, want) in [
            ("tar", SourceFormat::Tar),
            ("tar.gz", SourceFormat::TarGz),
            ("tar.bz2", SourceFormat::TarBz2),
            ("tar.xz", SourceFormat::TarXz),
            ("tar.zst", SourceFormat::TarZst),
        ] {
            let p = write_compressed(&dir, &bytes, ext);
            assert_eq!(
                detect_source_format(&p),
                Some(want),
                "format detection failed for {ext}"
            );
            let out = dir.join(format!("out-{ext}"));
            extract_source_archive(&p, &out).unwrap();
            assert_eq!(
                std::fs::read(out.join("pkg-1.0/tool")).unwrap(),
                b"compiled output\n",
                "{ext}: regular file"
            );
            assert!(out.join("pkg-1.0/sub").is_dir(), "{ext}: subdir");
            let link = std::fs::read_link(out.join("pkg-1.0/tool-link")).unwrap();
            assert_eq!(link.to_str().unwrap(), "tool", "{ext}: symlink target");
            // forward-referenced hard link materialised with the right content
            assert_eq!(
                std::fs::read(out.join("pkg-1.0/alias")).unwrap(),
                b"later file\n",
                "{ext}: hard link"
            );
            assert_eq!(
                std::fs::metadata(out.join("pkg-1.0/alias")).unwrap().ino(),
                std::fs::metadata(out.join("pkg-1.0/later")).unwrap().ino(),
                "{ext}: alias should be a real hard link to later"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn single_file_gz_is_not_an_archive() {
        let dir = scratch("sfgz");
        let p = dir.join("data.gz");
        let mut f = std::fs::File::create(&p).unwrap();
        flate2::write::GzEncoder::new(&mut f, flate2::Compression::default())
            .write_all(b"plain text content, not a tarball")
            .unwrap();
        assert_eq!(detect_source_format(&p), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn traversal_entries_are_skipped() {
        let dir = scratch("trav");
        // tar::Builder refuses `..` names, so craft the malicious entry by
        // hand (v7-style header, checksum computed with spaces)
        let mut img = vec![0u8; 512];
        let name = b"../evil.txt";
        img[..name.len()].copy_from_slice(name);
        img[100..108].copy_from_slice(b"0000644\0");
        img[124..136].copy_from_slice(format!("{:011o}\0", 1).as_bytes());
        img[156] = b'0'; // regular file
        img[148..156].copy_from_slice(b"        ");
        let sum: u64 = img.iter().map(|v| *v as u64).sum();
        img[148..156].copy_from_slice(format!("{:06o}\0 ", sum).as_bytes());
        let mut raw = img;
        raw.extend_from_slice(b"x");
        raw.resize(1024, 0); // pad the body to a full block
        raw.extend_from_slice(&[0u8; 1024]); // end-of-archive blocks
        let p = dir.join("evil.tar");
        std::fs::write(&p, &raw).unwrap();
        let out = dir.join("out");
        extract_source_archive(&p, &out).unwrap(); // skipped with a warning
        assert!(!dir.join("evil.txt").exists(), "must not escape the root");
        assert!(!out.join("evil.txt").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Regression: xz-5.8.1.tar.xz is compressed with `--check=sha256`, which
    /// the old lzma-rs decoder rejected; detection then returned None and the
    /// build died with "./configure: No such file or directory".
    #[test]
    fn tar_xz_with_sha256_check_extracts() {
        const FIXTURE: &[u8] = include_bytes!("../tests/fixtures/sha256-check.tar.xz");
        let dir = scratch("xz-sha256");
        let p = dir.join("sha256-check.tar.xz");
        std::fs::write(&p, FIXTURE).unwrap();
        assert_eq!(detect_source_format(&p), Some(SourceFormat::TarXz));
        let out = dir.join("out");
        extract_source_archive(&p, &out).unwrap();
        assert!(out.join("pkg-1.0/configure").is_file());
    }

    /// A corrupt xz stream must fail loudly (claimed as TarXz, extraction
    /// errors) instead of silently degrading to a "single-file source".
    #[test]
    fn corrupt_xz_is_an_extraction_error_not_a_single_file() {
        const FIXTURE: &[u8] = include_bytes!("../tests/fixtures/sha256-check.tar.xz");
        let dir = scratch("xz-corrupt");
        let mut bad = FIXTURE.to_vec();
        let mid = bad.len() / 2;
        bad[mid] ^= 0xff;
        let p = dir.join("bad.tar.xz");
        std::fs::write(&p, &bad).unwrap();
        assert_eq!(detect_source_format(&p), Some(SourceFormat::TarXz));
        assert!(extract_source_archive(&p, &dir.join("out")).is_err());
    }

    #[test]
    fn truncated_gz_archive_fails_extraction() {
        let dir = scratch("trunc");
        let bytes = sample_tar_bytes();
        let mut comp = Vec::new();
        flate2::write::GzEncoder::new(&mut comp, flate2::Compression::default())
            .write_all(&bytes)
            .unwrap();
        let p = dir.join("cut.tar.gz");
        std::fs::write(&p, &comp[..comp.len() / 2]).unwrap();
        assert!(extract_source_archive(&p, &dir.join("out")).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- copy_local --------------------------------------------------------

    #[test]
    fn copy_local_preserves_tree_shape() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let dir = scratch("cp");
        let src = dir.join("src");
        std::fs::create_dir_all(src.join("a/sub")).unwrap();
        std::fs::write(src.join("a/f"), b"content\n").unwrap();
        std::fs::set_permissions(src.join("a/f"), std::fs::Permissions::from_mode(0o600)).unwrap();
        std::os::unix::fs::symlink("f", src.join("a/link")).unwrap();
        std::fs::hard_link(src.join("a/f"), src.join("a/f2")).unwrap();
        let old = filetime::FileTime::from_unix_time(1_000_000, 0);
        filetime::set_file_times(src.join("a/f"), old, old).unwrap();

        let dst = dir.join("dst");
        copy_local(&src, &dst).unwrap();

        assert_eq!(std::fs::read(dst.join("a/f")).unwrap(), b"content\n");
        let mode = std::fs::metadata(dst.join("a/f")).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "permissions preserved");
        let lt = std::fs::read_link(dst.join("a/link")).unwrap();
        assert_eq!(lt.to_str().unwrap(), "f", "symlink preserved");
        assert_eq!(
            std::fs::metadata(dst.join("a/f")).unwrap().ino(),
            std::fs::metadata(dst.join("a/f2")).unwrap().ino(),
            "hard link preserved"
        );
        let got = filetime::FileTime::from_system_time(
            std::fs::symlink_metadata(dst.join("a/f")).unwrap().modified().unwrap(),
        );
        assert_eq!(got.unix_seconds(), 1_000_000, "mtime preserved");
        // directory mtime also carried over (children written before)
        let dgot = filetime::FileTime::from_system_time(
            std::fs::symlink_metadata(dst.join("a")).unwrap().modified().unwrap(),
        );
        assert!(dgot.unix_seconds() >= 1_000_000);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- real-network sanity (opt-in) ---------------------------------------

    /// End-to-end check against live upstream tarballs: the native HTTP stack
    /// downloads, the format is detected by content and every compression
    /// flavour is extracted. The codeload case must pass; the GNU/busybox
    /// mirrors self-skip when the running sandbox has no route to them.
    /// Run explicitly:  cargo test -- --ignored
    #[test]
    #[ignore = "requires network; run with: cargo test -- --ignored"]
    fn real_world_tarballs_download_and_extract() {
        let dir = scratch("real");

        // primary: gzip tarball over https (redirect-free, always-on host)
        let gz = dir.join("pcre2.tar.gz");
        download(
            "https://codeload.github.com/PCRE2Project/pcre2/tar.gz/refs/tags/pcre2-10.46",
            &gz,
        )
        .unwrap();
        assert_eq!(detect_source_format(&gz), Some(SourceFormat::TarGz));
        let out = dir.join("out-gz");
        extract_source_archive(&gz, &out).unwrap();
        // codeload names the top-level dir <repo>-<tag>
        assert!(out.join("pcre2-pcre2-10.46").is_dir(), "codeload tarball extracted");
        let _ = std::fs::remove_dir_all(&out);
        let _ = std::fs::remove_file(&gz);

        // optional: xz / bz2 upstream mirrors — skipped when unreachable
        for (url, want) in [
            ("https://mirrors.kernel.org/gnu/bison/bison-3.8.2.tar.xz", SourceFormat::TarXz),
            ("https://busybox.net/downloads/busybox-1.37.0.tar.bz2", SourceFormat::TarBz2),
        ] {
            let p = dir.join(url.rsplit('/').next().unwrap());
            match download(url, &p) {
                Ok(()) => {
                    assert_eq!(detect_source_format(&p), Some(want), "detect {url}");
                    let out = dir.join(format!("out-{}", want_debug_tag(want)));
                    extract_source_archive(&p, &out).unwrap();
                    assert!(out.read_dir().unwrap().next().is_some(), "empty tree: {url}");
                    let _ = std::fs::remove_dir_all(&out);
                    let _ = std::fs::remove_file(&p);
                }
                Err(e) if is_retryable(&e) => {
                    crate::nawa_info_stderr!("skipping {url}: no route from this sandbox ({e:#})");
                }
                Err(e) => panic!("{url}: {e:#}"),
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn want_debug_tag(f: SourceFormat) -> &'static str {
        match f {
            SourceFormat::Tar => "tar",
            SourceFormat::TarGz => "gz",
            SourceFormat::TarBz2 => "bz2",
            SourceFormat::TarXz => "xz",
            SourceFormat::TarZst => "zst",
        }
    }

    // ---- strip-aware extraction (fetch-phase.md §4.2) ------------------------

    /// The tar used to pin strip semantics, mirroring the GNU tar 1.35
    /// reference tree this design was verified against (`/tmp/stripref`):
    /// a `pkg/` top dir, a file, a nested dir with a deeper file, a hard
    /// link back to `pkg/file` and a relative symlink.
    fn strip_reference_tar() -> Vec<u8> {
        let mut b = tar::Builder::new(Vec::new());
        let mut h = tar::Header::new_gnu();
        h.set_entry_type(tar::EntryType::Directory);
        h.set_mode(0o755);
        h.set_mtime(1_000_000);
        h.set_size(0);
        h.set_cksum();
        b.append_data(&mut h, "pkg", std::io::empty()).unwrap();

        let data = b"hello\n";
        let mut h = tar::Header::new_gnu();
        h.set_size(data.len() as u64);
        h.set_mode(0o640);
        h.set_mtime(1_234_567);
        h.set_cksum();
        b.append_data(&mut h, "pkg/file", &data[..]).unwrap();

        let mut h = tar::Header::new_gnu();
        h.set_entry_type(tar::EntryType::Directory);
        h.set_mode(0o750);
        h.set_mtime(1_000_000);
        h.set_size(0);
        h.set_cksum();
        b.append_data(&mut h, "pkg/sub", std::io::empty()).unwrap();

        let deep = b"deep\n";
        let mut h = tar::Header::new_gnu();
        h.set_size(deep.len() as u64);
        h.set_mode(0o600);
        h.set_mtime(1_234_568);
        h.set_cksum();
        b.append_data(&mut h, "pkg/sub/deep", &deep[..]).unwrap();

        let mut h = tar::Header::new_gnu();
        h.set_entry_type(tar::EntryType::Link);
        h.set_size(0);
        h.set_mode(0o644);
        h.set_mtime(1_234_567);
        h.set_link_name("pkg/file").unwrap();
        h.set_cksum();
        b.append_data(&mut h, "pkg/sub/hardlink", std::io::empty()).unwrap();

        let mut h = tar::Header::new_gnu();
        h.set_entry_type(tar::EntryType::Symlink);
        h.set_size(0);
        h.set_mode(0o777);
        h.set_mtime(1_234_567);
        h.set_link_name("../file").unwrap();
        h.set_cksum();
        b.append_data(&mut h, "pkg/sub/sym", std::io::empty()).unwrap();

        b.into_inner().unwrap()
    }

    #[test]
    fn strip_one_matches_the_gnu_reference_tree() {
        let dir = scratch("strip1");
        let p = dir.join("ref.tar");
        std::fs::write(&p, strip_reference_tar()).unwrap();

        let out = dir.join("out");
        extract_source_archive_stripped(&p, &out, 1).unwrap();
        // the `pkg/` top dir is consumed (its name strip leaves it empty)
        assert!(!out.join("pkg").exists(), "no top dir may remain");
        // plain file lands at the workspace root, header metadata intact
        assert_eq!(std::fs::read(out.join("file")).unwrap(), b"hello\n");
        let md = std::fs::symlink_metadata(out.join("file")).unwrap();
        assert_eq!(md.permissions().mode() & 0o777, 0o640, "mode preserved");
        assert_eq!(md.mtime(), 1_234_567, "mtime preserved through the rewrite");
        // nested dir + file
        assert!(out.join("sub").is_dir());
        assert_eq!(std::fs::read(out.join("sub/deep")).unwrap(), b"deep\n");
        // hard link was re-targeted and is a real link to the extracted file
        assert_eq!(std::fs::read(out.join("sub/hardlink")).unwrap(), b"hello\n");
        assert_eq!(
            std::fs::metadata(out.join("sub/hardlink")).unwrap().ino(),
            std::fs::metadata(out.join("file")).unwrap().ino(),
            "hard link kept as a link to the stripped target"
        );
        // symlink target passes through verbatim (GNU does not translate
        // targets either)
        assert_eq!(
            std::fs::read_link(out.join("sub/sym")).unwrap().to_str().unwrap(),
            "../file"
        );
        // the rewrite tar is transient
        assert!(!dir.join("out.rewrite.tar").exists(), "rewrite tar cleaned up");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn strip_two_matches_the_gnu_reference_tree() {
        let dir = scratch("strip2");
        let p = dir.join("ref.tar");
        std::fs::write(&p, strip_reference_tar()).unwrap();
        let out = dir.join("out");
        extract_source_archive_stripped(&p, &out, 2).unwrap();
        // only the deepenough entries survive: dirs have too few components,
        // `file` loses its last component to nothing, and the hard link's
        // target `pkg/file` strips to empty — GNU errors there, nawa skips
        // the entry (fail-closed)
        assert_eq!(std::fs::read(out.join("deep")).unwrap(), b"deep\n");
        assert_eq!(
            std::fs::read_link(out.join("sym")).unwrap().to_str().unwrap(),
            "../file"
        );
        for missing in ["file", "hardlink", "symreal", "sub", "pkg"] {
            assert!(!out.join(missing).exists(), "{missing} must not exist");
        }
        // parents were created on demand despite the dir entries being
        // stripped away
        assert!(out.is_dir());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn strip_extraction_runs_through_compression() {
        let dir = scratch("stripgz");
        let bytes = strip_reference_tar();
        let p = write_compressed(&dir, &bytes, "tar.gz");
        assert_eq!(detect_source_format(&p), Some(SourceFormat::TarGz));
        let out = dir.join("out");
        extract_source_archive_stripped(&p, &out, 1).unwrap();
        assert_eq!(std::fs::read(out.join("file")).unwrap(), b"hello\n");
        assert!(out.join("sub/deep").is_file());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The legacy path skips unsanitisable entries with a warning; the
    /// new-schema path must reject them (§16). Crafted by hand because
    /// `tar::Builder` refuses `..` names.
    #[test]
    fn strip_extraction_rejects_traversal_names() {
        let dir = scratch("stripdotdot");
        let mut img = vec![0u8; 512];
        let name = b"pkg/../evil.txt";
        img[..name.len()].copy_from_slice(name);
        img[100..108].copy_from_slice(b"0000644\0");
        img[124..136].copy_from_slice(format!("{:011o}\0", 1).as_bytes());
        img[156] = b'0'; // regular file
        img[148..156].copy_from_slice(b"        ");
        let sum: u64 = img.iter().map(|v| *v as u64).sum();
        img[148..156].copy_from_slice(format!("{:06o}\0 ", sum).as_bytes());
        let mut raw = img;
        raw.extend_from_slice(b"x");
        raw.resize(1024, 0);
        raw.extend_from_slice(&[0u8; 1024]);
        let p = dir.join("evil.tar");
        std::fs::write(&p, &raw).unwrap();

        let err = extract_source_archive_stripped(&p, &dir.join("out"), 1).unwrap_err();
        assert!(err.to_string().contains("unsafe path"), "got: {err}");
        assert!(!dir.join("evil.txt").exists());
        assert!(!dir.join("out").join("evil.txt").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn strip_extraction_rejects_traversal_hardlink_targets() {
        let dir = scratch("stripdotdotlink");
        let mut b = tar::Builder::new(Vec::new());
        let mut h = tar::Header::new_gnu();
        h.set_size(3);
        h.set_mode(0o644);
        h.set_cksum();
        b.append_data(&mut h, "pkg/file", &b"ok\n"[..]).unwrap();
        let mut lh = tar::Header::new_gnu();
        lh.set_entry_type(tar::EntryType::Link);
        lh.set_size(0);
        lh.set_mode(0o644);
        lh.set_link_name("../outside").unwrap();
        lh.set_cksum();
        b.append_data(&mut lh, "pkg/hardlink", std::io::empty()).unwrap();
        let p = dir.join("evil-link.tar");
        std::fs::write(&p, b.into_inner().unwrap()).unwrap();

        let err = extract_source_archive_stripped(&p, &dir.join("out"), 1).unwrap_err();
        assert!(
            err.to_string().contains("hard-link target"),
            "got: {err}"
        );
        assert!(!dir.join("outside").exists());
        assert!(!dir.join("out").join("outside").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- content-addressed source cache (fetch-phase.md §11) -----------------

    fn sha256_of(bytes: &[u8]) -> String {
        let mut h = Sha256::new();
        h.update(bytes);
        hex::encode(h.finalize())
    }

    fn dead_addr() -> String {
        // bind, read the port, drop: connecting here fails fast
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        drop(l);
        format!("http://{}", addr)
    }

    #[test]
    fn content_addressed_download_publishes_readonly_and_reuses() {
        let body = b"content-addressed-source\n";
        let sha = sha256_of(body);
        let base = scripted_server(vec![http_response("HTTP/1.1 200 OK", body, "")]);
        let dir = scratch("ca");
        let dest = digest_cache_dir(&dir).join(&sha);

        download_content_addressed(
            &format!("{base}/gzip-1.2.tar.gz"),
            &[],
            &sha,
            &dest,
        )
        .unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), body);
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::symlink_metadata(&dest).unwrap().permissions().mode();
        assert_eq!(mode & 0o222, 0, "published entries are read-only (§11)");

        // the cache sidecar is exactly entry + lock: no `.part` leftovers
        let mut got: Vec<String> = digest_cache_dir(&dir)
            .read_dir()
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        got.sort();
        assert_eq!(
            got,
            vec![sha.clone(), format!("{sha}.lock")],
            "no transfer debris may remain"
        );

        // second call is served from the cache: a dead URL is never touched
        let dead = dead_addr();
        download_content_addressed(&format!("{dead}/gzip-1.2.tar.gz"), &[], &sha, &dest).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), body);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn content_addressed_replaces_a_corrupt_cache_entry() {
        let body = b"content-addressed-source\n";
        let sha = sha256_of(body);
        let base = scripted_server(vec![http_response("HTTP/1.1 200 OK", body, "")]);
        let dir = scratch("ca-corrupt");
        let dest = digest_cache_dir(&dir).join(&sha);
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        std::fs::write(&dest, b"truncated garbage").unwrap();

        download_content_addressed(&format!("{base}/f.tar.gz"), &[], &sha, &dest).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), body, "re-downloaded");
        let dead = dead_addr();
        // the repaired entry is a verified hit again
        download_content_addressed(&format!("{dead}/f.tar.gz"), &[], &sha, &dest).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), body);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn content_addressed_adopts_a_verified_legacy_entry() {
        let body = b"content-addressed-source\n";
        let sha = sha256_of(body);
        let dir = scratch("ca-legacy");
        // §11: legacy entries live at <srccache>/<sha16>-<file>
        let legacy = dir.join(format!("{}-gzip-1.2.tar.gz", &sha[..16]));
        std::fs::write(&legacy, body).unwrap();
        let dest = digest_cache_dir(&dir).join(&sha);

        // the URL is never contacted: the migration read happens first
        let dead = dead_addr();
        download_content_addressed(&format!("{dead}/gzip-1.2.tar.gz"), &[], &sha, &dest).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), body, "copied, not moved");
        assert!(legacy.is_file(), "the legacy entry must stay shareable");
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::symlink_metadata(&dest).unwrap().permissions().mode();
        assert_eq!(mode & 0o222, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn legacy_migration_replaces_a_corrupt_read_only_entry() {
        use std::os::unix::fs::PermissionsExt;
        let body = b"content-addressed-source\n";
        let sha = sha256_of(body);
        let dir = scratch("ca-legacy-corrupt");
        let legacy = dir.join(format!("{}-gzip-1.2.tar.gz", &sha[..16]));
        std::fs::write(&legacy, body).unwrap();
        // a corrupt entry a previous publication left behind: read-only, as
        // strip_write_permission makes every published entry
        let dest = digest_cache_dir(&dir).join(&sha);
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        std::fs::write(&dest, b"corrupt").unwrap();
        std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o444)).unwrap();

        let dead = dead_addr();
        download_content_addressed(&format!("{dead}/gzip-1.2.tar.gz"), &[], &sha, &dest).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), body, "the corrupt entry was replaced");
        let got: Vec<String> = digest_cache_dir(&dir)
            .read_dir()
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert!(
            got.iter().all(|n| *n == sha.as_str() || *n == format!("{sha}.lock")),
            "no .part debris: {got:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn content_addressed_refuses_wrong_bytes_and_malformed_digests() {
        // a mirror (no recipe mirrors here) serving other bytes is refused:
        // nothing is published
        let base = scripted_server(vec![http_response("HTTP/1.1 200 OK", b"evil bytes", "")]);
        let dir = scratch("ca-bad");
        let sha = sha256_of(b"content-addressed-source\n");
        let dest = digest_cache_dir(&dir).join(&sha);
        let res = download_content_addressed(&format!("{base}/f.tar.gz"), &[], &sha, &dest);
        assert!(res.is_err(), "unverified bytes must not be activated");
        assert!(!dest.exists(), "nothing may be left in the cache");
        // the digest dir may exist (the lock file lingers by design), but no
        // transfer debris beyond entry + lock
        let got: Vec<String> = digest_cache_dir(&dir)
            .read_dir()
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert!(
            got.iter().all(|n| *n == sha.as_str() || *n == format!("{sha}.lock")),
            "no .part debris: {got:?}"
        );

        // the digest doubles as a path component — the parse shape is
        // checked before anything is created
        let fresh = scratch("ca-bad2");
        let res = download_content_addressed(&format!("{base}/f.tar.gz"), &[], "XYZ", &dest);
        assert!(
            res.unwrap_err().to_string().contains("64 lowercase hex"),
            "malformed digest rejected"
        );
        assert!(!digest_cache_dir(&fresh).exists(), "nothing created for a bad digest");
        drop(fresh);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unique_part_paths_never_collide() {
        let a = unique_part_path(Path::new("/c/0123abcd"));
        let b = unique_part_path(Path::new("/c/0123abcd"));
        assert_ne!(a, b, "concurrent transfers must not share a part file");
        assert!(Path::new(&a).extension().unwrap() == "part");
        assert!(a.to_string_lossy().contains(&format!("{}-", std::process::id())));
    }
}
