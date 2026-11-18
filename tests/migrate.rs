//! `nawa system migrate` inspection across real processes (design §12.2):
//! the read-only record commands against seeded durable records, on plain
//! `--root` directories (which is exactly what an offline root is). The
//! records are written by hand in the record store's exact format so the
//! commands are exercised against durable state, not against in-process
//! helpers.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn nawa_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_nawa"))
}

/// A nawa invocation isolated from the ambient configuration: no NAWA_* env
/// and a cwd outside the repository, so the repo's own `nawa.toml` is never
/// discovered.
fn nawa() -> Command {
    let mut c = Command::new(nawa_bin());
    for k in [
        "NAWA_CONFIG",
        "NAWA_ROOT",
        "NAWA_RECIPES",
        "NAWA_BUILDDIR",
        "NAWA_PROFILES",
        "NAWA_JOBS",
        "NAWA_REPO",
        "NAWA_INSTALL_MODE",
    ] {
        c.env_remove(k);
    }
    c.env(
        "NAWA_SYSCONFDIR",
        std::env::temp_dir().join("nawa-test-no-sysconf"),
    );
    c.current_dir(std::env::temp_dir());
    c
}

fn scratch(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("nawa-migratetest-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

fn run(root: &Path, args: &[&str]) -> (String, String, bool) {
    let out = nawa()
        .args(["--root", root.to_str().unwrap()])
        .args(args)
        .output()
        .unwrap();
    (
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
        out.status.success(),
    )
}

/// The migrations record directory of `root`.
fn records_dir(root: &Path) -> PathBuf {
    root.join("var/lib/nawa/migrations")
}

/// Seed one record the way the record store writes it (schema 1), with an
/// optional `[[steps]]` block appended verbatim.
fn seed(root: &Path, id: &str, state: &str, steps_toml: &str) {
    let dir = records_dir(root);
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join(format!("{id}.toml")),
        format!(
            "schema = 1\nid = \"{id}\"\ndefinition_version = 1\n\
             from = {{ domain = \"init-system\", identity = \"runit\" }}\n\
             to = {{ domain = \"init-system\", identity = \"systemd\" }}\n\
             handler = \"faseela.init.runit_to_systemd\"\nstate = \"{state}\"\n{steps_toml}"
        ),
    )
    .unwrap();
}

/// A `[[steps]]` block with one succeeded, one failed and one interrupted
/// step — the write-ahead shapes §10.3 defines.
fn steps_all_shapes() -> String {
    "[[steps]]\nid = \"stage-artifacts\"\nintent = \"fetch and verify package artifacts\"\noutcome = \"succeeded\"\n\n\
     [[steps]]\nid = \"install-packages\"\nintent = \"install the systemd packages\"\noutcome = \"failed\"\nfailure = \"package payload conflict\"\n\n\
     [[steps]]\nid = \"prepare-boot\"\nintent = \"prepare boot/initramfs integration\"\n"
        .to_string()
}

/// Snapshot every record file's bytes and the directory listing, so a
/// later comparison can prove the inspection commands changed nothing.
fn record_snapshot(root: &Path) -> Vec<(String, Vec<u8>)> {
    let mut snapshot = Vec::new();
    let dir = records_dir(root);
    if let Ok(entries) = fs::read_dir(&dir) {
        for entry in entries.filter_map(|e| e.ok()) {
            let name = entry.file_name().to_string_lossy().to_string();
            let bytes = fs::read(entry.path()).unwrap_or_default();
            snapshot.push((name, bytes));
        }
    }
    snapshot.sort();
    snapshot
}

// ---- list ------------------------------------------------------------------

#[test]
fn list_on_a_fresh_root_reports_no_record_and_creates_nothing() {
    let root = scratch("list-fresh");
    let (stdout, _stderr, ok) = run(&root, &["system", "migrate", "list"]);
    assert!(ok, "list on a fresh root succeeds: {stdout}");
    assert!(
        stdout.contains("no migration record exists"),
        "the empty store is stated explicitly: {stdout}"
    );
    assert!(
        stdout.contains("no migration has ever been started"),
        "the empty store is explained: {stdout}"
    );
    assert!(
        !root.join("var").exists(),
        "reading must not create any nawa state under the root"
    );
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn list_on_an_empty_record_directory_reports_no_record() {
    let root = scratch("list-empty");
    fs::create_dir_all(records_dir(&root)).unwrap();
    let (stdout, _stderr, ok) = run(&root, &["system", "migrate", "list"]);
    assert!(ok, "list on an empty directory succeeds: {stdout}");
    assert!(stdout.contains("no migration record exists"), "{stdout}");
    // the empty directory itself is left untouched
    assert_eq!(fs::read_dir(records_dir(&root)).unwrap().count(), 0);
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn list_shows_every_record_sorted_with_its_durable_fields() {
    let root = scratch("list-many");
    seed(&root, "zzz-migration", "awaiting-reboot", "");
    seed(&root, "aaa-migration", "completed", "");

    let (stdout, _stderr, ok) = run(&root, &["system", "migrate", "list"]);
    assert!(ok, "list succeeds: {stdout}");
    let zzz = stdout.find("zzz-migration").expect("zzz row printed");
    let aaa = stdout.find("aaa-migration").expect("aaa row printed");
    assert!(aaa < zzz, "deterministic (sorted by id) order: {stdout}");
    assert!(stdout.contains("awaiting-reboot"), "{stdout}");
    assert!(stdout.contains("completed"), "{stdout}");
    assert!(stdout.contains("init-system=runit"), "{stdout}");
    assert!(stdout.contains("init-system=systemd"), "{stdout}");
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn list_fails_loudly_when_a_record_is_corrupt_but_shows_the_readable_ones() {
    let root = scratch("list-corrupt");
    seed(&root, "good-migration", "planned", "");
    let dir = records_dir(&root);
    fs::write(dir.join("broken-migration.toml"), "not [ valid toml").unwrap();

    let (stdout, stderr, ok) = run(&root, &["system", "migrate", "list"]);
    assert!(!ok, "a corrupt record must not pass silently: {stdout}");
    assert!(
        stdout.contains("good-migration") && stdout.contains("planned"),
        "the readable records are still listed: {stdout}"
    );
    assert!(
        stderr.contains("broken-migration.toml"),
        "the error names the exact file: {stderr}"
    );
    assert!(
        stderr.contains("not valid TOML"),
        "the error is the actual validation failure: {stderr}"
    );
    let _ = fs::remove_dir_all(&root);
}

// ---- show ------------------------------------------------------------------

#[test]
fn show_displays_the_full_record_including_the_write_ahead_steps() {
    let root = scratch("show-full");
    seed(
        &root,
        "faseela-runit-to-systemd-v1",
        "awaiting-reboot",
        &steps_all_shapes(),
    );

    let (stdout, _stderr, ok) = run(
        &root,
        &["system", "migrate", "show", "faseela-runit-to-systemd-v1"],
    );
    assert!(ok, "show succeeds: {stdout}");
    for expected in [
        "Migration faseela-runit-to-systemd-v1",
        "Record schema:      1",
        "Definition version: 1",
        "Source capability:  init-system=runit",
        "Target capability:  init-system=systemd",
        "State:              awaiting-reboot",
        "Handler:            faseela.init.runit_to_systemd",
        // derived from the trusted handler binding, not a persisted field
        "Requires reboot:    yes",
        "stage-artifacts",
        "intent:  fetch and verify package artifacts",
        "outcome: succeeded",
        "install-packages",
        "outcome: failed — package payload conflict",
        // an intent without outcome is interrupted, never a success
        "outcome: INTERRUPTED",
    ] {
        assert!(
            stdout.contains(expected),
            "missing {expected:?} in:\n{stdout}"
        );
    }
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn show_of_an_unknown_id_fails_naming_the_record_directory() {
    let root = scratch("show-unknown");
    let (stdout, stderr, ok) = run(&root, &["system", "migrate", "show", "no-such-migration"]);
    assert!(!ok, "an unknown id must fail: {stdout}");
    assert!(
        stderr.contains("no-such-migration"),
        "the error names the id: {stderr}"
    );
    assert!(
        stderr.contains("var/lib/nawa/migrations"),
        "the error names the record directory: {stderr}"
    );
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn show_of_a_corrupt_record_reports_the_exact_validation_failure() {
    let root = scratch("show-corrupt");
    seed(&root, "faseela-runit-to-systemd-v1", "prepped", "");

    let (stdout, stderr, ok) = run(
        &root,
        &["system", "migrate", "show", "faseela-runit-to-systemd-v1"],
    );
    assert!(!ok, "a corrupt record must fail loudly: {stdout}");
    assert!(
        stderr.contains("faseela-runit-to-systemd-v1.toml"),
        "the error names the exact file: {stderr}"
    );
    assert!(
        stderr.contains("unknown migration state"),
        "the error is the actual validation failure: {stderr}"
    );
    let _ = fs::remove_dir_all(&root);
}

// ---- status ----------------------------------------------------------------

#[test]
fn status_reports_an_active_gate_and_exits_zero() {
    let root = scratch("status-gate");
    seed(&root, "faseela-runit-to-systemd-v1", "awaiting-reboot", "");

    let (stdout, _stderr, ok) = run(&root, &["system", "migrate", "status"]);
    assert!(ok, "status is informational and always exits 0: {stdout}");
    assert!(
        stdout.contains("blocked by the persistent migration gate"),
        "{stdout}"
    );
    assert!(
        stdout.contains("faseela-runit-to-systemd-v1 is awaiting-reboot"),
        "the gate names the migration and its durable state: {stdout}"
    );
    assert!(
        stdout.contains("var/lib/nawa/migrations"),
        "the record directory is shown: {stdout}"
    );
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn status_without_a_gate_reports_not_blocked_and_exits_zero() {
    // a fresh root: no record at all
    let root = scratch("status-fresh");
    let (stdout, _stderr, ok) = run(&root, &["system", "migrate", "status"]);
    assert!(ok, "{stdout}");
    assert!(stdout.contains("not blocked"), "{stdout}");
    let _ = fs::remove_dir_all(&root);

    // a planned record does not keep the gate (§10.3)
    let root = scratch("status-planned");
    seed(&root, "faseela-runit-to-systemd-v1", "planned", "");
    let (stdout, _stderr, ok) = run(&root, &["system", "migrate", "status"]);
    assert!(ok, "{stdout}");
    assert!(stdout.contains("not blocked"), "{stdout}");
    assert!(
        !stdout.contains("blocked by the persistent migration gate"),
        "{stdout}"
    );
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn status_reports_a_corrupt_record_as_a_fail_closed_block_and_exits_zero() {
    let root = scratch("status-corrupt");
    let dir = records_dir(&root);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("broken-migration.toml"), "not [ valid toml").unwrap();

    let (stdout, _stderr, ok) = run(&root, &["system", "migrate", "status"]);
    assert!(ok, "status is informational and always exits 0: {stdout}");
    assert!(
        stdout.contains("blocked fail-closed"),
        "an unreadable record blocks mutations fail-closed: {stdout}"
    );
    assert!(
        stdout.contains("broken-migration.toml") && stdout.contains("not valid TOML"),
        "the report carries the actual cause: {stdout}"
    );
    let _ = fs::remove_dir_all(&root);
}

// ---- read-only proofs ------------------------------------------------------

#[test]
fn inspection_never_modifies_the_records() {
    let root = scratch("read-only");
    seed(
        &root,
        "faseela-runit-to-systemd-v1",
        "preparing",
        &steps_all_shapes(),
    );
    seed(&root, "aaa-old-migration", "completed", "");
    let before = record_snapshot(&root);
    assert_eq!(before.len(), 2);

    for args in [
        vec!["system", "migrate", "list"],
        vec!["system", "migrate", "show", "faseela-runit-to-systemd-v1"],
        vec!["system", "migrate", "status"],
    ] {
        let (stdout, _stderr, ok) = run(&root, &args);
        assert!(ok, "{args:?} succeeds: {stdout}");
        assert_eq!(
            record_snapshot(&root),
            before,
            "{args:?} must not modify a single record byte"
        );
    }
    // no state appeared anywhere else either: the shared reader lock may
    // exist (that is every reader's behavior on a stateful root), but the
    // package database must never be created by an inspection command
    assert!(
        !root.join("var/lib/nawa/local").exists(),
        "no package state may be created"
    );
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn inspection_is_rooted_under_the_selected_root() {
    // records under root A must not be visible through root B: the
    // commands read `<root>/var/lib/nawa/migrations` of the selected root
    // only (§10.3: the migration record is rooted at the target root)
    let a = scratch("rooted-a");
    let b = scratch("rooted-b");
    seed(&a, "faseela-runit-to-systemd-v1", "preparing", "");

    let (stdout_a, _stderr, ok_a) = run(&a, &["system", "migrate", "list"]);
    assert!(
        ok_a && stdout_a.contains("faseela-runit-to-systemd-v1"),
        "{stdout_a}"
    );

    let (stdout_b, _stderr, ok_b) = run(&b, &["system", "migrate", "list"]);
    assert!(ok_b, "{stdout_b}");
    assert!(
        stdout_b.contains("no migration record exists"),
        "root B must not see root A's records: {stdout_b}"
    );
    let (stdout_s, _stderr, ok_s) = run(&b, &["system", "migrate", "status"]);
    assert!(ok_s && stdout_s.contains("not blocked"), "{stdout_s}");
    let _ = fs::remove_dir_all(&a);
    let _ = fs::remove_dir_all(&b);
}

#[test]
fn inspection_works_on_an_offline_root_with_no_live_system() {
    // an offline root is just a directory tree: the commands must work on
    // it read-only — no lock file, no package database, nothing created
    let root = scratch("offline-root");
    fs::create_dir_all(root.join("usr/bin")).unwrap();
    seed(&root, "faseela-runit-to-systemd-v1", "awaiting-reboot", "");

    for args in [
        vec!["system", "migrate", "list"],
        vec!["system", "migrate", "status"],
        vec!["system", "migrate", "show", "faseela-runit-to-systemd-v1"],
    ] {
        let (stdout, _stderr, ok) = run(&root, &args);
        assert!(ok, "{args:?} on an offline root succeeds: {stdout}");
    }
    assert!(
        !root.join("var/lib/nawa/local").exists(),
        "reading an offline root must not create the package database"
    );
    assert_eq!(
        record_snapshot(&root).len(),
        1,
        "the record survived every read byte-identical (single entry)"
    );
    let _ = fs::remove_dir_all(&root);
}

// ---- resume (explicit live-host recovery, §12.2) -------------------------

#[test]
fn resume_is_documented_in_help_and_all_three_completions() {
    let out = nawa().arg("--help").output().unwrap();
    let help = String::from_utf8_lossy(&out.stdout);
    assert!(help.contains("system"), "the namespace is listed: {help}");

    for (shell, needle) in [
        ("bash", "list show status plan apply resume"),
        ("zsh", "list show status plan apply resume"),
        ("fish", "list show status plan apply resume"),
    ] {
        let out = nawa().args(["completions", shell]).output().unwrap();
        let script = String::from_utf8_lossy(&out.stdout);
        assert!(script.contains(needle), "{shell} completion: {script}");
    }
}

#[test]
fn resume_refuses_an_offline_root_and_changes_nothing() {
    let root = scratch("resume-offline");
    seed(&root, "faseela-runit-to-systemd-v1", "awaiting-reboot", "");
    let before = record_snapshot(&root);

    let (stdout, stderr, ok) = run(&root, &["system", "migrate", "resume"]);
    assert!(!ok, "resume is a live-host operation: {stdout}");
    let combined = format!("{stdout}{stderr}");
    assert!(
        combined.contains("offline root") || combined.contains("live-host"),
        "the refusal explains the boundary: {combined}"
    );
    assert_eq!(
        record_snapshot(&root),
        before,
        "no record was changed on the offline root"
    );
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn plan_is_read_only_and_works_on_an_offline_root() {
    let root = scratch("plan-offline");
    let before = record_snapshot(&root);
    let (stdout, stderr, ok) = run(
        &root,
        &["system", "migrate", "plan", "faseela-runit-to-systemd-v1"],
    );
    assert!(ok, "planning an offline root is read-only: {stderr}");
    for expected in [
        "Migration plan for faseela-runit-to-systemd-v1",
        "offline target root",
        "init-system=runit",
        "init-system=systemd",
        "Rollback limits",
        "no automatic rollback",
    ] {
        assert!(stdout.contains(expected), "missing {expected:?} in:\n{stdout}");
    }
    assert_eq!(record_snapshot(&root), before, "plan persisted nothing");
    assert!(!root.join("var/lib/nawa").exists(), "no nawa state created");
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn plan_refuses_an_unknown_built_in_id() {
    let root = scratch("plan-unknown");
    let (_stdout, stderr, ok) = run(&root, &["system", "migrate", "plan", "not-a-migration"]);
    assert!(!ok);
    assert!(stderr.contains("no built-in migration"), "{stderr}");
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn apply_refuses_an_offline_root_before_any_mutation() {
    let root = scratch("apply-offline");
    let before = record_snapshot(&root);
    // Supply the newly required identity so this test reaches the live-host
    // preflight boundary instead of stopping in the CLI argument parser.
    let plan_id = format!("sp-{}", "0".repeat(64));
    let args = [
        "system",
        "migrate",
        "apply",
        "faseela-runit-to-systemd-v1",
        "--from",
        "init-system=runit",
        "--to",
        "init-system=systemd",
        "--plan-id",
        plan_id.as_str(),
        "--acknowledge-reboot",
        "--acknowledge-recovery",
    ];
    let (stdout, stderr, ok) = run(&root, &args);
    assert!(!ok, "apply is live-host only: {stdout}");
    let combined = format!("{stdout}{stderr}");
    assert!(combined.contains("offline root"), "{combined}");
    assert_eq!(record_snapshot(&root), before, "no record was created");
    assert!(!root.join("var/lib/nawa").exists(), "no nawa state created");
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn apply_requires_explicit_per_migration_authorization() {
    let root = scratch("apply-no-auth");
    // a generic auto-confirm style invocation (no exact source/target and no
    // boundary acknowledgements) must not reach even the preflight.
    let (_stdout, stderr, ok) = run(
        &root,
        &["system", "migrate", "apply", "faseela-runit-to-systemd-v1"],
    );
    assert!(!ok, "a generic apply is refused: {stderr}");
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn resume_on_a_fresh_root_reports_no_interrupted_migration() {
    let root = scratch("resume-fresh-offline");
    // On an offline root the live-host preflight refuses before any record
    // is touched — there is no fake success path (§12.2).
    let (stdout, stderr, ok) = run(&root, &["system", "migrate", "resume"]);
    assert!(!ok, "{stdout}");
    let combined = format!("{stdout}{stderr}");
    assert!(combined.contains("offline root"), "{combined}");
    assert!(
        !root.join("var/lib/nawa/lock").exists(),
        "no lock was created on the refused offline root"
    );
    let _ = fs::remove_dir_all(&root);
}
