//! Adversarial security tests for the migration subsystem (design §13,
//! §14.1–14.2). Each test demonstrates a property with a real process and
//! durable on-disk state — not by inspecting the source. Threat model: the
//! same as the package manager — an attacker controls environment variables,
//! repository metadata, plugin manifests, archive contents and files under a
//! target root, but cannot write to a root-owned state directory without
//! already being privileged.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn nawa_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_nawa"))
}

/// An isolated invocation: no ambient NAWA_* configuration, cwd outside the
/// repository, so nothing is inherited from the host or the test harness.
fn nawa() -> Command {
    let mut c = Command::new(nawa_bin());
    for key in [
        "NAWA_CONFIG",
        "NAWA_ROOT",
        "NAWA_RECIPES",
        "NAWA_BUILDDIR",
        "NAWA_PROFILES",
        "NAWA_JOBS",
        "NAWA_REPO",
        "NAWA_INSTALL_MODE",
    ] {
        c.env_remove(key);
    }
    c.env(
        "NAWA_SYSCONFDIR",
        std::env::temp_dir().join("nawa-sec-no-sysconf"),
    );
    c.current_dir(std::env::temp_dir());
    c
}

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("nawa-sec-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn run(root: &Path, args: &[&str], forged_marker: bool) -> (String, String, bool) {
    let mut command = nawa();
    command.args(["--root", root.to_str().unwrap()]).args(args);
    if forged_marker {
        command.env("NAWA_LOCK_HELD", "1");
    }
    let out = command.output().unwrap();
    (
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
        out.status.success(),
    )
}

fn records_dir(root: &Path) -> PathBuf {
    root.join("var/lib/nawa/migrations")
}

/// Seed one record in the store's exact schema-1 format.
fn seed(root: &Path, id: &str, state: &str, handler: &str) -> PathBuf {
    let dir = records_dir(root);
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{id}.toml"));
    fs::write(
        &path,
        format!(
            "schema = 1\nid = \"{id}\"\ndefinition_version = 1\n\
             from = {{ domain = \"init-system\", identity = \"runit\" }}\n\
             to = {{ domain = \"init-system\", identity = \"systemd\" }}\n\
             handler = \"{handler}\"\nstate = \"{state}\"\n"
        ),
    )
    .unwrap();
    path
}

// ---- Mutation gate: fail-closed state ------------------------------------

