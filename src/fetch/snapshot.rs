//! Verified snapshots: private, cleanup-on-drop copies whose bytes were
//! hashed while copying.

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::fs::{File, OpenOptions};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// Private, cleanup-on-drop copy of a file whose bytes were hashed while copying.
/// Callers must use this path for all subsequent parsing or extraction.
pub(crate) struct VerifiedSnapshot {
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
pub(crate) fn snapshot_file(
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
        let sequence = crate::unique::next();
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
