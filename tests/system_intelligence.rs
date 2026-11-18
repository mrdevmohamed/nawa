//! Read-only system intelligence commands against isolated offline roots.
//! These tests prove that host PID 1 is never attributed to --root and that
//! desired profile overrides are shown with their actual source.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn nawa_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_nawa"))
}

fn nawa() -> Command {
    let mut command = Command::new(nawa_bin());
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
        command.env_remove(key);
    }
    command.env(
        "NAWA_SYSCONFDIR",
        std::env::temp_dir().join("nawa-system-intelligence-no-sysconf"),
    );
    // Explicitly disable the built-in repository for deterministic inspection tests.
    // Tests that need repository metadata opt back in explicitly.
    command.env("NAWA_REPO", "none");
    // The built-in stable repository is the last config fallback, so an empty
    // NAWA_REPO cannot disable it. Use an isolated config file with repo="".
    let config = std::env::temp_dir().join(format!(
        "nawa-system-intelligence-no-repo-{}.toml",
        std::process::id()
    ));
    fs::write(&config, "repo = \"\"\n").unwrap();
    command.env("NAWA_CONFIG", config);
    command.current_dir(std::env::temp_dir());
    command
}

fn scratch(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "nawa-system-intelligence-{tag}-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    root
}

fn run(root: &Path, args: &[&str]) -> (String, String, bool) {
    let output = nawa()
        .args(["--root", root.to_str().unwrap()])
        .args(args)
        .output()
        .unwrap();
    (
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
        output.status.success(),
    )
}

fn write_profile(root: &Path, relative: &str, content: &str) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

#[test]
fn status_does_not_misattribute_host_pid_one_to_an_offline_root() {
    let root = scratch("status-offline");
    write_profile(
        &root,
        "usr/share/nawa/system-profile.toml",
        "schema = 1\n[providers]\ninit-system = \"systemd\"\n",
    );
    let (stdout, stderr, success) = run(&root, &["system", "status"]);
    assert!(success, "{stderr}\n{stdout}");
    assert!(
        stdout.contains("Execution context: offline target root"),
        "{stdout}"
    );
    assert!(stdout.contains("Runtime PID 1:     unknown"), "{stdout}");
    assert!(stdout.contains("Desired init:      systemd"), "{stdout}");
    assert!(!stdout.contains("Runtime PID 1:     systemd"), "{stdout}");
    let _ = fs::remove_dir_all(root);
}

#[test]
fn profile_show_merges_override_and_reports_the_provider_source() {
    let root = scratch("profile-merge");
    write_profile(&root, "usr/share/nawa/system-profile.toml", "schema = 1\ndistribution = \"faseela\"\nprofile = \"desktop\"\narchitecture = \"x86_64\"\n[providers]\ninit-system = \"systemd\"\n");
    write_profile(
        &root,
        "etc/nawa/system-profile.toml",
        "schema = 1\n[providers]\ninit-system = \"runit\"\n",
    );
    let (stdout, stderr, success) = run(&root, &["system", "profile", "show"]);
    assert!(success, "{stderr}\n{stdout}");
    assert!(stdout.contains("Distribution:  faseela"), "{stdout}");
    assert!(stdout.contains("Profile:       desktop"), "{stdout}");
    assert!(stdout.contains("Architecture:  x86_64"), "{stdout}");
    assert!(stdout.contains("init-system = runit"), "{stdout}");
    assert!(stdout.contains("etc/nawa/system-profile.toml"), "{stdout}");
    assert!(!stdout.contains("init-system = systemd"), "{stdout}");
    let _ = fs::remove_dir_all(root);
}

#[test]
fn invalid_system_profile_fails_closed_instead_of_falling_back() {
    let root = scratch("profile-invalid");
    write_profile(
        &root,
        "usr/share/nawa/system-profile.toml",
        "schema = 99\n[providers]\ninit-system = \"systemd\"\n",
    );
    write_profile(
        &root,
        "etc/nawa/system-profile.toml",
        "schema = 1\n[providers]\ninit-system = \"runit\"\n",
    );
    let (_stdout, stderr, success) = run(&root, &["system", "profile", "show"]);
    assert!(!success, "an invalid distribution profile must fail closed");
    assert!(stderr.contains("unknown profile schema 99"), "{stderr}");
    let _ = fs::remove_dir_all(root);
}

