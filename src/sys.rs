//! nawa's syscall abstraction layer — the one place where `unsafe` ids live.
//!
//! nawa's first `cargo build --offline` happens from inside a sandboxear,
//! so every subsystem that needs the kernel leans on `rustix` (safe wrappers
//! keep the call sites simple and audit files small); this module exists for
//! the handful of syscalls rustix deliberately does not (or cannot) wrap:
//!
//! * `fork(2)` — rustix refuses to expose it because of its interaction with
//!   threads; nawa's sandbox parent is single-threaded when the children are
//!   forked, and each child runs a fixed setup routine and exits on failure.
//! * `prctl(PR_SET_NO_NEW_PRIVS)` and `capset` — small, fixed-shape calls
//!   with no rustix equivalent; wrapped so the sensitive struct layouts are
//!   in one file, reviewed once.
//! * the wall-clock timeout for the build: an `alarm(2)` + `SIGALRM`
//!   handler that kills PID 1 — a signal handler is intrinsically unsafe
//!   territory, and its async-signal-safe core (kill) uses libc directly.
//! * bringing `lo` up with a raw `SIOCSIFFLAGS` ioctl on a netns socket.
//!
//! Everything else (mounts, unshare, waitpid, rlimits, umask, kill, chroot,
//! pivot_root, O_PATH opens, fd flags) goes through `rustix` at the call
//! sites; see `sandbox::{mounts, init, probe}`.
//!
//! Invariants kept by every function here:
//! * no interior-NUL strings are ever handed to the kernel (paths in the
//!   sandbox come from our own config, verified at parse time);
//! * every error is surfaced as an ordinary `Result`, never a panic.

use anyhow::{bail, Result};
use rustix::fd::BorrowedFd;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

/// The sync fd the parent (build engine) passes as fd 3 to `sandbox-init`.
/// Absent when the process is started directly (tests, debugging), in which
/// case the fd simply has nothing behind it: reads see EOF immediately.
pub const SYNC_FD: i32 = 3;

// ---------------------------------------------------------------------------
// process
// ---------------------------------------------------------------------------

/// `fork(2)` — raw pid: `< 0` = error, `0` = child, `> 0` = child's pid.
///
/// # Safety
/// This call is only made from a **single-threaded** process (`init` at
/// process start, and the probe entry before any other thread exists).
/// The child's very next step is a fixed function that either execs bash or
/// exits; the parent immediately re-waits for the child. No file descriptors
/// are shared into synchronous section locks.
pub fn fork() -> Result<i32> {
    // SAFETY: see the function's contract; plain libc call, no arguments.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        bail!("fork: {}", std::io::Error::last_os_error());
    }
    Ok(pid)
}

/// `_exit(2)` — terminate the child without flushing anything, exactly as a
/// failed exec handler must (std's `exit` would flush and the child of a
/// failed exec must not run buffered destructors).
pub fn exit_now(code: i32) -> ! {
    // SAFETY: constant procedure, no state held.
    unsafe { libc::_exit(code) }
}

/// `prctl(PR_SET_NO_NEW_PRIVS, 1)` — even a setuid binary inside the build
/// cannot regain privilege (design §4.4).
pub fn no_new_privs() -> Result<()> {
    // SAFETY: fixed arguments on a single-threaded process; prctl may only
    // fail with EINVAL, which becomes the error below.
    let rc = unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) };
    if rc != 0 {
        bail!("prctl(NO_NEW_PRIVS): {}", std::io::Error::last_os_error());
    }
    Ok(())
}

// --- capabilities (§4.1: builds are dropped of every capability) ---

const LINUX_CAPABILITY_VERSION_3: u32 = 0x2008_0522;

#[repr(C)]
struct CapUserHeader {
    version: u32,
    pid: i32,
}
#[repr(C)]
#[derive(Clone, Copy)]
struct CapUserData {
    effective: u32,
    permitted: u32,
    inheritable: u32,
}

