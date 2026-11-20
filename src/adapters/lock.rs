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
//!   The marker is an *optimisation for read-only nested calls only*: it is
//!   inherited process state and forgeable, so it must never authorize a
//!   mutation. An exclusive acquisition fails closed when the marker is
//!   present — a nested package-mutating command has no lock of its own (the
//!   parent's flock belongs to the parent process) and must not change the
//!   package set from inside a hook or trigger script.
//! * The persistent migration mutation gate (design §10.3) is checked on
//!   every exclusive acquisition, *before* the marker is honored: while a
//!   migration record under the selected root is in a gate-keeping state,
//!   package-mutating commands stop before changing the package set. The
//!   check runs under the exclusive lock (or under the parent's already-held
//!   lock for the nested-exclusive case), so the marker can never bypass it;
//!   read-only commands remain allowed and are not gate-checked (the parent
//!   command was already checked at its own exclusive acquisition).
//!
//! Callers lock once, at the top-level command (see `main.rs`); the library
//! functions (`install_archive`, `upgrade`, ...) never lock, because
//! `upgrade` calls `install_archive` and flock is not re-entrant across two
//! open file descriptions.

use anyhow::{bail, Context, Result};
use rustix::fd::AsFd;
use rustix::fs::{flock, FlockOperation};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::adapters::migration_record as migrate;
use crate::adapters::pathutil::create_managed_dirs;

/// Set in the environment of hooks so nested nawa *read-only* calls skip
/// locking. It is process state inherited by children and forgeable at
/// will: it must never authorize a package mutation, and the migration
/// mutation gate (design §10.3) is checked before it can influence any
/// exclusive acquisition.
pub(crate) const HELD_ENV: &str = "NAWA_LOCK_HELD";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    Shared,
    Exclusive,
}

/// Held for as long as the value lives; dropping it closes the file, which
/// releases the lock.
#[derive(Debug)]
pub(crate) struct DbLock {
    _file: File,
}

pub(crate) fn lock_path(root: &Path) -> PathBuf {
    root.join("var/lib/nawa/lock")
}

/// The lock-defined in-process capability that authorizes the migration core
/// to hold the exclusive package-database lock *while a migration gate is
/// active* (§10.3, §13: "the only bypass is the private, typed in-process
/// permit held by the active migration coordinator"). Recovery needs this:
/// the gate is already durable when a killed coordinator's record is
/// inspected, so an ordinary [`acquire`] would refuse — correctly — and the
/// migration recovery use case must be able to take the lock past its own
/// gate to reconcile and conclude the record.
///
/// Authority is structural and in-process only:
///
/// * the seal is private to this module, so no value can be constructed
///   outside `lock` except through [`migration_authority`];
/// * there is no `serde`, `Display`, `FromStr`, environment spelling, CLI
///   flag or plugin capability that can carry one — a nested `nawa` process
///   gets no authority and is still refused by the gate;
/// * the layering check (`scripts/check-layering.py`) rejects any call to
///   [`migration_authority`] from outside the migration feature subtree, so
///   an ordinary package command cannot mint one either.
///
/// It is deliberately *not* `Clone`/`Copy`: minting is one-shot at the point
/// where the migration use case needs to cross the gate.
#[derive(Debug)]
pub(crate) struct MigrationAuthority {
    _seal: AuthoritySeal,
}

#[derive(Debug)]
struct AuthoritySeal;

/// Mint the gate-crossing capability for the migration recovery use case.
/// Only compiled-in migration feature code may call this (enforced by the
/// layering check); no external input can reach it.
pub(crate) fn migration_authority() -> MigrationAuthority {
    MigrationAuthority {
        _seal: AuthoritySeal,
    }
}

/// Acquire the exclusive database lock while a migration gate is active,
/// presenting [`MigrationAuthority`] as proof that the caller is the
/// migration core reconciling its own gate. This is the *only* path that
/// skips [`check_migration_gate`]; every ordinary exclusive acquisition
/// still goes through [`acquire`] and is gate-checked.
///
/// The `NAWA_LOCK_HELD` marker is still refused: a hook-spawned nested
/// `nawa` is never a migration coordinator, and the marker can be forged,
/// so it must not become a gate bypass (§10.3, §13).
pub(crate) fn acquire_for_migration(
    root: &Path,
    _authority: MigrationAuthority,
) -> Result<Option<DbLock>> {
    reject_nested_marker(std::env::var_os(HELD_ENV).is_some())?;
    acquire_unchecked(root, Mode::Exclusive)
}

