//! Native HTTP download engine (replaces shelling out to curl).

use anyhow::{bail, Context, Result};

use std::io::{BufWriter, Read, Write};
use std::path::Path;
use std::time::Duration;

use std::sync::atomic::{AtomicBool, Ordering};

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
pub(crate) fn check_offline(url: &str, offline: bool) -> Result<()> {
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

pub(crate) fn is_retryable(e: &anyhow::Error) -> bool {
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
pub(crate) fn redirect_target(current: &url::Url, loc: &str) -> Result<url::Url> {
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
pub(crate) fn progress_line(label: &str, done: u64, total: Option<u64>) -> String {
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
pub(crate) fn progress_label(dest: &Path) -> String {
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

pub(crate) enum Outcome {
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