/// `capset` with an all-zero set: drop every capability of this thread. The
/// struct layouts are fixed ABI (linux/capability.h); `_LINUX_CAPABILITY_...`
/// v3 carries two `cap_user_data` words.
pub fn drop_capabilities() -> Result<()> {
    let hdr = CapUserHeader { version: LINUX_CAPABILITY_VERSION_3, pid: 0 };
    let data = [CapUserData { effective: 0, permitted: 0, inheritable: 0 }; 2];
    // SAFETY: valid, correctly-aligned pointers to our own the initialized
    // structs of fixed size; SYS_capset reads exactly them and never
    // succeeds-with-side-effects on failure — checked below.
    let rc = unsafe {
        libc::syscall(libc::SYS_capset, &hdr as *const CapUserHeader, data.as_ptr())
    };
    if rc != 0 {
        // capset(2) is a bare syscall (no libc wrapper); the error mapping
        // deserves a comment: we only ever call it after NO_NEW_PRIVS in the
        // PID 1 of our own namespace, so any failure is kernel/ABI refusal
        bail!("capset: {}", std::io::Error::last_os_error());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// identity / sync-fd quick probes (borrowed fd, guarded by the caller)
// ---------------------------------------------------------------------------

/// The caller's effective uid, for the user-namespace mapping policy (§4.3).
pub fn geteuid() -> u32 {
    // SAFETY: trivial value read; libc's geteuid has no failure mode.
    unsafe { libc::geteuid() }
}

/// The caller's effective gid — same contract as [`geteuid`].
pub fn getegid() -> u32 {
    // SAFETY: trivial value read; libc's getegid has no failure mode.
    unsafe { libc::getegid() }
}

// ---------------------------------------------------------------------------
// the alarm-based section timeout (§4.5)
// ---------------------------------------------------------------------------

static PID1: AtomicI32 = AtomicI32::new(0);
static TIMED_OUT: AtomicBool = AtomicBool::new(false);

/// True once the alarm has fired in this process.
pub fn timed_out() -> bool {
    TIMED_OUT.load(Ordering::Relaxed)
}

/// Alarm-based timeout: kills PID 1, whose death makes the kernel reap the
/// whole PID namespace (§4.5). `secs == 0` (no limit) arms nothing.
pub fn install_timeout(secs: u64, pid1: i32) {
    if secs == 0 {
        return;
    }
    PID1.store(pid1, Ordering::Relaxed);
    unsafe {
        // SAFETY: installing the handler BEFORE any signal can arrive (the
        // alarm is armed right after); the handler uses only async-signal-
        // safe libc calls below.
        libc::signal(libc::SIGALRM, on_alarm as *const () as libc::sighandler_t);
        libc::alarm(secs as libc::c_uint);
    }
}

extern "C" fn on_alarm(_sig: libc::c_int) {
    TIMED_OUT.store(true, Ordering::Relaxed);
    let pid = PID1.load(Ordering::Relaxed);
    if pid > 0 {
        // SAFETY: async-signal-safe libc call (SysV guarantees only `kill`
        // among the ones used here); nothing is allocated or unlocked.
        unsafe { libc::kill(pid, libc::SIGKILL) };
    }
}

// ---------------------------------------------------------------------------
// loopback bring-up inside a fresh network namespace (§4.4)
// ---------------------------------------------------------------------------

/// Bring the network namespace's loopback interface up (`IFF_UP` on `lo`):
/// many test suites need 127.0.0.1 even with no other connectivity. There is
/// no portable safe wrapper for `SIOCSIFFLAGS`; this is the one raw ioctl.
pub fn bring_up_loopback() -> Result<()> {
    #[repr(C)]
    struct IfReq {
        name: [u8; 16],
        flags: i16,
        _pad: [u8; 22], // struct ifreq is 40 bytes
    }
    // SAFETY: socket(AF_INET, SOCK_DGRAM) for an interface-settings ioctl;
    // the fd is closed on every path below.
    let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0) };
    if fd < 0 {
        bail!("socket: {}", std::io::Error::last_os_error());
    }
    let ifr = IfReq {
        name: *b"lo\0\0\0\0\0\0\0\0\0\0\0\0\0\0",
        flags: libc::IFF_UP as i16,
        _pad: [0; 22],
    };
    // SAFETY: `ifr` is a 40-byte `ifreq` as the kernel expects for
    // SIOCSIFFLAGS; the ioctl either sets the flag or fails with no side
    // effect on our memory.
    let ok = unsafe { libc::ioctl(fd, libc::SIOCSIFFLAGS as _, &ifr) } == 0;
    let err = std::io::Error::last_os_error();
    // SAFETY: fd is an owned socket as above.
    unsafe { libc::close(fd) };
    if !ok {
        bail!("bring up loopback: {err}");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// parent/child sync protocol (the `R` … `G` handshake)
// ---------------------------------------------------------------------------

/// Announce readiness to the parent ("R" on the sync fd; a failed write means
/// there is no parent end — direct invocation).
pub fn sync_announce() {
    // SAFETY: fd 3 is our sync socket for the duration of the handshake.
    let fd = unsafe { BorrowedFd::borrow_raw(SYNC_FD) };
    let _ = rustix::io::write(fd, b"R");
}

/// Block until the parent (which may install a ranged uid map) answers with
/// `G`, or EOF — both mean "proceed" (the child writes its own map).
pub fn sync_read_gate() {
    // SAFETY: same borrowed fd as `sync_announce`.
    let fd = unsafe { BorrowedFd::borrow_raw(SYNC_FD) };
    let mut buf = [0u8; 1];
    let _ = rustix::io::read(fd, &mut buf);
}

/// The sync fd must not leak into exec'd bash — `FD_CLOEXEC`.
pub fn sync_make_cloexec() {
    // SAFETY: same borrowed fd as `sync_announce`.
    let fd = unsafe { BorrowedFd::borrow_raw(SYNC_FD) };
    let _ = rustix::io::fcntl_setfd(fd, rustix::io::FdFlags::CLOEXEC);
}
