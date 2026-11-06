//! Plugin runner with a strict out-of-process sandbox.
//!
//! Every invocation reuses the namespace sandbox used for source builds.
//! There is no unsandboxed fallback: user/mount/PID/network namespaces,
//! read-only host filesystem, masked Nawa secrets and user state, a dropped
//! identity for privileged Nawa, no_new_privs, dropped capabilities, resource
//! limits, bounded JSONL messages, and real wall-clock timeouts are required.

use anyhow::{bail, Context, Result};
use serde::Serialize;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use super::manifest::{Capability, PluginManifest};
use super::protocol::{self, EventRequest, HelloRequest, HelloResponse, RunRequest};

/// Default wall-clock limit for each plugin handshake, command, or event.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
static REQUEST_SEQUENCE: AtomicU64 = AtomicU64::new(1);

type PendingWrite = (Vec<u8>, mpsc::Sender<std::result::Result<(), String>>);

/// A live sandboxed plugin process. Worker threads exclusively own its pipes,
/// allowing the host to enforce a timeout even when the plugin stops reading
/// stdin or never writes a response.
pub struct PluginProcess {
    child: Child,
    requests: mpsc::Sender<PendingWrite>,
    responses: mpsc::Receiver<Result<protocol::Envelope>>,
    stderr_tail: Arc<Mutex<Vec<u8>>>,
    sandbox_dir: PathBuf,
    hello: HelloResponse,
}

impl Drop for PluginProcess {
    fn drop(&mut self) {
        // Killing the supervisor triggers PR_SET_PDEATHSIG in namespace PID 1.
        // PID namespace teardown then kills the plugin and all its children.
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.sandbox_dir);
    }
}

impl PluginProcess {
    /// Compatibility entry point for tests and callers that need no grants.
    /// It is still sandboxed; this never launches the plugin in the host.
    pub fn start(manifest: &PluginManifest, timeout: Duration) -> Result<Self> {
        Self::start_with_policy(manifest, &[], &[], timeout)
    }

