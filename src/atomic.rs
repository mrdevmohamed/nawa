//! Durable atomic file publication for transaction metadata.
//!
//! File contents are synced before rename and the containing directory is
//! synced afterwards. A successful return includes both the data and the
//! published directory entry in the filesystem durability boundary.
//!
//! # Durability policy (Step 3)
//!
//! Use [`crate::atomic::write_atomic`] (this module's `write_atomic`)
//! when a crash must not leave a torn or missing file that a later start would
//! treat as authoritative:
//!
//! * `db::Db::{add,migrate_install_dates}` — `local/<name>/{desc,files,hashes}` (0o644)
//! * `sign::sign` — `index.toml.sig` (0o644)
//! * `repo::write_index_atomic` — `index.toml` (0o644)
//! * `groups::membership::save` — group membership records (0o644)
//! * `transaction::outbox::enqueue_transaction` / `plugins::events::store_failure` — plugin event queue (0o600, dir 0o700)
//! * `transaction::{journal,history}` — WAL/history records (already durable)
//!
//! Keep the ephemeral `write(tmp)+rename` + `remove_file(tmp)` pattern only for
//! throw-away staging that can be regenerated on the next run and is never treated
//! as authoritative on restart:
//!
//! * `<work>/stage/*`, `<builddir>/distfiles` download `.part` sidecars
//! * `<work>/src` / `<work>/pkg` staged package tree (`pkgdir`)
//! * `artifact` staging objects (content-addressed, verified before publish)
//! * `closure` layer staging dirs
//! * install `payload` staging (`install/payload.rs` `.archive-*.tmp`, `.signature-*.tmp`)
//! * `sandbox` scratch (`sb/layer-work`)
//!
//! The split is intentional: fsyncing every staged file would slow builds
//! without improving crash consistency, because the whole `<work>` tree is
//! wiped on the next build (`build::wipe_work`) and the archive is only
//! published after lint/slim verification.

use anyhow::{bail, Context, Result};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};



#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FailPoint { Wrote, Synced, Published }

#[cfg(test)]
thread_local! {
    static FAILPOINT: std::cell::Cell<Option<FailPoint>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
fn maybe_fail(point: FailPoint) -> Result<()> {
    let hit = FAILPOINT.with(|slot| {
        if slot.get() == Some(point) { slot.set(None); true } else { false }
    });
    if hit { bail!("injected atomic-write failure at {point:?}"); }
    Ok(())
}

/// Sync a directory so its current entries are durable.
pub(crate) fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)
        .with_context(|| format!("open directory {} for fsync", path.display()))?
        .sync_all()
        .with_context(|| format!("fsync directory {}", path.display()))
}

/// Create missing parent directories and persist their directory entries.
/// This recursive form also handles relative paths whose first component has
/// an empty parent (for example, `var/lib/nawa`) without attempting to fsync
/// an empty path.
fn create_dir_all_durable(path: &Path) -> Result<()> {
    match fs::metadata(path) {
        Ok(metadata) if metadata.is_dir() => return Ok(()),
        Ok(_) => bail!("{} exists but is not a directory", path.display()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error)
            .with_context(|| format!("inspect parent directory {}", path.display())),
    }

    let parent = path.parent()
        .filter(|candidate| !candidate.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    create_dir_all_durable(parent)?;

    match fs::create_dir(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let metadata = fs::metadata(path)
                .with_context(|| format!("inspect concurrently-created directory {}", path.display()))?;
            if !metadata.is_dir() {
                bail!("{} exists but is not a directory", path.display());
            }
        }
        Err(error) => return Err(error)
            .with_context(|| format!("create directory {}", path.display())),
    }

    sync_directory(parent)?;
    sync_directory(path)?;
    Ok(())
}

/// Atomically replace `path` with `bytes`. The temp file is exclusive and
/// receives its final permissions at creation time.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    create_dir_all_durable(parent)?;
    let name = path.file_name()
        .ok_or_else(|| anyhow::anyhow!("atomic-write destination has no file name"))?
        .to_string_lossy();

    for attempt in 0..32u32 {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH)
            .unwrap_or_default().as_nanos();
        let sequence = crate::unique::next();
        let temporary = parent.join(format!(
            ".{name}.{}.{}.{sequence}.{attempt}.tmp", std::process::id(), nanos
        ));
        let mut file = match OpenOptions::new().write(true).create_new(true).mode(mode).open(&temporary) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error)
                .with_context(|| format!("create temporary file {}", temporary.display())),
        };

        let written = (|| -> Result<()> {
            file.write_all(bytes)
                .with_context(|| format!("write temporary file {}", temporary.display()))?;
            #[cfg(test)]
            maybe_fail(FailPoint::Wrote)?;
            file.sync_all()
                .with_context(|| format!("fsync temporary file {}", temporary.display()))?;
            #[cfg(test)]
            maybe_fail(FailPoint::Synced)?;
            Ok(())
        })();
        if let Err(error) = written {
            drop(file);
            let _ = fs::remove_file(&temporary);
            return Err(error);
        }
        drop(file);

        if let Err(error) = fs::rename(&temporary, path) {
            let _ = fs::remove_file(&temporary);
            return Err(error).with_context(|| format!("publish atomic file {}", path.display()));
        }
        // Failure here means the new file is visible but its directory entry
        // was not confirmed durable. The transaction WAL resolves that case.
        #[cfg(test)]
        maybe_fail(FailPoint::Published)?;
        sync_directory(parent)?;
        return Ok(());
    }
    bail!("could not allocate a unique temporary file for {}", path.display())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_dir() -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "nawa-atomic-{}-{}", std::process::id(),
            crate::unique::next()
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn pre_rename_failure_keeps_old_file_and_cleans_temporary() {
        let dir = temp_dir();
        let target = dir.join("record.toml");
        fs::write(&target, b"old").unwrap();
        FAILPOINT.with(|slot| slot.set(Some(FailPoint::Synced)));
        assert!(write_atomic(&target, b"new", 0o600).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"old");
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn write_failure_before_file_sync_keeps_old_file_and_cleans_temporary() {
        let dir = temp_dir();
        let target = dir.join("record.toml");
        fs::write(&target, b"old").unwrap();
        FAILPOINT.with(|slot| slot.set(Some(FailPoint::Wrote)));
        assert!(write_atomic(&target, b"new", 0o600).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"old");
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn write_atomic_durably_creates_missing_nested_parents() {
        let dir = temp_dir();
        let target = dir.join("var/lib/nawa/history/last-id");
        write_atomic(&target, b"42", 0o600).unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"42");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn post_rename_failure_leaves_a_complete_new_file() {
        let dir = temp_dir();
        let target = dir.join("record.toml");
        fs::write(&target, b"old").unwrap();
        FAILPOINT.with(|slot| slot.set(Some(FailPoint::Published)));
        assert!(write_atomic(&target, b"new-complete-record", 0o600).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"new-complete-record");
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
        fs::remove_dir_all(dir).unwrap();
    }
}
