//! Database lock: serialises every nawa process that touches
//! `<root>/var/lib/nawa`.
//!
//! * One lock file per root: `<root>/var/lib/nawa/lock`, taken with
//!   `flock(2)`. The kernel drops it when the process exits, however it
//!   exits (crash, SIGKILL, power-off), so there are no stale locks.
//! * Writers (`install`, `remove`, `upgrade`, `rollback`) take it
//!   **exclusively** and hold it for the whole command, including hooks.
//!   Readers (`query`, `files`, `owner`, `deps`) take it **shared**, so they
//!   never see a half-applied transaction but do not block each other.
//! * A second nawa waits (and says who it is waiting for). Set
//!   `NAWA_LOCK_TIMEOUT=<seconds>` to give up instead of waiting forever.
//! * Hooks run by a package are children of the nawa that holds the lock and
//!   get `NAWA_LOCK_HELD=1`, so a hook that itself calls nawa cannot deadlock.
//!
//! Callers lock once, at the top-level command (see `main.rs`); the library
//! functions (`install_archive`, `upgrade`, ...) never lock, because
//! `upgrade` calls `install_archive` and flock is not re-entrant across two
//! open file descriptions.

use anyhow::{bail, Context, Result};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::raw::c_int;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

extern "C" {
    // part of libc, which std already links — no extra crate needed (the
    // bootstrap builds nawa offline, so new dependencies are expensive)
    fn flock(fd: c_int, operation: c_int) -> c_int;
}
const LOCK_SH: c_int = 1;
const LOCK_EX: c_int = 2;
const LOCK_NB: c_int = 4;

/// Set in the environment of hooks so nested nawa calls skip locking.
pub const HELD_ENV: &str = "NAWA_LOCK_HELD";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Shared,
    Exclusive,
}

/// Held for as long as the value lives; dropping it closes the file, which
/// releases the lock.
#[derive(Debug)]
pub struct DbLock {
    _file: File,
}

pub fn lock_path(root: &Path) -> PathBuf {
    root.join("var/lib/nawa/lock")
}

/// Lock the database of `root`, waiting if another nawa holds it.
///
/// Returns `Ok(None)` when no lock is needed or possible:
/// * inside a hook of a nawa that already holds the lock (`NAWA_LOCK_HELD`);
/// * a *shared* lock on a root that has no nawa state yet, or that is
///   read-only for us (readers are best-effort, writers are strict).
pub fn acquire(root: &Path, mode: Mode) -> Result<Option<DbLock>> {
    if std::env::var_os(HELD_ENV).is_some() {
        return Ok(None);
    }
    let timeout = std::env::var("NAWA_LOCK_TIMEOUT")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|&s| s > 0)
        .map(Duration::from_secs);
    let started = Instant::now();
    let mut announced = false;
    loop {
        match try_acquire(root, mode)? {
            Attempt::Locked(l) => return Ok(Some(l)),
            Attempt::NotNeeded => return Ok(None),
            Attempt::Busy => {
                if !announced {
                    announced = true;
                    eprintln!(
                        "nawa: waiting for the package database lock ({}){}",
                        lock_path(root).display(),
                        holder_hint(root)
                    );
                }
                if let Some(t) = timeout {
                    if started.elapsed() >= t {
                        bail!(
                            "timed out after {}s waiting for the database lock{}",
                            t.as_secs(),
                            holder_hint(root)
                        );
                    }
                }
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    }
}

#[derive(Debug)]
pub enum Attempt {
    Locked(DbLock),
    /// Another process holds a conflicting lock.
    Busy,
    /// Shared lock requested where none can/needs to be taken.
    NotNeeded,
}

/// One non-blocking attempt.
pub fn try_acquire(root: &Path, mode: Mode) -> Result<Attempt> {
    let path = lock_path(root);
    let file = match mode {
        Mode::Exclusive => {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)
                    .with_context(|| format!("create {}", dir.display()))?;
            }
            open_rw(&path).with_context(|| format!("open lock file {}", path.display()))?
        }
        Mode::Shared => {
            // readers never create nawa state; a missing/unwritable lock
            // location just means nobody can be writing there
            match path.parent() {
                Some(dir) if dir.is_dir() => {}
                _ => return Ok(Attempt::NotNeeded),
            }
            match open_rw(&path).or_else(|_| File::open(&path)) {
                Ok(f) => f,
                Err(_) => return Ok(Attempt::NotNeeded),
            }
        }
    };

    let op = match mode {
        Mode::Shared => LOCK_SH,
        Mode::Exclusive => LOCK_EX,
    } | LOCK_NB;
    loop {
        // SAFETY: plain syscall wrapper on a file descriptor we own.
        let rc = unsafe { flock(file.as_raw_fd(), op) };
        if rc == 0 {
            break;
        }
        let err = std::io::Error::last_os_error();
        match err.kind() {
            std::io::ErrorKind::Interrupted => continue,
            std::io::ErrorKind::WouldBlock => return Ok(Attempt::Busy),
            _ => return Err(err).context("flock on the database lock failed"),
        }
    }

    if mode == Mode::Exclusive {
        // informational only (who to blame in the "waiting" message)
        let mut f = &file;
        let _ = file.set_len(0);
        let _ = writeln!(f, "{}", std::process::id());
    }
    Ok(Attempt::Locked(DbLock { _file: file }))
}

fn open_rw(path: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
}

fn holder_hint(root: &Path) -> String {
    match std::fs::read_to_string(lock_path(root)) {
        Ok(s) if s.trim().parse::<u32>().is_ok() => format!(" — held by pid {}", s.trim()),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("nawa-lock-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn locked(a: Attempt) -> DbLock {
        match a {
            Attempt::Locked(l) => l,
            other => panic!("expected the lock, got {other:?}"),
        }
    }

    #[test]
    fn exclusive_excludes_everything_until_dropped() {
        let root = tmp("ex");
        let first = locked(try_acquire(&root, Mode::Exclusive).unwrap());
        assert!(matches!(try_acquire(&root, Mode::Exclusive).unwrap(), Attempt::Busy));
        assert!(matches!(try_acquire(&root, Mode::Shared).unwrap(), Attempt::Busy));
        drop(first);
        let _again = locked(try_acquire(&root, Mode::Exclusive).unwrap());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn shared_locks_coexist_but_block_writers() {
        let root = tmp("sh");
        std::fs::create_dir_all(root.join("var/lib/nawa")).unwrap();
        let a = locked(try_acquire(&root, Mode::Shared).unwrap());
        let _b = locked(try_acquire(&root, Mode::Shared).unwrap());
        assert!(matches!(try_acquire(&root, Mode::Exclusive).unwrap(), Attempt::Busy));
        drop(a);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn shared_on_a_root_without_nawa_state_needs_no_lock_and_creates_nothing() {
        let root = tmp("none");
        assert!(matches!(try_acquire(&root, Mode::Shared).unwrap(), Attempt::NotNeeded));
        assert!(!root.join("var").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn exclusive_records_the_holder_pid() {
        let root = tmp("pid");
        let _l = locked(try_acquire(&root, Mode::Exclusive).unwrap());
        let s = std::fs::read_to_string(lock_path(&root)).unwrap();
        assert_eq!(s.trim(), std::process::id().to_string());
        let _ = std::fs::remove_dir_all(&root);
    }
}
