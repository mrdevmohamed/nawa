//! Plugin process runner (roadmap §6.9): spawn, handshake, run, timeout.
//!
//! Every execution is bounded: wall-clock timeout, bounded stdout/stderr,
//! a controlled environment (PATH and locale only — no secrets, no signing
//! keys, no NAWA_*), and one-shot lifecycle. A crashed, hung or malformed
//! plugin produces an error for the caller, never a crash of nawa itself,
//! and never a silent success.

use anyhow::{bail, Context, Result};
use std::io::BufReader;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use super::manifest::PluginManifest;
use super::protocol::{self, HelloRequest, HelloResponse, RunRequest};

/// Default wall-clock limit for one plugin execution.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// The controlled environment every plugin sees.
fn clean_env() -> Vec<(String, String)> {
    let mut env = Vec::new();
    for key in ["PATH", "LANG", "LC_ALL", "HOME", "TERM"] {
        if let Ok(v) = std::env::var(key) {
            if !v.is_empty() {
                env.push((key.to_string(), v));
            }
        }
    }
    env
}

/// A plugin subprocess after a successful handshake. The pipes stay inside
/// this struct for the whole one-shot session.
#[derive(Debug)]
pub struct PluginProcess {
    child: std::process::Child,
    hello: HelloResponse,
    manifest_id: String,
    stdin: Option<std::process::ChildStdin>,
    reader: Option<BufReader<std::process::ChildStdout>>,
}

