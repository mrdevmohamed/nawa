//! Integration tests for the plugin system (roadmap Phase 2): discovery,
//! enable/disable, protocol execution and the failure modes that must never
//! crash or silently succeed.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn nawa_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_nawa"))
}

fn scratch(tag: &str) -> PathBuf {
    // Root discovery deliberately rejects plugin fixtures beneath /tmp,
    // which is world-writable. Keep root-run tests under /root so they
    // exercise the privileged discovery path without weakening that check.
    let base = if rustix::process::geteuid().is_root() {
        PathBuf::from("/root")
    } else {
        std::env::temp_dir()
    };
    let dir = base.join(format!("nawa-plugins-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("plugins")).unwrap();
    dir
}

fn run(dir: &Path, args: &[&str]) -> Output {
    Command::new(nawa_bin())
        .env_remove("NAWA_CONFIG")
        .env_remove("NAWA_REPO")
        .env("NAWA_SYSCONFDIR", dir.join("no-sysconf"))
        .env("NO_COLOR", "1")
        .env("NAWA_PLUGIN_PATH", dir.join("plugins"))
        .current_dir(dir)
        .args(args)
        .output()
        .unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn ok(output: Output) -> String {
    assert!(
        output.status.success(),
        "command failed:\n{}\n{}",
        stdout(&output),
        String::from_utf8_lossy(&output.stderr)
    );
    stdout(&output)
}

/// A plugin that answers the handshake and echoes its arguments back.
const GOOD_PLUGIN: &str = r#"#!/bin/sh
read -r line
printf '%s\n' '{"protocol":"nawa-plugin","protocol_version":1,"request_id":"1","ok":true,"result":{"plugin_id":"good","plugin_version":"1.0.0","api_version":"1","commands":["hello-cmd"],"capabilities":["history.read"]}}'
read -r line
printf '%s\n' '{"protocol":"nawa-plugin","protocol_version":1,"request_id":"run-RUNID","ok":true,"result":{"greeting":"from-plugin"}}'
"#;

/// A plugin whose response line uses a wrong request id.
const WRONG_ID_PLUGIN: &str = r#"#!/bin/sh
read -r line
printf '%s\n' '{"protocol":"nawa-plugin","protocol_version":1,"request_id":"1","ok":true,"result":{"plugin_id":"wrongid","plugin_version":"1.0.0","api_version":"1","commands":["x"]}}'
read -r line
printf '%s\n' '{"protocol":"nawa-plugin","protocol_version":1,"request_id":"someone-else","ok":true,"result":{}}'
"#;

fn install_plugin(dir: &Path, id: &str, script: &str, commands: &str) {
    let pdir = dir.join("plugins").join(id);
    fs::create_dir_all(&pdir).unwrap();
    let exe = pdir.join("nawa-plugin-x");
    fs::write(&exe, script).unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(
        pdir.join("nawa-plugin.toml"),
        format!(
            "[plugin]\nid = \"{id}\"\nname = \"T\"\nversion = \"1.0.0\"\napi_version = \"1\"\nexecutable = \"{}\"\ndescription = \"test plugin\"\n\n[commands]\nnames = [{commands}]\n",
            exe.display()
        ),
    )
    .unwrap();
}

#[test]
fn plugin_list_and_info_show_discovered_state() {
    let dir = scratch("list");
    install_plugin(&dir, "good", GOOD_PLUGIN, "\"hello-cmd\"");
    install_plugin(&dir, "bad", "#!/bin/sh\nexit 3\n", "\"bad-cmd\""); // valid manifest, broken binary content is only seen at run

    let out = ok(run(&dir, &["plugin", "list"]));
    assert!(out.contains("good"), "{out}");
    assert!(out.contains("enabled"), "{out}");

    let out = ok(run(&dir, &["plugin", "info", "good"]));
    assert!(out.contains("Version:     1.0.0"), "{out}");
    assert!(out.contains("hello-cmd"), "{out}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn enable_disable_round_trip_through_config() {
    let dir = scratch("enable");
    install_plugin(&dir, "good", GOOD_PLUGIN, "\"hello-cmd\"");
    // a config file must exist for the state to be stored
    fs::write(dir.join("nawa.toml"), "jobs = 2\n").unwrap();

    ok(run(&dir, &["plugin", "enable", "good"]));
    let raw = fs::read_to_string(dir.join("nawa.toml")).unwrap();
    assert!(raw.contains("[plugins]"), "{raw}");
    assert!(raw.contains("\"good\""), "{raw}");
    assert!(raw.contains("jobs = 2"), "other keys preserved: {raw}");

    // disabled plugins never execute
    ok(run(&dir, &["plugin", "disable", "good"]));
    let out = run(&dir, &["plugin", "run", "good", "hello-cmd"]);
    assert!(!out.status.success(), "disabled plugin must refuse to run");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("disabled"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn plugin_run_executes_and_prints_json() {
    let dir = scratch("run");
    // install the smart plugin directly: it reflects the request id back
    let smart = r#"#!/bin/sh
read -r line
printf '%s\n' '{"protocol":"nawa-plugin","protocol_version":1,"request_id":"1","ok":true,"result":{"plugin_id":"good","plugin_version":"1.0.0","api_version":"1","commands":["hello-cmd"],"capabilities":[]}}'
read -r line
rid=$(printf '%s' "$line" | sed 's/.*"request_id":"\([^"]*\)".*/\1/')
printf '{"protocol":"nawa-plugin","protocol_version":1,"request_id":"%s","ok":true,"result":{"greeting":"from-plugin"}}\n' "$rid"
"#;
    install_plugin(&dir, "good", smart, "\"hello-cmd\"");

    let out = ok(run(&dir, &["plugin", "run", "good", "hello-cmd"]));
    assert!(out.contains("from-plugin"), "{out}");
    // argv is preserved (never a shell string)
    let out = ok(run(&dir, &["plugin", "run", "good", "hello-cmd", "a;rm", "b c"]));
    assert!(out.contains("from-plugin"), "{out}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn wrong_request_id_is_an_error() {
    let dir = scratch("wrongid");
    let smart = WRONG_ID_PLUGIN;
    install_plugin(&dir, "wrongid", smart, "\"x\"");
    let out = run(&dir, &["plugin", "run", "wrongid", "x"]);
    assert!(!out.status.success(), "a mismatched request id must fail the run");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("answered request") || err.contains("request"), "{err}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn unknown_plugin_and_unknown_command_are_errors() {
    let dir = scratch("unknown");
    install_plugin(&dir, "good", GOOD_PLUGIN, "\"hello-cmd\"");
    let out = run(&dir, &["plugin", "run", "nope", "x"]);
    assert!(!out.status.success());
    let out = run(&dir, &["plugin", "run", "good", "not-registered"]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("does not register"), "{err}");
    let out = run(&dir, &["plugin", "info", "nope"]);
    assert!(!out.status.success());
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn broken_plugins_do_not_crash_list_or_run() {
    let dir = scratch("broken");
    // invalid manifest: bad TOML
    let pdir = dir.join("plugins").join("invalid");
    fs::create_dir_all(&pdir).unwrap();
    fs::write(pdir.join("nawa-plugin.toml"), "not toml [[[").unwrap();
    // incompatible API version
    install_plugin(&dir, "oldapi", GOOD_PLUGIN, "\"x\"");
    let raw = fs::read_to_string(pdir2(&dir, "oldapi").join("nawa-plugin.toml")).unwrap();
    fs::write(pdir2(&dir, "oldapi").join("nawa-plugin.toml"), raw.replace("api_version = \"1\"", "api_version = \"9\"")).unwrap();

    // listing reports them without executing anything
    let out = ok(run(&dir, &["plugin", "list"]));
    assert!(out.contains("invalid"), "{out}");
    assert!(out.contains("oldapi"), "{out}");

    // info/run on them are clean errors
    let out = run(&dir, &["plugin", "run", "oldapi", "x"]);
    assert!(!out.status.success());
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn registered_commands_require_an_administrator_grant_and_dispatch_automatically() {
    // Only the root CI job can create an authentic administrator policy file.
    if !rustix::process::geteuid().is_root() {
        return;
    }

    let dir = scratch("command-register");
    let pdir = dir.join("plugins").join("registered");
    fs::create_dir_all(&pdir).unwrap();
    let exe = pdir.join("nawa-plugin-registered");
    fs::write(
        &exe,
        r#"#!/bin/sh
read -r hello
printf '%s\n' '{"protocol":"nawa-plugin","protocol_version":1,"request_id":"hello-1","ok":true,"result":{"plugin_id":"registered","plugin_version":"1.0.0","api_version":"1","commands":["hello-cmd"],"capabilities":["command.register"]}}'
read -r request
rid=$(printf '%s' "$request" | sed -n 's/.*"request_id":"\([^"]*\)".*/\1/p')
printf '{"protocol":"nawa-plugin","protocol_version":1,"request_id":"%s","ok":true,"result":{"automatically_dispatched":true}}\n' "$rid"
"#,
    ).unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&exe, fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(
        pdir.join("nawa-plugin.toml"),
        format!(
            "[plugin]\nid = \"registered\"\nname = \"Registered command\"\nversion = \"1.0.0\"\napi_version = \"1\"\nexecutable = \"{}\"\n\n[commands]\nnames = [\"hello-cmd\"]\n\n[capabilities]\nrequested = [\"command.register\"]\n",
            exe.display()
        ),
    ).unwrap();

    let sysconf = dir.join("no-sysconf");
    fs::create_dir_all(&sysconf).unwrap();
    let config = sysconf.join("nawa.toml");
    fs::write(&config, "[plugins]\nenabled = [\"registered\"]\n").unwrap();
    fs::set_permissions(&config, fs::Permissions::from_mode(0o600)).unwrap();

    let denied = run(&dir, &["hello-cmd", "before-grant"]);
    assert!(!denied.status.success(), "command.register must not be implicit");
    assert!(
        String::from_utf8_lossy(&denied.stderr)
            .contains("no enabled plugin has an administrator command.register grant"),
        "{}",
        String::from_utf8_lossy(&denied.stderr)
    );

    fs::write(
        &config,
        "[plugins]\nenabled = [\"registered\"]\n\n[plugins.grants]\nregistered = [\"command.register\"]\n",
    ).unwrap();
    fs::set_permissions(&config, fs::Permissions::from_mode(0o600)).unwrap();

    let output = ok(run(&dir, &["hello-cmd", "after-grant"]));
    assert!(output.contains("automatically_dispatched"), "{output}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn unknown_top_level_command_is_routed_through_plugin_dispatch() {
    let dir = scratch("external");
    let out = run(&dir, &["plugin-only-command", "one", "two words"]);
    assert!(!out.status.success(), "an unregistered dynamic command must fail");
    let error = String::from_utf8_lossy(&out.stderr);
    assert!(
        error.contains("no enabled plugin has an administrator command.register grant"),
        "{error}"
    );
    let _ = fs::remove_dir_all(&dir);
}

fn pdir2(dir: &Path, id: &str) -> PathBuf {
    dir.join("plugins").join(id)
}
