//! Strict plugin sandbox adapter built on Nawa's existing namespace sandbox.
//!
//! Host paths used by the plugin are explicitly staged. The host root is
//! read-only, Nawa and user secrets are masked, the executable bind mount is
//! read-only, networking is disabled, and privileged invocations drop to a
//! subordinate uid/gid before executing plugin code.

use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::os::unix::io::AsRawFd;
use std::os::unix::process::CommandExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::sandbox::spec::{BuildPaths, RootSpec, SandboxSpec};

pub(super) struct SandboxedChild {
    pub(super) child: Child,
    pub(super) cleanup: WorkDir,
}

pub(super) struct WorkDir(PathBuf);

impl Drop for WorkDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub(super) fn spawn(executable: &Path, protected_paths: &[PathBuf]) -> Result<SandboxedChild> {
    let executable = std::fs::canonicalize(executable)
        .with_context(|| format!("resolve plugin executable {}", executable.display()))?;
    let source_dir = executable.parent()
        .ok_or_else(|| anyhow::anyhow!("plugin executable has no parent directory"))?;
    let filename = executable.file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow::anyhow!("plugin executable file name is not valid UTF-8"))?
        .to_string();

    let guard = WorkDir(create_workdir()?);
    let workdir = guard.0.clone();
    let build_dir = workdir.join("build");
    let pkg_dir = workdir.join("pkg");
    let home_dir = workdir.join("home");
    let scratch_dir = workdir.join("scratch");
    for path in [&build_dir, &pkg_dir, &home_dir, &scratch_dir] {
        std::fs::create_dir_all(path)?;
        let mode = if path == &home_dir { 0o777 } else { 0o755 };
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    }

    let mut env = BTreeMap::new();
    env.insert("PATH".to_string(), "/usr/bin:/bin".to_string());
    env.insert("HOME".to_string(), "/build/home".to_string());
    env.insert("NAWA_PLUGIN_API".to_string(), "1".to_string());
    for key in ["LANG", "LC_ALL", "TERM"] {
        if let Ok(value) = std::env::var(key) {
            if !value.is_empty() {
                env.insert(key.to_string(), value);
            }
        }
    }

    let mut mask = vec![
        "/root".to_string(),
        // This kernel pseudo-filesystem cannot be remounted read-only with
        // bind-remount semantics in several user-namespace kernels. It is not
        // needed by plugins, so hide it with the other host-sensitive paths.
        "/proc/sys/fs/binfmt_misc".to_string(),
        "/home".to_string(),
        "/run".to_string(),
        "/etc/nawa".to_string(),
        "/etc/ssh".to_string(),
        "/etc/wireguard".to_string(),
        "/etc/NetworkManager/system-connections".to_string(),
        "/etc/ssl/private".to_string(),
        "/var/lib".to_string(),
        "/var/cache".to_string(),
        "/var/log".to_string(),
    ];
    if let Ok(home) = std::env::var("HOME") {
        push_mask_dir(Path::new(&home), &mut mask);
    }
    for path in protected_paths {
        push_mask_dir(path, &mut mask);
    }
    mask.sort();
    mask.dedup();

    // A privileged Nawa maps inside uid 65534 to the subordinate uid range.
    // An unprivileged Nawa has only its own uid mapped as namespace uid 0;
    // it is already unprivileged outside the namespace and loses all caps.
    let privileged = crate::diag::is_root();
    // /tmp is replaced with a private tmpfs by overlay_scratch before
    // build_inside is created. Keeping the plugin staging base there avoids
    // requiring write access to a host-level /build directory.
    let build_inside = "/tmp/nawa-plugin".to_string();
    let plugin_src_inside = format!("{build_inside}/src");
    let plugin_home_inside = format!("{build_inside}/home");
    let spec = SandboxSpec {
        version: 1,
        root: RootSpec::default(),
        build: BuildPaths {
            src_host: source_dir.to_string_lossy().to_string(),
            pkg_host: pkg_dir.to_string_lossy().to_string(),
            home_host: home_dir.to_string_lossy().to_string(),
            scratch: scratch_dir.to_string_lossy().to_string(),
        },
        env,
        script: format!("exec {}", shell_quote(&format!("{plugin_src_inside}/{filename}"))),
        timeout_secs: 30,
        network: false,
        sysfs: false,
        allow_degraded: false,
        mask,
        build_inside: build_inside.clone(),
        workdir: String::new(),
        run_uid: privileged.then_some(65534),
        run_gid: privileged.then_some(65534),
        memory_limit_bytes: 1024 * 1024 * 1024,
        process_limit: 32,
        file_size_limit_bytes: 64 * 1024 * 1024,
        open_files_limit: 64,
        readonly_paths: vec![plugin_src_inside],
        extra_binds: Vec::new(),
        strict_readonly_root: true,
        // /dev is a private tmpfs populated by setup_dev; leave its mount
        // writable for normal device-node opens while directory permissions
        // still prevent the unprivileged plugin from creating nodes.
        writable_paths: vec![
            "/tmp".to_string(),
            plugin_home_inside,
            "/dev".to_string(),
        ],
    };
    let spec_path = workdir.join("plugin-sandbox.toml");
    std::fs::write(&spec_path, toml::to_string(&spec)?)?;
    std::fs::set_permissions(&spec_path, std::fs::Permissions::from_mode(0o644))?;

    let nawa = std::env::current_exe().context("locate nawa for plugin sandbox")?;
    let (parent_end, child_end) = UnixStream::pair().context("create plugin sandbox sync socket")?;
    let child_fd = child_end.as_raw_fd();
    let parent_pid = std::process::id() as libc::pid_t;
    let mut command = Command::new(nawa);
    command
        .arg("sandbox-init")
        .arg(&spec_path)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    // FD 3 is the one-use uid-map handshake socket. Close all other inherited
    // descriptors before entering the user namespace; it is a Linux-only
    // sandbox and close_range is mandatory for this security boundary.
    unsafe {
        command.pre_exec(move || {
            // The sandbox supervisor must not outlive the Nawa process that
            // owns the plugin session. Set this before exec and verify the
            // parent did not die in the fork/pre_exec race.
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::getppid() != parent_pid {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Interrupted,
                    "Nawa parent exited while starting plugin sandbox",
                ));
            }
            if child_fd != 3 && libc::dup2(child_fd, 3) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            // SAFETY: close every inherited descriptor >= 4 in the forked
            // child; stdin/stdout/stderr and the sync fd are retained.
            if libc::syscall(libc::SYS_close_range, 4u32, u32::MAX, 0u32) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let child = command.spawn().context("start plugin namespace sandbox")?;
    drop(child_end);
    crate::sandbox::parent_sync(&child, parent_end);
    Ok(SandboxedChild { child, cleanup: guard })
}

fn create_workdir() -> Result<PathBuf> {
    for attempt in 0..32u32 {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH)
            .unwrap_or_default().as_nanos();
        let path = std::env::temp_dir().join(format!(
            "nawa-plugin-{}-{nanos}-{attempt}",
            std::process::id()
        ));
        match std::fs::create_dir(&path) {
            Ok(()) => {
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
                return Ok(path);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error).context("create plugin sandbox directory"),
        }
    }
    anyhow::bail!("could not allocate a unique plugin sandbox directory")
}

fn push_mask_dir(path: &Path, mask: &mut Vec<String>) {
    let Ok(resolved) = std::fs::canonicalize(path) else { return };
    let dir = if resolved.is_dir() {
        resolved
    } else {
        let Some(parent) = resolved.parent() else { return };
        parent.to_path_buf()
    };
    if dir == Path::new("/") {
        return;
    }
    let value = dir.to_string_lossy().to_string();
    if !mask.iter().any(|known| known == &value) {
        mask.push(value);
    }
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}
