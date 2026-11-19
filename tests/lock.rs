//! The database lock across real processes. Uses util-linux `flock(1)` to
//! play "another nawa holding the lock"; the tests are skipped when it is
//! not installed.

use std::fs;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

mod support;

fn nawa() -> Command {
    Command::new(PathBuf::from(env!("CARGO_BIN_EXE_nawa")))
}

fn have_flock() -> bool {
    Command::new("flock")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Hold the exclusive lock of `root` for `secs` seconds from another process.
///
/// The returned `Child` must be the *only* process holding the lock file
/// descriptor: `flock(1) <file> sleep N` would leave `sleep` as a child that
/// inherits the descriptor, so killing the `flock` process would not release
/// the lock. Here the shell opens fd 9, locks it, then `exec`s `sleep`, so the
/// `Child` itself owns the lock and killing it releases the lock.
fn hold(root: &std::path::Path, secs: u32) -> Child {
    support::create_state_dirs(&root.join("var/lib/nawa"));
    let c = Command::new("sh")
        .args([
            "-c",
            "exec 9>>\"$1\"; flock 9 || exit 1; exec sleep \"$2\"",
            "sh",
            root.join("var/lib/nawa/lock").to_str().unwrap(),
            &secs.to_string(),
        ])
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(400)); // let it take the lock
    c
}

