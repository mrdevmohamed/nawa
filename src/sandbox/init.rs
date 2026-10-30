//! Sandbox process orchestration: namespace creation, PID 1 of the new PID
//! namespace, the wall-clock timeout, and the build engine's spawn/sync
//! protocol. Mount recipes live in `mounts.rs`, the confined `unsafe` calls
//! in [`crate::sys`].

use anyhow::{bail, Context, Result};
use rustix::process::{chdir, setrlimit, umask, waitpid, Pid, Rlimit, Resource, WaitOptions};
use rustix::thread::{unshare_unsafe, UnshareFlags};
use std::os::unix::io::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};

use super::mounts;
use super::spec::SandboxSpec;
use super::sys;
use super::{EXIT_TIMEOUT, EXEC_MUST_NOT_RETURN, PID1_SETUP_FAILED};

/// The sync fd the parent (build engine) passes pre_exec; see
/// [sys::sync_announce] for the byte protocol.
pub use crate::sys::SYNC_FD;

// ---------------------------------------------------------------------------
// Entry point (`nawa sandbox-init <spec.toml>`)
// ---------------------------------------------------------------------------

/// Run the spec; returns the exit code the build engine should observe.
pub fn init(path: &Path) -> Result<i32> {
    let spec = SandboxSpec::load(path)?;

    // -- 1. user namespace + identity (§4.3) ------------------------------
    // The ids must be captured BEFORE unshare: inside the unmapped namespace
    // they read back as 65534.
    let euid = sys::geteuid();
    let egid = sys::getegid();

    // SAFETY: single-threaded process, no open file references that a
    // namespace change could observe.
    if let Err(e) = unsafe { unshare_unsafe(UnshareFlags::NEWUSER) } {
        bail!(
            "cannot create a user namespace ({}).\n\
             The kernel needs CONFIG_USER_NS and unprivileged user namespaces \
             enabled (sysctl kernel.unprivileged_userns_clone). Refusing to \
             build without the requested isolation; rerun with --allow-degraded \
             to continue at a lower level",
            std::io::Error::from(e)
        );
    }
    // give the parent a chance to install a ranged map (root runs); the byte
    // arrives after it is done (or never, when invoked manually)
    sys::sync_announce();
    sys::sync_read_gate();
    sys::sync_make_cloexec();
    ensure_identity(euid, egid)?;
    if sys::geteuid() != 0 {
        bail!("no uid mapping installed — builds must run as root inside the sandbox");
    }

    // -- 2. the remaining namespaces --------------------------------------
    let mut flags = UnshareFlags::NEWNS | UnshareFlags::NEWPID | UnshareFlags::NEWIPC | UnshareFlags::NEWUTS;
    if !spec.network {
        flags |= UnshareFlags::NEWNET;
    }
    // SAFETY: as above — single-threaded, at process startup.
    if let Err(e) = unsafe { unshare_unsafe(flags) } {
        bail!(
            "cannot create the mount/pid/ipc/uts namespaces: {}",
            std::io::Error::from(e)
        );
    }

    // -- 3. fork PID 1 -----------------------------------------------------
    let pid = sys::fork()?;
    if pid == 0 {
        // PID 1 of the new pid namespace. Errors are fatal and must not
        // outlive us — exit directly.
        let code = match run_pid1(&spec) {
            Ok(()) => EXEC_MUST_NOT_RETURN, // exec must not return
            Err(e) => {
                eprintln!("nawa: sandbox: {e:#}");
                PID1_SETUP_FAILED
            }
        };
        sys::exit_now(code);
    }

    // -- 4. timeout + reap -------------------------------------------------
    sys::install_timeout(spec.timeout_secs, pid);
    // blocks until PID 1 is reaped; `from_raw` maps the fork pid directly
    // into the wrapper the call needs (pid > 0 on the parent side)
    let status = waitpid(Pid::from_raw(pid), WaitOptions::empty())
        .map_err(|e| anyhow::Error::msg(format!("waitpid: {}", std::io::Error::from(e))))?;
    let st = status
        .ok_or_else(|| anyhow::Error::msg("waitpid: no status (child vanished)"))?
        .1;
    if sys::timed_out() {
        eprintln!(
            "nawa: sandbox: section timed out after {}s (all processes killed)",
            spec.timeout_secs
        );
        return Ok(EXIT_TIMEOUT);
    }
    // the same exit-code convention as timeout(1) consumers observe:
    // the build's own code, 128+signal when killed, or a fixed 125 for
    // "stopped/continued" states (never actually observed — but the old
    // WUNTRACED-less wait could only produce exited/signaled too).
    if st.exited() {
        Ok(st.exit_status().unwrap_or(PID1_SETUP_FAILED))
    } else if st.signaled() {
        Ok(128 + st.terminating_signal().unwrap_or(0))
    } else {
        Ok(PID1_SETUP_FAILED)
    }
}

/// Set up the filesystem as PID 1 and exec the build.
fn run_pid1(spec: &SandboxSpec) -> Result<()> {
    if spec.root.mode == "live" {
        mounts::setup_live_root(spec)?;
    } else {
        mounts::setup_dir_root(spec)?;
    }
    common_prep(spec)?;
    exec_build(spec)
}