/// Reject a nested-process marker before a migration authority can cross the
/// gate. Kept as a pure input check so tests never mutate process-global
/// environment while other Rust tests are running in parallel.
pub(crate) fn reject_nested_marker(marker_present: bool) -> Result<()> {
    if marker_present {
        bail!(
            "a nested nawa process ({HELD_ENV} is set) cannot cross the migration \
             mutation gate: recovery runs in the coordinator process only"
        );
    }
    Ok(())
}

/// The gate-free inner acquisition used only by `acquire_for_migration`.
/// `acquire` itself keeps its gate check on the ordinary path.
fn acquire_unchecked(root: &Path, mode: Mode) -> Result<Option<DbLock>> {
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
                    crate::nawa_info_stderr!(
                        "waiting for the package database lock ({}){}",
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

/// The persistent migration mutation gate (design §10.3): while a migration
/// record under `root` is in a gate-keeping state, every Nawa-owned
/// package-mutating command must stop *before changing the package set*.
///
/// Called on the exclusive paths only, and always under a lock that covers
/// the state: under this process's own exclusive flock, or — for a nested
/// hook-spawned process — before the `NAWA_LOCK_HELD` marker is honored,
/// where the parent's already-held lock covers the window (§10.3: the gate
/// is checked before the marker, so the marker can never bypass it; the
/// marker only ever made read-only nesting possible, and the parent command
/// was gate-checked at its own exclusive acquisition).
///
/// Read-only paths never run this check, on purpose: read-only inspection
/// remains allowed while a migration is pending (§10.3), and readers are
/// best-effort here — even a corrupt migration record must not take read
/// commands down. Mutations are strict: if the gate state cannot be read,
/// they fail closed rather than risk running under a pending migration.
///
/// A parent command holding the lock cannot race a gate into existence
/// under it: beginning preparation (`BeginPreparation`, §10.4) requires
/// the coordinator to hold the exclusive package database lock itself, so
/// no gate can appear while any command holds the lock.
fn check_migration_gate(root: &Path) -> Result<()> {
    let gates = match migrate::active_gates(root) {
        Ok(gates) => gates,
        Err(error) => bail!(
            "cannot check the persistent migration mutation gate: {error:#} — \
             refusing to change the package set fail-closed (§10.3)"
        ),
    };
    if gates.is_empty() {
        return Ok(());
    }
    let list = gates
        .iter()
        .map(|gate| format!("{} is {}", gate.id, gate.state.as_str()))
        .collect::<Vec<_>>()
        .join("; ");
    bail!(
        "a system migration holds the package mutation gate: {list} — \
         package-mutating commands are blocked until the migration completes \
         or is recovered (§10.3; migration records: {})",
        migrate::migrations_dir(root).display()
    )
}

/// Lock the database of `root`, waiting if another nawa holds it.
///
/// Returns `Ok(None)` when no lock is needed or possible:
/// * a *read-only* (shared) acquisition inside a hook of a nawa that already
///   holds the lock (`NAWA_LOCK_HELD`) — the parent's lock covers the read
///   window; the child's own shared lock would deadlock against the parent's
///   exclusive lock;
/// * a *shared* lock on a root that has no nawa state yet, or that is
///   read-only for us (readers are best-effort, writers are strict).
///
/// An *exclusive* acquisition inside such a hook fails closed: the marker
/// proves nothing about lock ownership (it is inherited process state, and
/// the parent's flock does not extend to the child), so a nested
/// package-mutating command must not run without its own lock.
pub(crate) fn acquire(root: &Path, mode: Mode) -> Result<Option<DbLock>> {
    if std::env::var_os(HELD_ENV).is_some() {
        match mode {
            Mode::Shared => return Ok(None),
            Mode::Exclusive => {
                // §10.3: the gate is checked before the marker is honored —
                // the marker can never authorize a mutation or bypass the
                // gate. Both outcomes below refuse; when a migration is
                // pending, its error is the more precise one.
                check_migration_gate(root)?;
                bail!(
                    "this nawa runs inside a package hook or transaction trigger \
                     ({HELD_ENV} is set): the parent command owns the package \
                     database lock and this process holds none, so package-mutating \
                     commands must not run here"
                );
            }
        }
    }
    let guard = acquire_unchecked(root, mode)?;
    if mode == Mode::Exclusive && guard.is_some() {
        // under our own exclusive flock (§10.3: the gate is checked under
        // the exclusive package database lock). If not acquired, no lock
        // was needed/possible; callers that need ownership handle `None`.
        check_migration_gate(root)?;
    }
    Ok(guard)
}

#[derive(Debug)]
pub(crate) enum Attempt {
    Locked(DbLock),
    /// Another process holds a conflicting lock.
    Busy,
    /// Shared lock requested where none can/needs to be taken.
    NotNeeded,
}

/// One non-blocking attempt.
pub(crate) fn try_acquire(root: &Path, mode: Mode) -> Result<Attempt> {
    let path = lock_path(root);
    let file = match mode {
        Mode::Exclusive => {
            if let Some(dir) = path.parent() {
                create_managed_dirs(dir).with_context(|| format!("create {}", dir.display()))?;
            }
            open_rw(&path).with_context(|| {
                format!("cannot open the package database lock {}", path.display())
            })?
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
        Mode::Shared => FlockOperation::NonBlockingLockShared,
        Mode::Exclusive => FlockOperation::NonBlockingLockExclusive,
    };
    loop {
        // rustix::fs::flock is a thin, safe wrapper around flock(2); only
        // EINTR needs manual handling (rustix reports it like any error)
        match flock(file.as_fd(), op) {
            Ok(()) => break,
            Err(e) => match std::io::Error::from(e).kind() {
                std::io::ErrorKind::Interrupted => continue,
                std::io::ErrorKind::WouldBlock => return Ok(Attempt::Busy),
                kind => {
                    let err = std::io::Error::from(e);
                    bail!("flock on the database lock failed: {err} (kind {kind:?})")
                }
            },
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
        assert!(matches!(
            try_acquire(&root, Mode::Exclusive).unwrap(),
            Attempt::Busy
        ));
        assert!(matches!(
            try_acquire(&root, Mode::Shared).unwrap(),
            Attempt::Busy
        ));
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
        assert!(matches!(
            try_acquire(&root, Mode::Exclusive).unwrap(),
            Attempt::Busy
        ));
        drop(a);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn shared_on_a_root_without_nawa_state_needs_no_lock_and_creates_nothing() {
        let root = tmp("none");
        assert!(matches!(
            try_acquire(&root, Mode::Shared).unwrap(),
            Attempt::NotNeeded
        ));
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

    // ---- the migration mutation gate (design §10.3) -------------------------

    /// Seed a migration record the way the store writes it (§10.3 record
    /// format); written by hand here so the gate is exercised against raw
    /// durable state, not through the store's own API.
    fn seed_gate(root: &Path, id: &str, state: &str) -> PathBuf {
        let dir = root.join("var/lib/nawa/migrations");
        create_managed_dirs(&dir).unwrap();
        let path = dir.join(format!("{id}.toml"));
        crate::adapters::atomic::write_atomic(
            &path,
            format!(
                "schema = 1\nid = \"{id}\"\ndefinition_version = 1\n\
                 from = {{ domain = \"init-system\", identity = \"runit\" }}\n\
                 to = {{ domain = \"init-system\", identity = \"systemd\" }}\n\
                 handler = \"faseela.init.runit_to_systemd\"\nstate = \"{state}\"\n"
            )
            .as_bytes(),
            0o600,
        )
        .unwrap();
        path
    }

    #[test]
    fn an_active_migration_blocks_the_exclusive_gate_until_the_record_goes_away() {
        let root = tmp("gate-block");
        let path = seed_gate(&root, "faseela-runit-to-systemd-v1", "preparing");

        let e = check_migration_gate(&root).unwrap_err();
        let msg = format!("{e:#}");
        assert!(
            msg.contains("faseela-runit-to-systemd-v1 is preparing"),
            "{msg}"
        );
        assert!(msg.contains("package mutation gate"), "{msg}");
        assert!(msg.contains("var/lib/nawa/migrations"), "{msg}");

        // the same refusal on the real exclusive-acquisition path
        let e = acquire(&root, Mode::Exclusive).unwrap_err();
        assert!(
            format!("{e:#}").contains("faseela-runit-to-systemd-v1 is preparing"),
            "{e:#}"
        );

        // once the record is gone the mutation proceeds again
        std::fs::remove_file(&path).unwrap();
        let _l = acquire(&root, Mode::Exclusive).unwrap().unwrap();
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn planned_and_terminal_records_keep_the_gate_open() {
        // §10.3: `planned` was approved but the gate is taken only by
        // BeginPreparation; verified completion and a completed, verified
        // rollback clear the gate
        for state in ["planned", "completed", "failed"] {
            let root = tmp("gate-open");
            seed_gate(&root, "faseela-runit-to-systemd-v1", state);
            check_migration_gate(&root).unwrap();
            let _l = acquire(&root, Mode::Exclusive).unwrap().unwrap();
            let _ = std::fs::remove_dir_all(&root);
        }
    }

    #[test]
    fn the_gate_never_blocks_readers() {
        // §10.3: read-only inspection stays available while a migration is
        // pending, and readers are best-effort — even a corrupt record must
        // not take read commands down
        let root = tmp("gate-read");
        let path = seed_gate(&root, "faseela-runit-to-systemd-v1", "preparing");
        let _l = acquire(&root, Mode::Shared).unwrap().unwrap();
        std::fs::write(&path, "not [ valid toml").unwrap();
        let _l2 = acquire(&root, Mode::Shared).unwrap().unwrap();
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_corrupt_record_fails_the_exclusive_gate_closed() {
        // mutations are strict: unreadable gate state refuses the mutation
        // instead of risking a package change under a pending migration
        let root = tmp("gate-corrupt");
        let path = seed_gate(&root, "faseela-runit-to-systemd-v1", "preparing");
        std::fs::write(&path, "not [ valid toml").unwrap();
        let e = acquire(&root, Mode::Exclusive).unwrap_err();
        let msg = format!("{e:#}");
        assert!(
            msg.contains("cannot check the persistent migration mutation gate"),
            "{msg}"
        );
        // the same for an unreadable record entry (a directory in its place)
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        let e = acquire(&root, Mode::Exclusive).unwrap_err();
        assert!(
            format!("{e:#}").contains("cannot check the persistent migration mutation gate"),
            "{e:#}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    // ---- concurrency -------------------------------------------------------

    /// Threads holding their **own open file description** contend for the
    /// same kernel lock, exactly like separate processes. `try_acquire` is
    /// non-blocking, so "excluded until released" is observable directly.
    #[test]
    fn four_threads_excluded_until_exclusive_release_then_all_proceed() {
        let root = std::env::temp_dir().join(format!("nawa-lock-threads-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("var/lib/nawa")).unwrap();

        let first = locked(try_acquire(&root, Mode::Exclusive).unwrap());

        std::thread::scope(|s| {
            // 3 readers + 1 writer: all must see Busy while the holder lives
            let mut handles = Vec::new();
            for mode in [Mode::Shared, Mode::Shared, Mode::Shared, Mode::Exclusive] {
                let root = root.clone();
                handles.push(s.spawn(move || match try_acquire(&root, mode) {
                    Ok(Attempt::Busy) => true,
                    other => panic!("expected Busy under contention, got {other:?}"),
                }));
            }
            for h in handles {
                assert!(h.join().unwrap(), "contention while the lock is held");
            }
            // double-check the total observation count
        });

        // after release every thread must win its own lock (shared first,
        // so the final exclusive attempt is the release point)
        drop(first);
        std::thread::scope(|_| {
            let mut locks = Vec::new();
            for _ in 0..3 {
                locks.push(locked(try_acquire(&root, Mode::Shared).unwrap()));
            }
            assert!(matches!(
                try_acquire(&root, Mode::Exclusive).unwrap(),
                Attempt::Busy
            ));
            drop(locks);
        });
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A failed (Busy) attempt must leave the lock file openable and the
    /// final state clean — no stray fd leaks that could hold the lock.
    #[test]
    fn busy_attempt_does_not_leak_a_lock() {
        let root = tmp("leak");
        let _holder = locked(try_acquire(&root, Mode::Exclusive).unwrap());
        assert!(matches!(
            try_acquire(&root, Mode::Shared).unwrap(),
            Attempt::Busy
        ));
        // repeated busy attempts stay busy (no fd was left locked behind)
        assert!(matches!(
            try_acquire(&root, Mode::Shared).unwrap(),
            Attempt::Busy
        ));
        assert!(matches!(
            try_acquire(&root, Mode::Exclusive).unwrap(),
            Attempt::Busy
        ));
        let _ = std::fs::remove_dir_all(&root);
    }
}