fn scratch(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("nawa-locktest-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn writers_and_readers_wait_for_a_held_lock_and_time_out() {
    if !have_flock() {
        eprintln!("skipped: flock(1) not available");
        return;
    }
    let root = scratch("timeout");
    let mut holder = hold(&root, 8);

    for args in [vec!["remove", "nothing"], vec!["query"]] {
        let t = Instant::now();
        let out = nawa()
            .env("NAWA_LOCK_TIMEOUT", "1")
            .args(["--root", root.to_str().unwrap()])
            .args(&args)
            .output()
            .unwrap();
        assert!(
            !out.status.success(),
            "{args:?} must not run while the lock is held"
        );
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(
            err.contains("waiting for the package database lock"),
            "{err}"
        );
        assert!(err.contains("timed out"), "{err}");
        assert!(t.elapsed() < Duration::from_secs(6), "gave up too late");
    }
    let _ = holder.kill();
    let _ = holder.wait();
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn lock_is_released_when_the_holder_dies() {
    if !have_flock() {
        eprintln!("skipped: flock(1) not available");
        return;
    }
    let root = scratch("release");
    let mut holder = hold(&root, 30);
    let _ = holder.kill(); // SIGKILL: no chance to clean up
    let _ = holder.wait();

    let out = nawa()
        .env("NAWA_LOCK_TIMEOUT", "3")
        .args(["--root", root.to_str().unwrap(), "query"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn waiter_proceeds_as_soon_as_the_holder_finishes() {
    if !have_flock() {
        eprintln!("skipped: flock(1) not available");
        return;
    }
    let root = scratch("handoff");
    let mut holder = hold(&root, 2);
    let t = Instant::now();
    let out = nawa()
        .env("NAWA_LOCK_TIMEOUT", "10")
        .args(["--root", root.to_str().unwrap(), "query"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(t.elapsed() < Duration::from_secs(8));
    let _ = holder.wait();
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn nested_nawa_inside_a_hook_does_not_deadlock() {
    if !have_flock() {
        eprintln!("skipped: flock(1) not available");
        return;
    }
    let root = scratch("held");
    let mut holder = hold(&root, 8);
    // a nawa started with NAWA_LOCK_HELD (as hooks are) skips locking for
    // read-only commands: its own shared lock would deadlock against the
    // parent's exclusive one
    let out = nawa()
        .env("NAWA_LOCK_HELD", "1")
        .env("NAWA_LOCK_TIMEOUT", "1")
        .args(["--root", root.to_str().unwrap(), "query"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let _ = holder.kill();
    let _ = holder.wait();
    let _ = fs::remove_dir_all(&root);
}

/// `NAWA_LOCK_HELD` is inherited process state and forgeable at will: it
/// must never let a nested (hook- or trigger-spawned) invocation change the
/// package set. Mutating commands fail closed, even when someone sets the
/// marker by hand — the parent's flock does not extend to this process.
#[test]
fn forged_held_marker_cannot_authorize_a_mutating_command() {
    let root = scratch("forged");
    for args in [
        vec!["remove", "curl"],      // whole-command exclusive lock in main
        vec!["autoremove", "--yes"], // exclusive
        vec!["install", "/tmp/nonexistent.nawa.tar.zst"], // all-archive install: exclusive
    ] {
        let out = nawa()
            .env("NAWA_LOCK_HELD", "1")
            .env("NAWA_LOCK_TIMEOUT", "1")
            .args(["--root", root.to_str().unwrap()])
            .args(&args)
            .output()
            .unwrap();
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(
            !out.status.success(),
            "{args:?} must fail closed; got: {err}"
        );
        assert!(
            err.contains("package hook or transaction trigger"),
            "{args:?} must name the hook context: {err}"
        );
        assert!(
            err.contains("holds none"),
            "{args:?} must explain that no lock is held: {err}"
        );
    }
    // nothing was installed, nothing was recorded: the database stays empty
    assert!(
        !root.join("var/lib/nawa/local").exists(),
        "no package state may be created"
    );
    let _ = fs::remove_dir_all(&root);
}

/// The same mutating command *without* the marker runs normally: the
/// fail-closed behavior is the marker's doing, not the command's.
#[test]
fn mutating_command_without_the_marker_uses_its_own_lock() {
    if !have_flock() {
        eprintln!("skipped: flock(1) not available");
        return;
    }
    let root = scratch("unfrozen");
    let mut holder = hold(&root, 8);
    // under contention it must WAIT for the lock (proving it tries to lock
    // at all) instead of sneaking past it
    let out = nawa()
        .env("NAWA_LOCK_TIMEOUT", "1")
        .args(["--root", root.to_str().unwrap(), "remove", "curl"])
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success());
    assert!(
        err.contains("waiting for the package database lock"),
        "{err}"
    );
    let _ = holder.kill();
    let _ = holder.wait();
    let _ = fs::remove_dir_all(&root);
}

// ---- the migration mutation gate (design §10.3) -----------------------------

/// Seed a migration record the way the record store writes it (§10.3
/// record format), by hand: the gate must react to durable state under
/// `<root>/var/lib/nawa/migrations`, not to anything in-process.
fn seed_gate(root: &std::path::Path, id: &str, state: &str) -> PathBuf {
    let dir = root.join("var/lib/nawa/migrations");
    support::create_state_dirs(&dir);
    let path = dir.join(format!("{id}.toml"));
    support::write_record(
        &path,
        format!(
            "schema = 1\nid = \"{id}\"\ndefinition_version = 1\n\
             from = {{ domain = \"init-system\", identity = \"runit\" }}\n\
             to = {{ domain = \"init-system\", identity = \"systemd\" }}\n\
             handler = \"faseela.init.runit_to_systemd\"\nstate = \"{state}\"\n"
        ),
    );
    path
}

/// While a migration record under the selected root is in a gate-keeping
/// state, every package-mutating command must stop *before changing the
/// package set*: the refusal names the migration, its state and where the
/// records live, and the package database is never created.
#[test]
fn a_pending_migration_blocks_mutating_commands() {
    let root = scratch("gate-mutate");
    seed_gate(&root, "faseela-runit-to-systemd-v1", "preparing");
    for args in [
        vec!["remove", "curl", "--yes"],
        vec!["autoremove", "--yes"],
        vec!["install", "/tmp/nonexistent.nawa.tar.zst"],
        vec!["upgrade"],
    ] {
        let out = nawa()
            .env("NAWA_LOCK_TIMEOUT", "1")
            .args(["--root", root.to_str().unwrap()])
            .args(&args)
            .output()
            .unwrap();
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success(), "{args:?} must refuse; got: {err}");
        assert!(
            err.contains("faseela-runit-to-systemd-v1 is preparing"),
            "{args:?} must name the migration and its state: {err}"
        );
        assert!(
            err.contains("package mutation gate"),
            "{args:?} must name the gate: {err}"
        );
        assert!(
            err.contains("var/lib/nawa/migrations"),
            "{args:?} must point at the record directory: {err}"
        );
    }
    // the record is untouched and the package database was never created
    assert!(
        root.join("var/lib/nawa/migrations/faseela-runit-to-systemd-v1.toml")
            .exists(),
        "the gate check must not modify migration records"
    );
    assert!(
        !root.join("var/lib/nawa/local").exists(),
        "no package state may be created"
    );
    let _ = fs::remove_dir_all(&root);
}

/// Read-only inspection stays available while a migration is pending
/// (§10.3): readers are never gate-checked, best-effort by design.
#[test]
fn read_only_commands_stay_available_during_a_migration() {
    let root = scratch("gate-read");
    seed_gate(&root, "faseela-runit-to-systemd-v1", "preparing");
    let out = nawa()
        .env("NAWA_LOCK_TIMEOUT", "1")
        .args(["--root", root.to_str().unwrap(), "query"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let _ = fs::remove_dir_all(&root);
}

/// The `NAWA_LOCK_HELD` marker is inherited process state and forgeable:
/// it must never bypass the gate (§10.3: the gate is checked *before* the
/// marker is honored). With a pending migration the gate's error wins over
/// the hook-context error; nested read-only calls keep working (the parent
/// command was gate-checked when it acquired its own exclusive lock).
#[test]
fn the_forged_held_marker_cannot_bypass_the_migration_gate() {
    let root = scratch("gate-marker");
    seed_gate(&root, "faseela-runit-to-systemd-v1", "preparing");
    for args in [vec!["remove", "curl", "--yes"], vec!["autoremove", "--yes"]] {
        let out = nawa()
            .env("NAWA_LOCK_HELD", "1")
            .env("NAWA_LOCK_TIMEOUT", "1")
            .args(["--root", root.to_str().unwrap()])
            .args(&args)
            .output()
            .unwrap();
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success(), "{args:?} must refuse; got: {err}");
        assert!(
            err.contains("faseela-runit-to-systemd-v1 is preparing"),
            "{args:?} must name the gate, not the hook context: {err}"
        );
        assert!(
            !err.contains("package hook or transaction trigger"),
            "{args:?}: the gate error is the more precise refusal: {err}"
        );
    }
    // nested read-only calls keep working: reads stay lenient
    let out = nawa()
        .env("NAWA_LOCK_HELD", "1")
        .env("NAWA_LOCK_TIMEOUT", "1")
        .args(["--root", root.to_str().unwrap(), "query"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let _ = fs::remove_dir_all(&root);
}

/// The gate is the record's doing, nothing sticky: once the record stops
/// keeping it (removed here), the same mutating command proceeds to its
/// ordinary behavior instead of the gate refusal.
#[test]
fn the_gate_lifts_when_the_record_stops_keeping_it() {
    let root = scratch("gate-lift");
    let path = seed_gate(&root, "faseela-runit-to-systemd-v1", "preparing");
    let out = nawa()
        .args(["--root", root.to_str().unwrap(), "remove", "curl", "--yes"])
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("package mutation gate"), "{err}");
    fs::remove_file(&path).unwrap();
    let out = nawa()
        .args(["--root", root.to_str().unwrap(), "remove", "curl", "--yes"])
        .output()
        .unwrap();
    let all = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!all.contains("package mutation gate"), "{all}");
    let _ = fs::remove_dir_all(&root);
}