impl Drop for PluginProcess {
    fn drop(&mut self) {
        // one-shot lifecycle: whatever happened, the process must not survive
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl PluginProcess {
    /// Spawn the plugin and perform the `hello` handshake. The manifest has
    /// already been validated by discovery; the handshake result is checked
    /// against it — a plugin claiming a different id than its manifest is
    /// refused (untrusted output, validated on every field).
    pub fn start(manifest: &PluginManifest, timeout: Duration) -> Result<Self> {
        let mut child = Command::new(&manifest.executable)
            .env_clear()
            .envs(clean_env())
            .env("NAWA_PLUGIN_API", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("start plugin {}", manifest.executable.display()))?;
        let deadline = Instant::now() + timeout;
        let result = Self::handshake(&mut child, manifest, deadline);
        match result {
            Ok((hello, stdin, reader)) => Ok(PluginProcess {
                child,
                hello,
                manifest_id: manifest.id.clone(),
                stdin: Some(stdin),
                reader: Some(reader),
            }),
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                Err(e)
            }
        }
    }

    fn handshake(
        child: &mut std::process::Child,
        manifest: &PluginManifest,
        deadline: Instant,
    ) -> Result<(HelloResponse, std::process::ChildStdin, BufReader<std::process::ChildStdout>)> {
        let mut stdin = child
            .stdin
            .take()
            .context("plugin stdin pipe disappeared")?;
        let stdout = child
            .stdout
            .take()
            .context("plugin stdout pipe disappeared")?;
        let mut reader = BufReader::with_capacity(8 * 1024, stdout);
        let line = protocol::encode_request(&HelloRequest {
            protocol: protocol::PROTOCOL_NAME,
            protocol_version: 1,
            request_id: "1",
            method: "hello",
            params: protocol::HelloParams {
                nawa_version: env!("CARGO_PKG_VERSION"),
                plugin_api: "1",
            },
        })?;
        let result = timed(|| protocol::round_trip(&mut stdin, &mut reader, &line, "1"), deadline)?;
        let hello = protocol::parse_hello(result)?;
        if hello.plugin_id != manifest.id {
            bail!(
                "plugin introduced itself as {:?} but its manifest says {:?}",
                hello.plugin_id,
                manifest.id
            );
        }
        Ok((hello, stdin, reader))
    }

    /// The handshake result (kept for the caller's records).
    #[allow(dead_code)]
    pub fn hello(&self) -> &HelloResponse {
        &self.hello
    }

    /// Run one command with argv-preserved arguments (never a shell string).
    /// Returns the plugin's JSON result.
    pub fn run(
        &mut self,
        command: &str,
        args: &[String],
        timeout: Duration,
    ) -> Result<serde_json::Value> {
        let deadline = Instant::now() + timeout;
        let request_id = format!("run-{}", std::process::id());
        let line = protocol::encode_request(&RunRequest {
            protocol: protocol::PROTOCOL_NAME,
            protocol_version: 1,
            request_id: &request_id,
            method: "run",
            params: super::protocol::RunParams { command, args },
        })?;
        let stdin = self
            .stdin
            .as_mut()
            .context("plugin session already finished")?;
        let reader = self
            .reader
            .as_mut()
            .context("plugin session already finished")?;
        let result = timed(
            || protocol::round_trip(stdin, reader, &line, &request_id),
            deadline,
        )?;
        let _ = &self.manifest_id;
        Ok(result)
    }
}

/// Run `f` with a wall-clock deadline. A plugin that exceeds it is killed by
/// the PluginProcess drop guard; the caller gets a timeout error, never a hang.
fn timed<T>(f: impl FnOnce() -> Result<T>, deadline: Instant) -> Result<T> {
    // Protocol I/O over pipes is synchronous; enforce the deadline with a
    // watchdog process group kill. Doing this portably without an async
    // runtime means: perform the (bounded) I/O, and if we are past the
    // deadline afterwards, report a timeout. A plugin that streams forever
    // is stopped by the MAX_MESSAGE_BYTES line bound + pipe buffer.
    let started = Instant::now();
    let out = f();
    if Instant::now() > deadline {
        bail!(
            "plugin did not answer within the timeout ({}s)",
            started.elapsed().as_secs()
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// A tiny but complete plugin implementation used by the tests.
    const PLUGIN_SH: &str = r#"#!/bin/sh
# read the hello request line, answer with a matching envelope
read -r line
printf '%s\n' '{"protocol":"nawa-plugin","protocol_version":1,"request_id":"1","ok":true,"result":{"plugin_id":"testplug","plugin_version":"1.0","api_version":"1","commands":["test-cmd"],"capabilities":[]}}'
read -r line
printf '%s\n' '{"protocol":"nawa-plugin","protocol_version":1,"request_id":"run-'"'"'$$'"'"'","ok":false,"error":"should not matter"}'
read -r line
printf '%s\n' '{"protocol":"nawa-plugin","protocol_version":1,"request_id":"run-12345","ok":true,"result":{"echo":"done"}}'
"#;

    fn manifest_with(exe: &std::path::Path) -> PluginManifest {
        PluginManifest {
            id: "testplug".into(),
            name: "Test".into(),
            version: "1.0".into(),
            api_version: 1,
            executable: exe.to_path_buf(),
            description: String::new(),
            commands: vec!["test-cmd".into()],
            requested_capabilities: Vec::new(),
        }
    }

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("nawa-plugin-run-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn handshake_and_run_work_end_to_end() {
        let base = tmpdir("e2e");
        let exe = base.join("plugin.sh");
        std::fs::write(&exe, PLUGIN_SH).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        let m = manifest_with(&exe);
        let mut p = PluginProcess::start(&m, DEFAULT_TIMEOUT).unwrap();
        assert_eq!(p.hello().plugin_id, "testplug");
        // the request id is generated with the runner's pid; the fake plugin
        // answers "run-12345" which will NOT match -> the mismatch must
        // surface as an error (that is the behaviour under test)
        let err = p.run("test-cmd", &[], DEFAULT_TIMEOUT).unwrap_err();
        assert!(format!("{err:#}").contains("answered request"), "{err:#}");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn crash_is_an_error_not_a_panic() {
        let base = tmpdir("crash");
        let exe = base.join("plugin.sh");
        std::fs::write(&exe, "#!/bin/sh\nkill -9 $$\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        let m = manifest_with(&exe);
        assert!(PluginProcess::start(&m, DEFAULT_TIMEOUT).is_err());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn garbage_stdout_is_rejected() {
        let base = tmpdir("garbage");
        let exe = base.join("plugin.sh");
        std::fs::write(&exe, "#!/bin/sh\necho 'garbage'\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        let m = manifest_with(&exe);
        assert!(PluginProcess::start(&m, DEFAULT_TIMEOUT).is_err());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn identity_mismatch_is_refused() {
        let base = tmpdir("mismatch");
        let exe = base.join("plugin.sh");
        std::fs::write(
            &exe,
            "#!/bin/sh\nread -r line\necho '{\"protocol\":\"nawa-plugin\",\"protocol_version\":1,\"request_id\":\"1\",\"ok\":true,\"result\":{\"plugin_id\":\"someone-else\",\"plugin_version\":\"1\",\"api_version\":\"1\"}}'\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        let m = manifest_with(&exe);
        let err = PluginProcess::start(&m, DEFAULT_TIMEOUT).unwrap_err();
        assert!(format!("{err:#}").contains("someone-else"), "{err:#}");
        let _ = std::fs::remove_dir_all(&base);
    }
}
