//! The database lock across real processes. Uses util-linux `flock(1)` to
//! play "another nawa holding the lock"; the tests are skipped when it is
//! not installed.

use std::fs;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

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
    fs::create_dir_all(root.join("var/lib/nawa")).unwrap();
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
        assert!(!out.status.success(), "{args:?} must not run while the lock is held");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.contains("waiting for the package database lock"), "{err}");
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
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
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
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
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
    // a nawa started with NAWA_LOCK_HELD (as hooks are) skips locking entirely
    let out = nawa()
        .env("NAWA_LOCK_HELD", "1")
        .env("NAWA_LOCK_TIMEOUT", "1")
        .args(["--root", root.to_str().unwrap(), "query"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let _ = holder.kill();
    let _ = holder.wait();
    let _ = fs::remove_dir_all(&root);
}
