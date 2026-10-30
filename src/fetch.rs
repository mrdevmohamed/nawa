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
//!   * `download_opt` still treats HTTP 404-class answers as "file absent"
//!     for optional `.sig` sidecars — but server errors (5xx) are retried and
//!     then reported, so a misbehaving mirror can no longer silently hide a
//!     signature;
//!   * `copy_local` is a pure-Rust `cp -a` (symlinks, modes, timestamps,
//!     hard links and — as root — ownership are preserved);
//!   * source tarballs are recognised and extracted with the `tar` crate plus
//!     in-process gzip/xz/bzip2/zstd decoders, with `tar --no-same-owner`
//!     semantics (ownership never restored, suid bits dropped, mtime kept)
//!     and GNU-tar-style path sanitisation.

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::io::{BufWriter, Read, Seek, Write};
use std::path::{Component, Path, PathBuf};
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

// ---- native HTTP download engine --------------------------------------------

const CONNECT_TIMEOUT: Duration = Duration::from_secs(20); // curl --connect-timeout 20
/// Per-read socket timeout: a mirror that stops sending for this long is
/// treated as dead instead of hanging the build forever.
const READ_TIMEOUT: Duration = Duration::from_secs(60);
/// curl --retry 3 for regular downloads (initial try + 3 retries).
const DOWNLOAD_ATTEMPTS: u32 = 4;
/// curl --retry 2 for optional sidecars.
const OPT_ATTEMPTS: u32 = 3;
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
        eprintln!("nawa: following redirect {current} -> {loc}");
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

/// Stream the response body into `dest` with an occasional MiB counter on
/// interactive terminals (curl used to show a progress meter there).
fn stream_body(resp: ureq::Response, dest: &Path) -> Result<u64> {
    let mut reader = resp.into_reader();
    let f = std::fs::File::create(dest).with_context(|| format!("create {}", dest.display()))?;
    let mut w = BufWriter::with_capacity(256 * 1024, f);
    let mut buf = [0u8; 128 * 1024];
    let mut total = 0u64;
    let tty = stderr_is_tty();
    let mut next_mark: u64 = 8 * 1024 * 1024;
    loop {
        let n = reader.read(&mut buf).context("read download body")?;
        if n == 0 {
            break;
        }
        w.write_all(&buf[..n]).with_context(|| format!("write {}", dest.display()))?;
        total += n as u64;
        if tty && total >= next_mark {
            eprint!("\r  nawa: {} MiB", total / (1024 * 1024));
            let _ = std::io::stderr().flush();
            next_mark += 8 * 1024 * 1024;
        }
    }
    w.flush().with_context(|| format!("flush {}", dest.display()))?;
    if tty {
        eprintln!("\r  nawa: {} MiB — {}", total / (1024 * 1024), dest.display());
    }
    Ok(total)
}

enum Outcome {
    Downloaded,
    /// HTTP 404-class answer: the file is simply not there.
    Absent,
}

fn single_transfer(agent: &ureq::Agent, url: &str, dest: &Path) -> Result<Outcome> {
    match http_get(agent, url) {
        Ok(resp) => stream_body(resp, dest).map(|_| Outcome::Downloaded),
        Err(e) => {
            // a non-transient 4xx means "not there" — optional sidecars ask
            // for that explicitly; 408/429 stay retryable, 5xx fail loudly
            if let Some(ureq::Error::Status(code, _)) = e.downcast_ref::<ureq::Error>() {
                if (400..500).contains(code) && !transient_status(*code) {
                    return Ok(Outcome::Absent);
                }
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
                eprintln!("nawa: {url}: attempt {attempt}/{attempts} failed ({e:#}); retrying in {wait:?}");
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
    let a = agent();
    match fetch_to_file(&a, url, dest, DOWNLOAD_ATTEMPTS)? {
        Outcome::Downloaded => Ok(()),
        Outcome::Absent => bail!("download failed: {url}"),
    }
}

/// Like `download`, but treats 404-class HTTP answers as "file absent" and
/// returns `false` instead of failing (used for optional `.sig` sidecars).
/// Server errors (5xx) are retried and still surface as an error, so a
/// broken mirror can no longer make a signature look unpublished.
pub fn download_opt(url: &str, dest: &Path) -> Result<bool> {
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
    let as_root = rustix::process::geteuid().is_root();
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
            eprintln!("nawa: warning: skipping special file {}", e.path().display());
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
                eprintln!("nawa: cached {} failed sha256 — re-downloading", dest.display());
            }
            None => return Ok(()), // unpinned cache hit (index: signature checked separately)
        }
    }
    let tmp = dest.with_extension("part");
    let mut last_err = None;
    for cand in candidate_urls_with(url, mirrors) {
        if cand != url {
            eprintln!("nawa: trying alternate mirror {cand}");
        }
        let res = download(&cand, &tmp).and_then(|_| match expect_sha256 {
            Some(e) => verify_sha256(&tmp, e),
            None => Ok(()),
        });
        match res {
            Ok(()) => {
                last_err = None;
                break;
            }
            Err(e) => {
                eprintln!("nawa: {e}");
                let _ = std::fs::remove_file(&tmp);
                last_err = Some(e);
            }
        }
    }
    if let Some(e) = last_err {
        return Err(e);
    }
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
    let f = std::fs::File::open(tarball).with_context(|| format!("open {}", tarball.display()))?;
    match fmt {
        SourceFormat::Tar => unpack_tar_stream(f, dest),
        SourceFormat::TarGz => unpack_tar_stream(flate2::read::GzDecoder::new(f), dest),
        SourceFormat::TarBz2 => unpack_tar_stream(bzip2::read::BzDecoder::new(f), dest),
        SourceFormat::TarXz => unpack_tar_stream(
            lzma_rust2::XzReader::new(std::io::BufReader::new(f), true),
            dest,
        ),
        SourceFormat::TarZst => unpack_tar_stream(zstd::stream::read::Decoder::new(f)?, dest),
    }
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
            eprintln!("nawa: warning: skipped unsafe path in source archive: {}", p.display());
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
                    eprintln!("skipping {url}: no route from this sandbox ({e:#})");
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
}