// ---------------------------------------------------------------------------
// identity, limits, exec
// ---------------------------------------------------------------------------

fn common_prep(spec: &SandboxSpec) -> Result<()> {
    if !spec.network {
        sys::bring_up_loopback()?;
    }
    // umask 022 (§6): environment variables cannot carry it
    umask(rustix::fs::Mode::from_raw_mode(0o022));
    // RLIMIT_CORE = 0 (§4.5)
    let _ = setrlimit(Resource::Core, Rlimit { current: Some(0), maximum: Some(0) });
    // PR_SET_NO_NEW_PRIVS: even a setuid binary inside cannot regain power
    sys::no_new_privs()?;
    // drop every capability (§4.1): builds need none
    sys::drop_capabilities()?;
    Ok(())
}

fn exec_build(spec: &SandboxSpec) -> Result<()> {
    let cwd = if spec.workdir.is_empty() {
        format!("{}/src", spec.build_inside.trim_end_matches('/'))
    } else {
        spec.workdir.clone()
    };
    // bash inherits the cwd; recipes address their sources relative to it
    if chdir(cwd.as_str()).is_err() {
        // meta packages run directly in an (empty) src dir that must exist
        if let Err(e) = chdir(spec.build_inside.as_str()) {
            bail!("chdir {cwd}: {}", std::io::Error::from(e));
        }
    }
    for shell in ["/usr/bin/bash", "/bin/bash"] {
        // std::process::Command::exec is the safe execve: fixed absolute
        // program path (no PATH search happens for paths containing '/'),
        // argv and the whitelisted environment set exactly like the raw call
        // did (env_clear + explicit vars = empty char*env[] plus our entries).
        let mut cmd = std::process::Command::new(shell);
        cmd.arg("-e").arg("-c").arg(&spec.script);
        cmd.env_clear();
        for (k, v) in &spec.env {
            cmd.env(k, v);
        }
        cmd.env("PWD", cwd.clone());
        // exec never returns on success; on failure std returns the errno
        let err: std::io::Error = cmd.exec();
        if err.raw_os_error() != Some(libc::ENOENT) {
            bail!("exec {shell}: {err}");
        }
    }
    bail!(
        "cannot exec bash at /usr/bin/bash or /bin/bash inside the sandbox root \
         (ENOENT: bash is missing, or its ELF interpreter /lib64/ld-linux-x86-64.so.2 \
         is — check that the build root has the merged-usr links from base-files)"
    )
}

/// After `unshare(CLONE_NEWUSER)`: if the parent did not install a mapping
/// (non-root callers, or manual invocation), self-write the single own-uid
/// map the kernel allows for unprivileged processes.
pub(crate) fn ensure_identity(euid: u32, egid: u32) -> Result<()> {
    let has_map = std::fs::read_to_string("/proc/self/uid_map")
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);
    if has_map {
        return Ok(());
    }

    let _ = std::fs::write("/proc/self/setgroups", b"deny\n");
    let uid_line = format!("0 {euid} 1\n");
    let gid_line = format!("0 {egid} 1\n");
    std::fs::write("/proc/self/uid_map", uid_line)
        .context("write /proc/self/uid_map (single own-uid map)")?;
    std::fs::write("/proc/self/gid_map", gid_line)
        .context("write /proc/self/gid_map")?;
    Ok(())
}

// ---------------------------------------------------------------------------
// §4.3 identity emulation (used by the build engine)
// ---------------------------------------------------------------------------