#[test]
fn an_unreadable_record_directory_fails_the_gate_closed_for_mutations() {
    let root = scratch("unreadable-dir");
    let state = root.join("var/lib/nawa");
    fs::create_dir_all(&state).unwrap();
    // a *file* where the record directory should be: no-follow validation
    // rejects it before enumerating or trusting its contents.
    fs::write(records_dir(&root), "not a directory").unwrap();

    let (_stdout, stderr, ok) = run(&root, &["remove", "anything", "--yes"], false);
    assert!(!ok, "a mutation must fail when gate state cannot be read: {stderr}");
    assert!(
        stderr.contains("cannot check the persistent migration mutation gate")
            && (stderr.contains("Not a directory") || stderr.contains("not a real directory")),
        "the refusal names the unreadable state: {stderr}"
    );

    // `status` reports the same fail-closed answer, but read-only inspection
    // never mutates and exits 0 (§12.2).
    let (stdout, _stderr, ok) = run(&root, &["system", "migrate", "status"], false);
    assert!(ok, "status is informational: {stdout}");
    assert!(stdout.contains("blocked fail-closed"), "{stdout}");
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn multiple_gate_records_are_reported_deterministically() {
    let root = scratch("multiple-gates");
    seed(&root, "zzz-migration", "awaiting-reboot", "faseela.init.runit_to_systemd");
    seed(&root, "aaa-migration", "needs-recovery", "faseela.init.runit_to_systemd");

    let (_stdout, stderr, ok) = run(&root, &["remove", "anything", "--yes"], false);
    assert!(!ok);
    let first = stderr.find("aaa-migration is needs-recovery").expect("first gate");
    let second = stderr.find("zzz-migration is awaiting-reboot").expect("second gate");
    assert!(first < second, "deterministic sorted order: {stderr}");
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn a_corrupt_but_parseable_record_fails_closed_naming_the_file() {
    let root = scratch("corrupt-record");
    let path = seed(&root, "broken", "bogus-state", "faseela.init.runit_to_systemd");

    let (_stdout, stderr, ok) = run(&root, &["remove", "anything", "--yes"], false);
    assert!(!ok);
    assert!(stderr.contains("cannot check the persistent migration mutation gate"), "{stderr}");
    assert!(
        stderr.contains(path.file_name().unwrap().to_string_lossy().as_ref()),
        "the exact file is named: {stderr}"
    );
    assert!(stderr.contains("unknown migration state"), "the cause is named: {stderr}");
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn a_symlinked_record_cannot_redirect_gate_state_outside_the_root() {
    let root = scratch("symlink-record");
    let outside = scratch("symlink-target");
    let target = outside.join("gate.toml");
    fs::write(
        &target,
        "schema = 1\nid = \"evil\"\ndefinition_version = 1\n\
         from = { domain = \"init-system\", identity = \"runit\" }\n\
         to = { domain = \"init-system\", identity = \"systemd\" }\n\
         handler = \"faseela.init.runit_to_systemd\"\nstate = \"preparing\"\n",
    )
    .unwrap();
    let dir = records_dir(&root);
    fs::create_dir_all(&dir).unwrap();
    std::os::unix::fs::symlink(&target, dir.join("evil.toml")).unwrap();

    // no-follow: the symlink is refused, so gate verification fails closed.
    let (_stdout, stderr, ok) = run(&root, &["remove", "anything", "--yes"], false);
    assert!(!ok);
    assert!(
        stderr.contains("symlink") || stderr.contains("cannot check"),
        "the symlink is never followed: {stderr}"
    );
    let _ = fs::remove_dir_all(&root);
    let _ = fs::remove_dir_all(&outside);
}

#[test]
fn terminal_records_do_not_block_mutations() {
    let root = scratch("terminal-records");
    seed(&root, "done", "completed", "faseela.init.runit_to_systemd");
    seed(&root, "failed", "failed", "faseela.init.runit_to_systemd");
    // `remove` reaches its ordinary "not installed" behaviour rather than
    // the migration gate (the gate is clear in terminal states).
    let (_stdout, stderr, ok) = run(&root, &["remove", "not-installed", "--yes"], false);
    assert!(!ok);
    assert!(
        !stderr.contains("holds the package mutation gate"),
        "terminal states keep no gate: {stderr}"
    );
    let _ = fs::remove_dir_all(&root);
}

// ---- Lock ownership / authority ------------------------------------------

#[test]
fn a_forged_held_marker_cannot_resume_a_migration_or_cross_the_gate() {
    let root = scratch("forged-resume");
    seed(&root, "faseela-runit-to-systemd-v1", "awaiting-reboot", "faseela.init.runit_to_systemd");
    let before = fs::read(records_dir(&root).join("faseela-runit-to-systemd-v1.toml")).unwrap();

    // offline-root preflight already refuses, but the forged marker must
    // never become migration authority: no record is touched, no lock made.
    let (stdout, stderr, ok) = run(&root, &["system", "migrate", "resume"], true);
    assert!(!ok, "a nested process cannot resume a migration: {stdout}{stderr}");
    assert_eq!(
        fs::read(records_dir(&root).join("faseela-runit-to-systemd-v1.toml")).unwrap(),
        before,
        "the record survived byte-for-byte"
    );
    assert!(!root.join("var/lib/nawa/lock").exists(), "no lock was created");
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn a_nested_mutating_command_is_refused_even_with_the_gate_absent() {
    let root = scratch("nested-mutate");
    // No migration record: the gate is clear, but the forged marker still
    // must not authorize a package mutation from a hook/trigger child.
    let (_stdout, stderr, ok) = run(&root, &["remove", "anything", "--yes"], true);
    assert!(!ok);
    assert!(
        stderr.contains("package hook or transaction trigger"),
        "the marker carries no mutation authority: {stderr}"
    );
    let _ = fs::remove_dir_all(&root);
}

// ---- Trust boundary: privileged handlers ---------------------------------

#[test]
fn a_repository_style_handler_cannot_drive_the_migration_namespace() {
    // The CLI has no way to submit a handler id: the §12.2 namespace accepts
    // only built-in verbs and a validated record id. An executable path or
    // shell text as an argument is therefore refused as an unknown command,
    // never executed (the typed allowlist is tested in the domain suite).
    let root = scratch("handler-attack-cli");
    for attack in ["/usr/bin/sh", "sh -c reboot", ".HOOKS", "migration.handler"] {
        let (_stdout, stderr, ok) = run(&root, &["system", "migrate", "show", attack], false);
        assert!(!ok, "an attack string {attack:?} must not be accepted: {stderr}");
        assert!(
            stderr.contains("no migration record")
                || stderr.contains("unknown")
                || stderr.contains("invalid migration id"),
            "the refusal is about an unknown id, not execution: {stderr}"
        );
    }
    let _ = fs::remove_dir_all(&root);
}
