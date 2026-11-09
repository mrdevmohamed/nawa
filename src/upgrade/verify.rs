//! Verification helpers.

// Note: trim unused imports in later pass
#[allow(unused_imports)]
use anyhow::{bail, Context, Result};
use std::path::Path;

use crate::repo;
use crate::sign;

pub(crate) fn require_trust_for_remote(root: &Path, url: &str, allow_unsigned: bool) -> Result<()> {
    if allow_unsigned {
        crate::nawa_warn!("--allow-unsigned: {url} is NOT authenticated");
        return Ok(());
    }
    if sign::trusted_keys(root)?.is_empty() {
        bail!(
            "refusing remote repository {url}: no trusted keys configured in {}\n\
             (add the repository's public key with `nawa trust <key.pub>`, or pass --allow-unsigned to accept it unauthenticated)",
            sign::trust_dir(root).display()
        );
    }
    if url.starts_with("http://") {
        crate::nawa_warn!("{url} uses plain http — authenticity relies on signatures only");
    }
    Ok(())
}

/// Rollback / freeze protection for the (signed) index timestamp.
/// Remote repos remember the newest `generated` value seen in the cache dir
/// and refuse anything older; a very old index only produces a warning.
pub(crate) fn check_index_freshness(
    repo_dir: &Path,
    index: &repo::Index,
    remote: bool,
    force: bool,
    authenticated: bool,
) -> Result<()> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let gen = match index.generated {
        Some(g) => g,
        None => {
            crate::nawa_note_stderr!("index has no timestamp — no rollback protection");
            return Ok(());
        }
    };
    // A `generated` value ahead of this machine's clock is not an error: an
    // unsynchronised clock (no RTC, fresh VM or chroot) must not make every
    // freshly published index unusable. It only must never become the
    // "newest seen" marker, or one bad publisher/system clock would block
    // every later genuine index.
    let future = gen > now;
    if future {
        eprintln!(
            "nawa: warning: repository index timestamp is ahead of this system's clock; \
             check the system clock (the index is used, but not recorded as newest seen)"
        );
    }
    if remote {
        let seen_path = repo_dir.join("index.seen");
        if let Ok(raw) = std::fs::read_to_string(&seen_path) {
            if let Ok(seen) = raw.trim().parse::<u64>() {
                if gen < seen {
                    bail!(
                        "repository index is older than one already seen (possible rollback attack)"
                    );
                }
            }
        }
        // Best effort, as before: failing to record the marker must not fail
        // the upgrade. The write is atomic so a crash cannot leave a torn file.
        if authenticated && !future {
            let part = seen_path.with_extension("seen.part");
            let recorded = std::fs::write(&part, gen.to_string())
                .and_then(|()| std::fs::rename(&part, &seen_path));
            if recorded.is_err() {
                let _ = std::fs::remove_file(&part);
            }
        }
    }
    if let Some(exp) = index.expires {
        if now > exp {
            if remote && !force {
                bail!(
                    "repository index expired {} days ago — the mirror may be frozen or stale\n(use --force to upgrade anyway)",
                    (now - exp) / 86400
                );
            }
            crate::nawa_warn!("repository index has expired");
        }
    }
    if now > gen && now - gen > 30 * 24 * 3600 {
        crate::nawa_warn!(
            "repository index is {} days old",
            (now - gen) / 86400
        );
    }
    Ok(())
}

/// The `.PKGINFO` inside the archive must agree with the index entry.
pub(crate) fn check_archive_matches_index(
    archive: &Path,
    re: &repo::RepoEntry,
    strict_v2: bool,
) -> Result<()> {
    let raw = crate::archive::read_pkginfo(archive)?;
    let info = crate::archive::parse_pkginfo(&raw)?;
    let get = |k: &str| info.get(k).cloned().unwrap_or_default();
    let rel: u32 = match info.get("pkgrel") {
        Some(raw) => match raw.parse::<u32>() {
            Ok(value) if !strict_v2 || value > 0 => value,
            Ok(_) => bail!("archive {} has invalid pkgrel 0 in .PKGINFO", re.file),
            Err(_) if !strict_v2 => 1,
            Err(_) => bail!("archive {} has invalid pkgrel {:?} in .PKGINFO", re.file, raw),
        },
        None if strict_v2 => bail!("archive {} is missing pkgrel in .PKGINFO", re.file),
        None => 1,
    };
    let arch = get("arch");
    let host = std::env::var("NAWA_ARCH").unwrap_or_else(|_| std::env::consts::ARCH.to_string());
    if !crate::commands::arch_fits(&arch) {
        bail!("archive {} is built for {arch}, this system is {host}", re.file);
    }
    if get("pkgname") != re.name || get("pkgver") != re.version || rel != re.rel || arch != re.arch {
        bail!(
            "archive {} metadata does not match the index (archive: {}-{}-{} arch {}; index: {}-{}-{} arch {})",
            re.file,
            get("pkgname"),
            get("pkgver"),
            rel,
            arch,
            re.name,
            re.version,
            re.rel,
            re.arch
        );
    }
    Ok(())
}

/// A malicious or compromised index must not be able to escape the cache
/// directory through crafted archive paths.
pub(crate) fn validate_entry_filename(file: &str) -> Result<()> {
    if file.is_empty()
        || file.contains('/')
        || file.contains('\\')
        || file.contains("..")
        || file.starts_with('.')
    {
        bail!("index entry has an unsafe archive path: `{file}`");
    }
    Ok(())
}