/// What the §4.3 mapping does to a *host* owner id, as seen from inside the
/// sandbox: the caller's own id becomes 0; real root additionally maps
/// inside 1..65535 to the subordinate range 100000..165534 (`parent_sync`);
/// anything else is unmapped (reads back as 65534 = nobody).
///
/// The mount that assembles a layered build root happens **inside** the
/// sandbox (`run_pid1` → `mounts::mount_layers`), so overlay lowerdirs are
/// resolved by the sandbox's mapped identity — not by whoever ran nawa. A
/// layer cache the user nawa reads happily (a `sudo` run against a `--root`
/// under a mode-700 home directory is the common case) can therefore still
/// be unreachable from the build.
pub fn first_untraversable_ancestor(path: &Path) -> Option<PathBuf> {
    use std::os::unix::fs::MetadataExt;

    let euid = sys::geteuid();
    let egid = sys::getegid();
    let mapped = |x: u32, own: u32| {
        if x == own {
            Some(0)
        } else if euid == 0 && (100000..165535).contains(&x) {
            Some(x - 100000 + 1)
        } else {
            None
        }
    };
    let start = if path.is_absolute() {
        path.to_path_buf()
    } else {
        let cwd = std::env::current_dir().ok()?;
        cwd.join(path)
    };

    // from the deepest component up to "/" — the FIRST offending ancestor is
    // the most useful report. The path itself may not exist yet (a cache dir
    // is created later), but its parents must, and a stat failure here is
    // itself the answer (owning trees the map cannot read).
    for a in start.ancestors() {
        let st = match std::fs::metadata(a) {
            Ok(st) => st,
            Err(_) => return Some(a.to_path_buf()),
        };
        // permission follows the matched class: owner (by the mapped view of
        // the ids — the user namespace's own check does the same) then group
        // then other. Inside uid/gid are both 0 (§4.3).
        let (uid, gid, bits) = (st.uid(), st.gid(), st.mode() & 0o777);
        let x_needed = if mapped(uid, euid) == Some(0) {
            0o100
        } else if mapped(gid, egid) == Some(0) {
            0o010
        } else {
            0o001
        };
        if bits & x_needed == 0 {
            return Some(a.to_path_buf());
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Spawning (used by the build engine)
// ---------------------------------------------------------------------------

/// Spawn `nawa sandbox-init <spec>` with the sync channel on fd 3 and stdout
/// piped for log streaming. Returns the child; the caller keeps the socket
/// end to install the ranged uid map when running as root.
pub fn spawn_init(
    exe: &Path,
    spec_path: &Path,
) -> std::io::Result<(std::process::Child, std::os::unix::net::UnixStream)> {
    use std::os::unix::net::UnixStream;
    let (parent_end, child_end) = UnixStream::pair()?;
    // child side must arrive exactly at fd 3 and close on exec of bash
    let child_fd = child_end.as_raw_fd();

    let mut cmd = std::process::Command::new(exe);
    cmd.arg("sandbox-init").arg(spec_path);
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit());
    // SAFETY: the dup2 target (fd 3) is reserved by both sides of this
    // spawn; `pre_exec` runs in the forked child before exec and is
    // inherently unsafe to call.
    unsafe {
        std::os::unix::process::CommandExt::pre_exec(&mut cmd, move || {
            // SAFETY: see the outer comment; dup2 into the reserved slot can
            // only fail with EBADF/EMFILE, which std surfaces as an error.
            if child_fd != SYNC_FD && libc::dup2(child_fd, SYNC_FD) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = cmd.spawn()?;
    drop(child_end);
    Ok((child, parent_end))
}

/// Parent side of the sync protocol: after `R`, try to install the ranged
/// map (only possible for real root, §4.3: inside 0 → the caller's own id,
/// inside 1..65535 → outside 100000..); then release the child with `G`. The socket is always
/// closed at the end — EOF also releases the child (self-written map).
pub fn parent_sync(child: &std::process::Child, sock: std::os::unix::net::UnixStream) {
    use std::io::{Read as _, Write as _};
    let mut sock = sock;
    let mut buf = [0u8; 1];
    let _ = sock.set_read_timeout(Some(std::time::Duration::from_secs(60)));
    if sock.read(&mut buf).is_ok() && buf[0] == b'R' {
        let pid = child.id();
        // The child process itself keeps the caller's kernel ids. A map that
        // only covers 100000.. leaves them unmapped, so the child would read
        // back as 65534 instead of root. Map inside 0 to the caller's own id
        // (1 id) and the rest of the range to the subordinate block.
        let euid = sys::geteuid();
        let egid = sys::getegid();
        let _ = std::fs::write(
            format!("/proc/{pid}/uid_map"),
            format!("0 {euid} 1\n1 100000 65535\n"),
        );
        let _ = std::fs::write(
            format!("/proc/{pid}/gid_map"),
            format!("0 {egid} 1\n1 100000 65535\n"),
        );
        let _ = sock.write_all(b"G");
        let _ = sock.flush();
    }
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn reachable_tree_is_accepted() {
        // every component owned by the current identity: the §4.3 map sends
        // it to 0, so owner bits speak — a normal 700 tree traverses; the
        // build engine passes an *existing* cache dir (layer_cache_dir has
        // just created it), so use `tmp` itself
        let tmp = std::env::temp_dir().join(format!("nawa-reach-{}-ok", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(first_untraversable_ancestor(&tmp), None);
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn untraversable_component_is_reported() {
        // a directory with NO execute bits for anyone (even its owner) can
        // never be traversed by the mapped identity — owner, group, other: 0
        let tmp = std::env::temp_dir().join(format!("nawa-reach-{}-denied", std::process::id()));
        let inner = tmp.join("layers");
        std::fs::create_dir_all(&inner).unwrap();
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o000)).unwrap();
        let block = first_untraversable_ancestor(&inner);
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::remove_dir_all(&inner).unwrap();
        let block = block.expect("a mode-000 directory must be reported");
        // stat itself already fails for non-root (EACCES at `inner`); root
        // walks further and flags the mode-000 directory by its bits —
        // either way something in the tree is reported
        assert!(
            block.starts_with(&tmp) || block == inner,
            "unexpected report {block:?}"
        );
    }

    #[test]
    fn missing_parents_are_reported() {
        // a cache is created later: not-yet-existing parents cannot be
        // traversed by anyone
        let p = std::env::temp_dir()
            .join(format!("nawa-reach-{}-gone", std::process::id()))
            .join("layers");
        let _ = std::fs::remove_dir_all(&p);
        assert_eq!(first_untraversable_ancestor(&p), Some(p.clone()));
        // nobody created anything — nothing to clean up
    }
}