#[test]
fn system_plan_keeps_an_offline_runtime_unresolved_and_is_explicitly_read_only() {
    let root = scratch("plan-offline");
    write_profile(
        &root,
        "usr/share/nawa/system-profile.toml",
        "schema = 1\n[providers]\ninit-system = \"systemd\"\n",
    );
    let (stdout, stderr, success) = run(&root, &["system", "plan"]);
    assert!(success, "{stderr}\n{stdout}");
    assert!(stdout.contains("System plan (read-only"), "{stdout}");
    assert!(stdout.contains("Decision: unresolved"), "{stdout}");
    assert!(stdout.contains("runtime PID 1 is unknown"), "{stdout}");
    assert!(
        stdout.contains("Exact repository provenance, transition decision and artifact pins"),
        "{stdout}"
    );
    assert!(stdout.contains("Unified SystemPlan (read-only"), "{stdout}");
    assert!(stdout.contains("Plan identity: sp-"), "{stdout}");
    assert!(stdout.contains("Repository: not configured"), "{stdout}");
    assert!(
        stdout.contains("Transition: unresolved — no repository is configured"),
        "{stdout}"
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn system_plan_preserves_observation_when_installed_package_database_is_corrupt() {
    let root = scratch("plan-corrupt-package-db");
    write_profile(
        &root,
        "usr/share/nawa/system-profile.toml",
        r#"schema = 1
[providers]
init-system = "systemd"
"#,
    );
    let package = root.join("var/lib/nawa/local/example");
    fs::create_dir_all(&package).unwrap();
    fs::write(package.join("desc"), "not valid package metadata").unwrap();

    let output = nawa()
        .args(["--root", root.to_str().unwrap(), "system", "plan"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "system plan is an inspection report: {stdout}"
    );
    assert!(stdout.contains("System plan (read-only"), "{stdout}");
    assert!(
        stdout.contains("installed package inventory is unavailable"),
        "{stdout}"
    );
    assert!(stdout.contains("corrupt db entry"), "{stdout}");
    assert!(stdout.contains("Repository: not inspected"), "{stdout}");
    assert!(
        stdout.contains("Transition: unresolved — installed package inventory is unavailable"),
        "{stdout}"
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn system_plan_reports_unavailable_repository_metadata_as_unresolved() {
    let root = scratch("plan-repository-unavailable");
    write_profile(
        &root,
        "usr/share/nawa/system-profile.toml",
        r#"schema = 1
[providers]
init-system = "systemd"
"#,
    );
    let repository = root.join("repository");
    fs::create_dir_all(&repository).unwrap();
    fs::write(repository.join("index.toml"), "not valid TOML").unwrap();

    let config = root.join("nawa.toml");
    fs::write(
        &config,
        format!("repo = {:?}\n", repository.to_string_lossy()),
    )
    .unwrap();
    let output = nawa()
        .env_remove("NAWA_REPO")
        .env("NAWA_CONFIG", &config)
        .args(["--root", root.to_str().unwrap(), "system", "plan"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "system plan is an inspection report: {stdout}"
    );
    assert!(stdout.contains("Decision: unresolved"), "{stdout}");
    assert!(stdout.contains("Repository: unavailable"), "{stdout}");
    assert!(
        stdout.contains("Transition: unresolved — authenticated repository metadata could not be loaded or verified"),
        "{stdout}"
    );
    assert!(stdout.contains("not valid TOML"), "{stdout}");
    let _ = fs::remove_dir_all(root);
}

#[test]
fn explain_init_system_documents_the_metadata_and_service_boundary() {
    let root = scratch("explain-init");
    let (stdout, stderr, success) = run(&root, &["system", "explain", "init-system"]);
    assert!(success, "{stderr}\n{stdout}");
    assert!(stdout.contains("Capability: init-system"), "{stdout}");
    assert!(stdout.contains("signed repository index v4"), "{stdout}");
    assert!(
        stdout.contains("legacy v1-v3 metadata remains unknown"),
        "{stdout}"
    );
    assert!(
        stdout.contains("does not query or manage service state"),
        "{stdout}"
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn explain_refuses_unsupported_system_topics() {
    let root = scratch("explain-unsupported");
    let (_stdout, stderr, success) = run(&root, &["system", "explain", "service-manager"]);
    assert!(
        !success,
        "unsupported capability domains must not be implied"
    );
    assert!(
        stderr.contains("supported topic is `init-system`"),
        "{stderr}"
    );
    let _ = fs::remove_dir_all(root);
}