    /// Start a plugin with the effective grants and host paths that must be
    /// hidden from its sandbox. Requested manifest capabilities are not used
    /// unless the administrator has granted them.
    pub fn start_with_policy(
        manifest: &PluginManifest,
        granted: &[Capability],
        protected_paths: &[PathBuf],
        timeout: Duration,
    ) -> Result<Self> {
        let sandboxed = super::sandbox::spawn(&manifest.executable, protected_paths)?;
        let mut child = sandboxed.child;
        let stdin = child.stdin.take().context("plugin sandbox stdin pipe disappeared")?;
        let stdout = child.stdout.take().context("plugin sandbox stdout pipe disappeared")?;
        let stderr = child.stderr.take().context("plugin sandbox stderr pipe disappeared")?;

        let (request_tx, request_rx) = mpsc::channel::<PendingWrite>();
        spawn_writer(stdin, request_rx);

        let (response_tx, responses) = mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let mut reader = std::io::BufReader::with_capacity(8 * 1024, stdout);
            loop {
                let result = protocol::read_response(&mut reader);
                let stop = result.is_err();
                if response_tx.send(result).is_err() || stop {
                    break;
                }
            }
        });

        let stderr_tail = Arc::new(Mutex::new(Vec::new()));
        spawn_stderr_reader(stderr, Arc::clone(&stderr_tail));

        let mut process = Self {
            child,
            requests: request_tx,
            responses,
            stderr_tail,
            sandbox_dir: sandboxed.workdir,
            hello: HelloResponse {
                plugin_id: String::new(),
                plugin_version: String::new(),
                api_version: String::new(),
                commands: Vec::new(),
                capabilities: Vec::new(),
            },
        };

        let granted_names: Vec<String> =
            granted.iter().map(|capability| capability.as_str().to_string()).collect();
        let request = HelloRequest {
            protocol: protocol::PROTOCOL_NAME,
            protocol_version: 1,
            request_id: "hello-1",
            method: "hello",
            params: protocol::HelloParams {
                nawa_version: env!("CARGO_PKG_VERSION"),
                plugin_api: "1",
                granted_capabilities: &granted_names,
            },
        };
        let result = process.exchange(&request, "hello-1", timeout)?;
        let hello = protocol::parse_hello(result)?;
        if hello.plugin_id != manifest.id {
            bail!(
                "plugin introduced itself as {:?} but its manifest says {:?}",
                hello.plugin_id,
                manifest.id
            );
        }
        if hello.api_version != manifest.api_version.to_string() {
            bail!(
                "plugin {} answered API version {:?}; manifest declares {}",
                manifest.id,
                hello.api_version,
                manifest.api_version
            );
        }
        let mut commands = hello.commands.clone();
        commands.sort();
        commands.dedup();
        if commands != manifest.commands {
            bail!("plugin {} handshake command list does not match its manifest", manifest.id);
        }
        process.hello = hello;
        Ok(process)
    }

    fn exchange<T: Serialize>(
        &mut self,
        request: &T,
        request_id: &str,
        timeout: Duration,
    ) -> Result<serde_json::Value> {
        let line = protocol::encode_request(request)?.into_bytes();
        let (written_tx, written_rx) = mpsc::channel();
        self.requests
            .send((line, written_tx))
            .context("plugin input worker stopped")?;

        let started = std::time::Instant::now();
        match written_rx.recv_timeout(timeout) {
            Ok(Ok(())) => {}
            Ok(Err(error)) => bail!("write plugin request: {error}"),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let _ = self.child.kill();
                bail!("plugin stopped reading input before the {} second timeout", timeout.as_secs());
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                bail!("plugin input worker exited unexpectedly");
            }
        }

        let remaining = timeout.saturating_sub(started.elapsed());
        let envelope = match self.responses.recv_timeout(remaining) {
            Ok(Ok(envelope)) => envelope,
            Ok(Err(error)) => return Err(error).context("read plugin response"),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let _ = self.child.kill();
                let detail = self.stderr_summary();
                bail!(
                    "plugin did not answer within {} seconds{}",
                    timeout.as_secs(),
                    detail
                );
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                let detail = self.stderr_summary();
                bail!("plugin response channel closed unexpectedly{detail}");
            }
        };
        protocol::expect_result(envelope, request_id)
    }

    fn stderr_summary(&self) -> String {
        let Ok(saved) = self.stderr_tail.lock() else { return String::new() };
        if saved.is_empty() {
            String::new()
        } else {
            format!("; plugin stderr: {}", String::from_utf8_lossy(&saved).trim())
        }
    }

    pub fn hello(&self) -> &HelloResponse {
        &self.hello
    }

    /// Arguments stay an argv-like JSON array; no shell command is assembled.
    pub fn run(
        &mut self,
        command: &str,
        args: &[String],
        timeout: Duration,
    ) -> Result<serde_json::Value> {
        let request_id = format!(
            "run-{}-{}-{}",
            std::process::id(),
            self.hello.plugin_id,
            REQUEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        );
        let request = RunRequest {
            protocol: protocol::PROTOCOL_NAME,
            protocol_version: 1,
            request_id: &request_id,
            method: "run",
            params: super::protocol::RunParams { command, args },
        };
        self.exchange(&request, &request_id, timeout)
    }

    /// Deliver a typed post-transaction event over the same validated JSONL
    /// envelope used for command responses.
    pub fn notify_event(
        &mut self,
        event_type: &str,
        payload: &serde_json::Value,
        timeout: Duration,
    ) -> Result<serde_json::Value> {
        let request_id = format!(
            "event-{}-{}-{}",
            std::process::id(),
            self.hello.plugin_id,
            REQUEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        );
        let request = EventRequest {
            protocol: protocol::PROTOCOL_NAME,
            protocol_version: 1,
            request_id: &request_id,
            method: "event",
            params: super::protocol::EventParams { event_type, payload },
        };
        self.exchange(&request, &request_id, timeout)
    }
}

fn spawn_writer(stdin: ChildStdin, requests: mpsc::Receiver<PendingWrite>) {
    std::thread::spawn(move || {
        let mut stdin = stdin;
        while let Ok((line, reply)) = requests.recv() {
            let result = stdin
                .write_all(&line)
                .and_then(|_| stdin.write_all(b"\n"))
                .and_then(|_| stdin.flush())
                .map_err(|error| error.to_string());
            let stop = result.is_err();
            let _ = reply.send(result);
            if stop {
                break;
            }
        }
    });
}

fn spawn_stderr_reader(stderr: std::process::ChildStderr, tail: Arc<Mutex<Vec<u8>>>) {
    std::thread::spawn(move || {
        let mut stderr = stderr;
        let mut buffer = [0u8; 4096];
        loop {
            match stderr.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(size) => {
                    if let Ok(mut saved) = tail.lock() {
                        let remaining = (16 * 1024usize).saturating_sub(saved.len());
                        if remaining > 0 {
                            saved.extend_from_slice(&buffer[..size.min(remaining)]);
                        }
                    }
                    // Continue draining after the diagnostic cap to prevent
                    // a full stderr pipe from blocking the plugin process.
                }
            }
        }
    });
}
