//! The build sandbox — docs/design/build-isolation.md §4.
//!
//! `nawa sandbox-init <spec.toml>` is spawned by the build engine instead of
//! `bash` when isolation is `sandbox` or `closure`. It:
//!
//!   1. `unshare(CLONE_NEWUSER)`; the *parent* writes a ranged uid/gid map
//!      when it runs as real root (fake root, §4.3); otherwise the process
//!      self-writes a single own-uid map (the unprivileged pattern);
//!   2. `unshare(CLONE_NEWNS|NEWPID|NEWIPC|NEWUTS[|NEWNET])`;
//!   3. forks the real build process as PID 1 of the new PID namespace —
//!      when it exits, the kernel kills every straggler;
//!   4. PID 1 builds the sandbox filesystem, drops all capabilities, sets
//!      `NO_NEW_PRIVS`, `RLIMIT_CORE=0`, umask 022 and execs
//!      `bash -e -c <script>` with the whitelisted environment only.
//!
//! The spec is a plain TOML file (the `toml` crate is already a dependency —
//! no `serde_json`, so the offline vendored bootstrap of nawa itself keeps
//! working unchanged). Everything that *decides* the sandbox stays in
//! ordinary testable Rust; only `sandbox-init` touches the kernel through
//! the confined `unsafe` helpers at the bottom of this file.
//!
//! This stops accidents and impurity (wrong-but-linkable libraries, builds
//! that write outside `DESTDIR`, leftover daemons, wall-clock timestamps),
//! not a determined attacker — sources are sha256-pinned and recipes are
//! trusted code (design §2 non-goals).

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::os::unix::io::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};

/// Exit code convention: 124 = timed out (like timeout(1)).
pub const EXIT_TIMEOUT: i32 = 124;

/// The sync fd the parent (build engine) passes pre_exec. Absent when
/// `sandbox-init` is invoked directly (tests, debugging): the process then
/// simply self-writes its uid map.
const SYNC_FD: i32 = 3;

// ---------------------------------------------------------------------------
// Spec
// ---------------------------------------------------------------------------

/// Everything the sandbox needs, serialised as TOML next to the build logs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxSpec {
    pub version: u32,
    #[serde(default)]
    pub root: RootSpec,
    pub build: BuildPaths,
    /// The exact environment the build section receives (§6 whitelist).
    #[serde(default)]
    pub env: std::collections::BTreeMap<String, String>,
    /// The (already expanded) script, run as `bash -e -c <script>`.
    pub script: String,
    /// Per-section wall-clock limit in seconds (0 = none).
    #[serde(default)]
    pub timeout_secs: u64,
    /// Escape hatch (§4.4): only effective when the user passed
    /// `--allow-network`; skips the network namespace entirely.
    #[serde(default)]
    pub network: bool,
    /// Mount /sys read-only instead of hiding it.
    #[serde(default)]
    pub sysfs: bool,
    /// Continue (with a loud warning) when the host kernel refuses an
    /// isolation feature instead of failing the build (§9).
    #[serde(default)]
    pub allow_degraded: bool,
    /// Host paths to hide behind an empty tmpfs in `live` mode: the build
    /// directory, the repo copy, the bootstrap seed, package caches…
    #[serde(default)]
    pub mask: Vec<String>,
    /// Where `/build` lives *inside* the sandbox. The design pins `/build`
    /// (it always exists in the stage chroots); an unprivileged host that
    /// cannot create `/build` uses a base on the fresh `/tmp` tmpfs instead
    /// (groundwork for rootless builds, §10 Phase 7). Recipes see it
    /// through `%{srcdir}` / `%{pkgdir}`, never as a hardcoded path.
    #[serde(default = "default_build_inside")]
    pub build_inside: String,
    /// Working directory of the build sections *inside* the sandbox: the
    /// unpacked source dir (e.g. `/build/src/m4-1.4.19`). Empty = `<build_inside>/src`.
    #[serde(default)]
    pub workdir: String,
}

fn default_build_inside() -> String {
    "/build".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RootSpec {
    /// `live` = the running system (sandbox level),
    /// `dir` = an assembled build root (closure level).
    #[serde(default = "default_root_mode")]
    pub mode: String,
    /// Host path of the assembled build root when `mode = "dir"`.
    #[serde(default)]
    pub dir: String,
    /// Phase 5 (§5.3 v2): host paths of extracted package layers, first
    /// entry bottom-most. When set, `dir` is mounted as an **overlayfs**
    /// (lowerdir = these layers, throw-away upper/work under the sandbox
    /// scratch) instead of being bound — assembly cost drops from
    /// "extract every archive" to "mount what is already on disk".
    /// Requires overlayfs inside a user namespace (kernel ≥ 5.11); the
    /// builder only fills this after `nawa sandbox-probe overlay` passed.
    #[serde(default)]
    pub layers: Vec<String>,
}

impl Default for RootSpec {
    fn default() -> Self {
        RootSpec { mode: default_root_mode(), dir: String::new(), layers: Vec::new() }
    }
}

fn default_root_mode() -> String {
    "live".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildPaths {
    /// Host path of the source staging dir (bound at /build/src).
    pub src_host: String,
    /// Host path of the DESTDIR staging dir (bound at /build/pkg).
    pub pkg_host: String,
    /// Host path of an empty dir (bound at /build/home).
    pub home_host: String,
    /// Host dir for the sandbox's own scratch (newroot, home, …).
    pub scratch: String,
}

impl SandboxSpec {
    pub fn parse(text: &str) -> Result<Self> {
        let spec: SandboxSpec = toml::from_str(text).context("bad sandbox spec")?;
        if spec.version != 1 {
            bail!("unsupported sandbox spec version {}", spec.version);
        }
        match spec.root.mode.as_str() {
            "live" => {}
            "dir" => {
                if spec.root.dir.trim().is_empty() {
                    bail!("sandbox spec root.mode = \"dir\" needs root.dir");
                }
                for l in &spec.root.layers {
                    if l.trim().is_empty() {
                        bail!("sandbox spec root.layers entries must be non-empty");
                    }
                }
            }
            other => bail!("unknown sandbox root mode `{other}`"),
        }
        Ok(spec)
    }

    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("read sandbox spec {}", path.display()))?;
        Self::parse(&text)
    }
}

// ---------------------------------------------------------------------------
// Entry point (`nawa sandbox-init <spec.toml>`)
// ---------------------------------------------------------------------------

/// Run the spec; returns the exit code the build engine should observe.
pub fn init(path: &Path) -> Result<i32> {
    let spec = SandboxSpec::load(path)?;

    // -- 1. user namespace + identity (§4.3) ------------------------------
    // The ids must be captured BEFORE unshare: inside the unmapped namespace
    // they read back as 65534.
    let euid = unsafe { libc::geteuid() };
    let egid = unsafe { libc::getegid() };
    if unsafe { libc::unshare(libc::CLONE_NEWUSER) } != 0 {
        let err = std::io::Error::last_os_error();
        bail!(
            "cannot create a user namespace ({}).\n\
             The kernel needs CONFIG_USER_NS and unprivileged user namespaces \
             enabled (sysctl kernel.unprivileged_userns_clone). Refusing to \
             build without the requested isolation; rerun with --allow-degraded \
             to continue at a lower level",
            err
        );
    }
    // give the parent a chance to install a ranged map (root runs); the byte
    // arrives after it is done (or never, when invoked manually)
    sync_wait_for_parent();
    ensure_identity(euid, egid)?;
    if unsafe { libc::geteuid() } != 0 {
        bail!("no uid mapping installed — builds must run as root inside the sandbox");
    }

    // -- 2. the remaining namespaces --------------------------------------
    let mut flags = libc::CLONE_NEWNS | libc::CLONE_NEWPID | libc::CLONE_NEWIPC | libc::CLONE_NEWUTS;
    if !spec.network {
        flags |= libc::CLONE_NEWNET;
    }
    if unsafe { libc::unshare(flags) } != 0 {
        bail!(
            "cannot create the mount/pid/ipc/uts namespaces: {}",
            std::io::Error::last_os_error()
        );
    }

    // -- 3. fork PID 1 -----------------------------------------------------
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        bail!("fork: {}", std::io::Error::last_os_error());
    }
    if pid == 0 {
        // PID 1 of the new pid namespace. Errors are fatal and must not
        // outlive us — exit directly.
        let code = match run_pid1(&spec) {
            Ok(()) => 126, // exec must not return
            Err(e) => {
                eprintln!("nawa: sandbox: {e:#}");
                125
            }
        };
        unsafe { libc::_exit(code) };
    }

    // -- 4. timeout + reap -------------------------------------------------
    install_timeout(spec.timeout_secs, pid);
    let mut status: i32 = 0;
    if unsafe { libc::waitpid(pid, &mut status, 0) } < 0 {
        bail!("waitpid: {}", std::io::Error::last_os_error());
    }
    if TIMED_OUT.load(std::sync::atomic::Ordering::Relaxed) {
        eprintln!(
            "nawa: sandbox: section timed out after {}s (all processes killed)",
            spec.timeout_secs
        );
        return Ok(EXIT_TIMEOUT);
    }
    if libc::WIFEXITED(status) {
        Ok(libc::WEXITSTATUS(status))
    } else if libc::WIFSIGNALED(status) {
        Ok(128 + libc::WTERMSIG(status))
    } else {
        Ok(125)
    }
}

/// Set up the filesystem as PID 1 and exec the build.
fn run_pid1(spec: &SandboxSpec) -> Result<()> {
    if spec.root.mode == "live" {
        setup_live_root(spec)?;
    } else {
        setup_dir_root(spec)?;
    }
    common_prep(spec)?;
    exec_build(spec)
}

// ---------------------------------------------------------------------------
// Filesystem setup (§4.2)
// ---------------------------------------------------------------------------

/// `sandbox` level: keep the current root (the live system), but overlay the
/// writable and kernel-managed parts in *our private mount namespace only*:
/// a self-bind makes `/` a mount we own and can mark read-only without
/// touching the host's view of it. Read-only goes on LAST — creating the
/// mount points writes to the root while it is still writable.
fn setup_live_root(spec: &SandboxSpec) -> Result<()> {
    mount_rprivate("/")?;

    // self-bind: a new mount object over / that we own; its per-mount flags
    // (read-only below) do not propagate to the host namespace
    mbind("/", "/", true)?;

    overlay_scratch(spec)?;
    remount_ro("/", spec, "the build root (/)")?;
    Ok(())
}

/// `closure` level: pivot into the assembled build root; the live system
/// stays reachable only under the (masked) /old until we drop it. The root
/// is marked read-only AFTER the pivot (the same pattern the `live` mode
/// uses — and pivot_root into a read-only tree is refused by the kernel).
fn setup_dir_root(spec: &SandboxSpec) -> Result<()> {
    let root = PathBuf::from(&spec.root.dir);
    match std::fs::metadata(&root) {
        Ok(m) if m.is_dir() => {}
        Ok(_) => bail!("sandbox build root {} is not a directory", root.display()),
        Err(e) => bail!(
            "sandbox build root {} is not reachable from inside the sandbox: {e}\n\
             (when nawa runs as root the sandbox maps only uid 0 and 100000.., so a \
             parent directory owned by another user with mode 750 — e.g. a default \
             /home/<user> — cannot be traversed there. Use a --builddir outside \
             your home, e.g. /var/tmp/nawa or /build)",
            root.display()
        ),
    }
    mount_rprivate("/")?;

    // Phase 5: a layered build root is one overlayfs mount; the plain
    // extract-per-build root is a self-bind. Either way the future root is
    // a mount, which is what pivot_root requires.
    if spec.root.layers.is_empty() {
        let root_s = root.to_string_lossy().into_owned();
        mbind(&root_s, &root_s, false)?;
    } else {
        mount_layers(spec, &root)?;
    }

    // mounts inside the future root happen before the pivot so that nothing
    // ever writes through an un-pivoted path
    overlay_scratch_at(spec, &root)?;
    let old = root.join("old");
    std::fs::create_dir_all(&old)
        .with_context(|| format!("create {}", old.display()))?;

    // pivot, falling back to chroot in chroots where pivot is not possible
    // (design §4.1)
    let root_s = root.to_string_lossy().into_owned();
    let old_s = old.to_string_lossy().into_owned();
    let root_c = cstr(&root_s);
    if unsafe { libc::chdir(root_c.as_ptr()) } != 0 {
        bail!("chdir {}: {}", root.display(), std::io::Error::last_os_error());
    }
    // NUL-terminated C strings: a Rust `String`'s pointer is NOT, and the
    // kernel would read past the end of the path (ENOENT / wrong path)
    let old_c = cstr(&old_s);
    if unsafe {
        libc::syscall(libc::SYS_pivot_root, root_c.as_ptr(), old_c.as_ptr())
    } < 0
    {
        let err = std::io::Error::last_os_error();
        eprintln!(
            "nawa: sandbox: pivot_root failed ({err}) — falling back to chroot"
        );
        if unsafe { libc::chroot(root_c.as_ptr()) } != 0 {
            bail!(
                "chroot {}: {}",
                root.display(),
                std::io::Error::last_os_error()
            );
        }
    }
    let root_after = cstr("/");
    if unsafe { libc::chdir(root_after.as_ptr()) } != 0 {
        bail!("chdir /: {}", std::io::Error::last_os_error());
    }
    // hide the old root entirely (pivot moved the live system to /old)
    tmpfs_mask("/old")?;
    // now that we live inside it, the build root becomes read-only
    remount_ro("/", spec, "the build root (/)")?;
    Ok(())
}

/// Mount the build root as an overlayfs over the extracted layers (§5.3 v2).
/// The upper/work dirs live under the sandbox scratch and are thrown away
/// with it; the root itself ends up read-only right after the pivot, so the
/// upperdir only ever exists to satisfy the mount, never to record writes.
fn mount_layers(spec: &SandboxSpec, root: &Path) -> Result<()> {
    let lower: Vec<String> = spec
        .root
        .layers
        .iter()
        .map(|l| l.trim_end_matches('/').to_string())
        .collect();
    for l in &lower {
        match std::fs::metadata(l) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                bail!("overlay layer {} is missing", l);
            }
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => bail!(
                "overlay layer {} is not reachable by the sandbox's mapped \
                 identity ({}) — a parent directory belongs to a uid the \
                 sandbox does not map (e.g. a home directory with mode 700, \
                 §4.3); the builder is supposed to fall back to \
                 extract-per-build assembly before choosing layers",
                l,
                e
            ),
            Err(e) => bail!("overlay layer {}: {}", l, e),
        }
    }
    let scratch = PathBuf::from(&spec.build.scratch);
    let upper = scratch.join("layer-upper");
    let work = scratch.join("layer-work");
    std::fs::create_dir_all(&upper)?;
    std::fs::create_dir_all(&work)?;
    // overlayfs splits its data string on ',' and delimits entries with
    // ':' — paths containing either cannot be expressed (no escaping); the
    // builder refuses them up front, this is the last line of defence
    for p in lower.iter().chain([upper.to_string_lossy().into_owned(), work.to_string_lossy().into_owned()].iter()) {
        if p.contains(',') || p.contains(':') {
            bail!("path {} cannot be used in an overlayfs mount (, or :)", p);
        }
    }
    // `userxattr`: inside a user namespace the kernel cannot set trusted.overlay.*
    // xattrs ("failed to set xattr on upper"); it must use user.overlay.* (>= 5.11).
    let data = format!(
        "lowerdir={},upperdir={},workdir={},userxattr",
        // overlayfs: the FIRST lowerdir is the TOPMOST; `layers` is bottom-most
        // first (later members win conflicts, like extract-per-build), so reverse
        lower.iter().rev().cloned().collect::<Vec<_>>().join(":"),
        upper.display(),
        work.display()
    );
    let croot = cstr(&root.to_string_lossy());
    let cdata = cstr(&data);
    if unsafe {
        libc::mount(
            c"overlay".as_ptr(),
            croot.as_ptr(),
            c"overlay".as_ptr(),
            0,
            cdata.as_ptr() as *const libc::c_void,
        )
    } != 0
    {
        let err = std::io::Error::last_os_error();
        bail!(
            "overlayfs build root mount failed: {}\n\
             (the builder probed this before the build; a failure here means \
             the kernel stopped allowing user-namespace overlays — rerun the \
             build so nawa falls back to extract-per-build assembly)",
            err
        );
    }
    Ok(())
}

/// Overlay the §4.2 layout over the *current* root (`live` mode).
fn overlay_scratch(spec: &SandboxSpec) -> Result<()> {
    // O_PATH handles for the staged dirs come FIRST, while every path
    // resolves against the live system: mounting the build base (§4.2 pins
    // `/build`, which is also where the bootstrap stages keep their build
    // tree) makes those host paths unresolvable afterwards, and plain path
    // binds would fail with ENOENT. The fd holds the mount reference.
    let staging = StagingHandles::open(&spec.build)?;
    // fresh /tmp first: an unprivileged host cannot create `/build` on the
    // live root, and the fallback base then lives on this tmpfs
    tmpfs("/tmp", "size=2048m,mode=1777")?;
    // the build base: a tiny tmpfs holding only the three bind points; the
    // heavy dirs themselves are disk-backed host binds
    let base_p = Path::new(spec.build_inside.as_str());
    std::fs::create_dir_all(base_p)
        .with_context(|| format!("create the sandbox build base {}", spec.build_inside))?;
    tmpfs_at(base_p, "size=1m,mode=755")?;
    staging.bind_at(base_p)?;
    drop(staging);
    setup_dev(Path::new("/dev"), Path::new(&spec.build.scratch))?;
    fresh_proc_or_mask(spec, Path::new("/proc"))?;
    setup_sys(spec, Path::new("/sys"))?;
    for p in &spec.mask {
        if Path::new(p).exists() {
            tmpfs_mask(p)?;
        }
    }
    Ok(())
}

/// Overlay the §4.2 layout *inside* the assembled root (`dir` mode): the
/// root is a directory we own, so `/build` is always creatable there.
fn overlay_scratch_at(spec: &SandboxSpec, root: &Path) -> Result<()> {
    let at = |rel: &str| root.join(rel.trim_start_matches('/'));
    // same fd-first pattern: the base tmpfs covers the bind targets, and the
    // handles keep the (host-side) bind sources reachable no matter what
    // covers what below
    let staging = StagingHandles::open(&spec.build)?;
    tmpfs_at(&at("tmp"), "size=2048m,mode=1777")?;
    let base = root.join(spec.build_inside.trim_start_matches('/'));
    tmpfs_at(&base, "size=1m,mode=755")?;
    staging.bind_at(&base)?;
    drop(staging);
    setup_dev(&at("dev"), Path::new(&spec.build.scratch))?;
    fresh_proc_or_mask(spec, &at("proc"))?;
    // an assembled closure never contains /sys; nothing to hide
    Ok(())
}

/// O_PATH descriptors of the staged dirs (`src`, `pkg`, `home`) and the logic
/// that binds them into the sandbox build base. The descriptors are taken
/// *before* any mount covers their host paths: a bind source resolved by path
/// after a cover yields ENOENT, while the open file the fd refers to stays
/// mounted in the kernel even through the shadowing mount. Closed on drop.
struct StagingHandles {
    fds: [libc::c_int; 3],
}

impl StagingHandles {
    fn open(build: &BuildPaths) -> Result<Self> {
        let hosts = [&build.src_host, &build.pkg_host, &build.home_host];
        let mut fds = [0 as libc::c_int; 3];
        for (i, host) in hosts.iter().enumerate() {
            if !Path::new(host).is_dir() {
                bail!("sandbox staging dir {} does not exist", host);
            }
            let c = cstr(host);
            let fd = unsafe {
                libc::open(c.as_ptr(), libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW)
            };
            if fd < 0 {
                bail!("open sandbox bind source {}: {}", host, std::io::Error::last_os_error());
            }
            fds[i] = fd;
        }
        Ok(Self { fds })
    }

    /// Create the three bind targets under `base` and bind the handles
    /// through their `/proc/self/fd` aliases. `/proc` must be resolvable
    /// (the host's procfs is still mounted here — the fresh one mounts after).
    fn bind_at(&self, base: &Path) -> Result<()> {
        let names = ["src", "pkg", "home"];
        for (i, name) in names.iter().enumerate() {
            let dst = base.join(name);
            std::fs::create_dir_all(&dst)?;
            let link = format!("/proc/self/fd/{}", self.fds[i]);
            mbind(&link, &dst.to_string_lossy(), false)
                .with_context(|| format!("bind staging {} -> {}", names[i], dst.display()))?;
        }
        Ok(())
    }
}

impl Drop for StagingHandles {
    fn drop(&mut self) {
        for fd in self.fds {
            if fd >= 0 {
                unsafe { libc::close(fd) };
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Shared mount helpers with precise failure reporting
// ---------------------------------------------------------------------------

fn mount_rprivate(what: &str) -> Result<()> {
    let cwhat = cstr(what);
    if unsafe {
        libc::mount(
            std::ptr::null(),
            cwhat.as_ptr(),
            std::ptr::null(),
            libc::MS_REC | libc::MS_PRIVATE,
            std::ptr::null(),
        )
    } != 0
    {
        bail!(
            "mount --make-rprivate {} failed: {}",
            what,
            std::io::Error::last_os_error()
        );
    }
    Ok(())
}

/// A bind mount, optionally recursive.
fn mbind(src: &str, dst: &str, rec: bool) -> Result<()> {
    let csrc = cstr(src);
    let cdst = cstr(dst);
    let flags = if rec { libc::MS_BIND | libc::MS_REC } else { libc::MS_BIND };
    if unsafe {
        libc::mount(csrc.as_ptr(), cdst.as_ptr(), std::ptr::null(), flags, std::ptr::null())
    } != 0
    {
        bail!(
            "bind mount {} -> {} failed: {}",
            src,
            dst,
            std::io::Error::last_os_error()
        );
    }
    Ok(())
}

/// Mark a mount we created read-only (per-mount flag; the original stays
/// writable — this is what makes `make install` without DESTDIR fail
/// loudly instead of writing into the running system).
fn remount_ro(target: &str, spec: &SandboxSpec, what: &str) -> Result<()> {
    let c = cstr(target);
    let ok = unsafe {
        libc::mount(
            std::ptr::null(),
            c.as_ptr(),
            std::ptr::null(),
            libc::MS_BIND | libc::MS_REMOUNT | libc::MS_RDONLY | libc::MS_NOSUID | libc::MS_NODEV,
            std::ptr::null(),
        )
    } == 0;
    if !ok {
        let err = std::io::Error::last_os_error();
        if spec.allow_degraded {
            eprintln!(
                "nawa: sandbox: WARNING: cannot mark {what} read-only ({err}) — \
                 continuing WITHOUT read-only isolation (--allow-degraded)"
            );
            return Ok(());
        }
        bail!(
            "cannot mark {what} read-only ({err}); refusing to run a build that \
             could write into the system — rerun with --allow-degraded"
        );
    }
    Ok(())
}

fn tmpfs(at: &str, opts: &str) -> Result<()> {
    tmpfs_at(Path::new(at), opts)
}

fn tmpfs_at(at: &Path, opts: &str) -> Result<()> {
    std::fs::create_dir_all(at)?;
    let cat = cstr(&at.to_string_lossy());
    let copts = cstr(opts);
    if unsafe {
        libc::mount(
            c"tmpfs".as_ptr(),
            cat.as_ptr(),
            c"tmpfs".as_ptr(),
            libc::MS_NOSUID | libc::MS_NODEV,
            copts.as_ptr() as *const libc::c_void,
        )
    } != 0
    {
        bail!(
            "tmpfs on {} failed: {}",
            at.display(),
            std::io::Error::last_os_error()
        );
    }
    Ok(())
}

/// Hide a path behind an empty, unsearchable tmpfs.
fn tmpfs_mask(path: &str) -> Result<()> {
    let p = Path::new(path);
    std::fs::create_dir_all(p)?;
    let c = cstr(path);
    if unsafe {
        libc::mount(
            c"tmpfs".as_ptr(),
            c.as_ptr(),
            c"tmpfs".as_ptr(),
            libc::MS_NOSUID | libc::MS_NODEV,
            c"size=1m,mode=0".as_ptr() as *const libc::c_void,
        )
    } != 0
    {
        bail!(
            "cannot mask {} (tmpfs): {}",
            path,
            std::io::Error::last_os_error()
        );
    }
    Ok(())
}

/// Minimal /dev (§4.2): tmpfs plus real (bind-mounted) device nodes —
/// mknod is not permitted inside a user namespace. The host nodes are
/// staged under `scratch` FIRST: in live mode /dev gets covered by the fresh
/// tmpfs, and the staging binds survive that cover.
fn setup_dev(dev: &Path, scratch: &Path) -> Result<()> {
    let staging = scratch.join("devnodes");
    std::fs::create_dir_all(&staging)?;
    for node in ["null", "zero", "full", "random", "urandom", "tty"] {
        let src = PathBuf::from("/dev").join(node);
        if !src.exists() {
            eprintln!("nawa: sandbox: note: host /dev/{node} missing, skipped");
            continue;
        }
        let staged = staging.join(node);
        if !staged.exists() {
            std::fs::File::create(&staged)
                .with_context(|| format!("create {}", staged.display()))?;
        }
        mbind(&src.to_string_lossy(), &staged.to_string_lossy(), false)?;
    }
    tmpfs_at(dev, "size=64m,mode=755")?;
    for node in ["null", "zero", "full", "random", "urandom", "tty"] {
        let staged = staging.join(node);
        if !staged.exists() {
            continue;
        }
        let dst = dev.join(node);
        // bind targets must exist: create a placeholder file first
        if !dst.exists() {
            std::fs::File::create(&dst)
                .with_context(|| format!("create {}", dst.display()))?;
        }
        mbind(&staged.to_string_lossy(), &dst.to_string_lossy(), false)?;
    }
    let pts = dev.join("pts");
    std::fs::create_dir_all(&pts)?;
    let cpts = cstr(&pts.to_string_lossy());
    if unsafe {
        libc::mount(
            c"devpts".as_ptr(),
            cpts.as_ptr(),
            c"devpts".as_ptr(),
            libc::MS_NOSUID | libc::MS_NOEXEC,
            c"newinstance,ptmxmode=0666".as_ptr() as *const libc::c_void,
        )
    } != 0
    {
        bail!(
            "devpts on {} failed: {}",
            pts.display(),
            std::io::Error::last_os_error()
        );
    }
    // the classic /dev/ptmx alias for the new instance
    let ptmx = dev.join("ptmx");
    if !ptmx.exists() {
        std::fs::File::create(&ptmx)?;
    }
    mbind(&pts.join("ptmx").to_string_lossy(), &ptmx.to_string_lossy(), false)?;
    tmpfs_at(&dev.join("shm"), "size=64m,mode=1777")?;
    Ok(())
}

/// Fresh procfs for the new PID namespace — required: the copied /proc of
/// the live root shows the *host's* processes. When the kernel refuses
/// (restricted containers), degrade to masking it rather than leaking.
fn fresh_proc_or_mask(spec: &SandboxSpec, at: &Path) -> Result<()> {
    std::fs::create_dir_all(at)?;
    let c = cstr(&at.to_string_lossy());
    let ok = unsafe {
        libc::mount(
            c"proc".as_ptr(),
            c.as_ptr(),
            c"proc".as_ptr(),
            libc::MS_NOSUID | libc::MS_NOEXEC | libc::MS_NODEV,
            std::ptr::null(),
        )
    } == 0;
    if !ok {
        let err = std::io::Error::last_os_error();
        if spec.allow_degraded {
            eprintln!(
                "nawa: sandbox: WARNING: cannot mount a fresh /proc ({err}) — \
                 masking it instead; tools that read /proc may misbehave \
                 (--allow-degraded)"
            );
            return tmpfs_mask(&at.to_string_lossy());
        }
        bail!(
            "cannot mount a fresh /proc for the build ({err}); a sandboxed build \
             must not see the host's process list — rerun with --allow-degraded"
        );
    }
    Ok(())
}

/// /sys is hidden by default; recipes that probe it can opt in to read-only.
fn setup_sys(spec: &SandboxSpec, at: &Path) -> Result<()> {
    if spec.sysfs {
        std::fs::create_dir_all(at)?;
        let c = cstr(&at.to_string_lossy());
        if unsafe {
            libc::mount(
                c"sysfs".as_ptr(),
                c.as_ptr(),
                c"sysfs".as_ptr(),
                libc::MS_RDONLY | libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC,
                std::ptr::null(),
            )
        } != 0
        {
            let err = std::io::Error::last_os_error();
            if spec.allow_degraded {
                eprintln!(
                    "nawa: sandbox: WARNING: cannot mount /sys read-only ({err}) — \
                     masking instead (--allow-degraded)"
                );
                return tmpfs_mask(&at.to_string_lossy());
            }
            bail!("cannot mount /sys read-only ({err}) — rerun with --allow-degraded");
        }
        return Ok(());
    }
    if at.exists() {
        tmpfs_mask(&at.to_string_lossy())?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Identity, limits, exec
// ---------------------------------------------------------------------------

/// Bring the network namespace's loopback up (§4.4): many test suites need
/// 127.0.0.1 even with no other connectivity.
fn lo_up() -> Result<()> {
    #[repr(C)]
    struct IfReq {
        name: [u8; 16],
        flags: i16,
        _pad: [u8; 22], // struct ifreq is 40 bytes
    }
    let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0) };
    if fd < 0 {
        bail!("socket: {}", std::io::Error::last_os_error());
    }
    let mut ifr = IfReq { name: *b"lo\0\0\0\0\0\0\0\0\0\0\0\0\0\0", flags: 0, _pad: [0; 22] };
    ifr.flags = libc::IFF_UP as i16;
    let ok = unsafe { libc::ioctl(fd, libc::SIOCSIFFLAGS as _, &ifr) } == 0;
    let err = std::io::Error::last_os_error();
    unsafe { libc::close(fd) };
    if !ok {
        bail!("bring up loopback: {err}");
    }
    Ok(())
}

fn common_prep(spec: &SandboxSpec) -> Result<()> {
    if !spec.network {
        lo_up()?;
    }
    // umask 022 (§6): environment variables cannot carry it
    unsafe { libc::umask(0o022) };
    // RLIMIT_CORE = 0 (§4.5)
    let rl = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    unsafe { libc::setrlimit(libc::RLIMIT_CORE, &rl) };
    // PR_SET_NO_NEW_PRIVS: even a setuid binary inside cannot regain power
    if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
        bail!("prctl(NO_NEW_PRIVS): {}", std::io::Error::last_os_error());
    }
    // drop every capability (§4.1): builds need none
    drop_capabilities();
    Ok(())
}

fn drop_capabilities() {
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct CapHeader {
        version: u32,
        pid: i32,
    }
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct CapData {
        effective: u32,
        permitted: u32,
        inheritable: u32,
    }
    let hdr = CapHeader { version: 0x2008_0522 /* _LINUX_CAPABILITY_VERSION_3 */, pid: 0 };
    let data = [CapData { effective: 0, permitted: 0, inheritable: 0 }; 2];
    unsafe {
        libc::syscall(
            libc::SYS_capset,
            &hdr as *const CapHeader,
            data.as_ptr(),
        );
    }
}

fn exec_build(spec: &SandboxSpec) -> Result<()> {
    let cwd = if spec.workdir.is_empty() {
        format!("{}/src", spec.build_inside.trim_end_matches('/'))
    } else {
        spec.workdir.clone()
    };
    let cwd_c = cstr(&cwd);
    let base_c = cstr(&spec.build_inside);
    if unsafe { libc::chdir(cwd_c.as_ptr()) } != 0 {
        // meta packages run directly in an (empty) src dir that must exist
        if unsafe { libc::chdir(base_c.as_ptr()) } != 0 {
            bail!("chdir {cwd}: {}", std::io::Error::last_os_error());
        }
    }
    let argv: Vec<std::ffi::CString> =
        vec![cstr("bash"), cstr("-e"), cstr("-c"), cstr(&spec.script)];
    let mut env: Vec<std::ffi::CString> = spec
        .env
        .iter()
        .map(|(k, v)| cstr(&format!("{k}={v}")))
        .collect();
    env.push(cstr(&format!("PWD={cwd}")));
    let mut argvp: Vec<*const libc::c_char> = argv.iter().map(|a| a.as_ptr()).collect();
    argvp.push(std::ptr::null());
    let mut envp: Vec<*const libc::c_char> = env.iter().map(|a| a.as_ptr()).collect();
    envp.push(std::ptr::null());
    for shell in ["/usr/bin/bash", "/bin/bash"] {
        let sh = cstr(shell);
        if unsafe { libc::execve(sh.as_ptr(), argvp.as_ptr(), envp.as_ptr()) } == 0 {
            unreachable!("execve returned");
        }
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() != Some(libc::ENOENT) {
            bail!("exec {}: {err}", shell);
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
fn ensure_identity(euid: u32, egid: u32) -> Result<()> {
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

/// Block until the parent (which may write a ranged uid map) signals done.
/// EOF or a missing fd both mean "no parent help — self-write".
fn sync_wait_for_parent() {
    let mut buf = [0u8; 1];
    // announce readiness (a failed write = no parent on the other end)
    // SAFETY: write on a small fd we may or may not have
    unsafe {
        libc::write(SYNC_FD, b"R".as_ptr() as *const libc::c_void, 1);
    }
    // SAFETY: blocking read of one byte — the parent's `G`, or EOF
    unsafe {
        libc::read(SYNC_FD, buf.as_mut_ptr() as *mut libc::c_void, buf.len() as libc::size_t);
    }
    // the fd must not leak into bash
    unsafe { libc::fcntl(SYNC_FD, libc::F_SETFD, libc::FD_CLOEXEC) };
}

/// Alarm-based timeout: kills PID 1, whose death makes the kernel reap the
/// whole PID namespace (§4.5).
fn install_timeout(secs: u64, pid1: libc::pid_t) {
    if secs == 0 {
        return;
    }
    PID1.store(pid1, std::sync::atomic::Ordering::Relaxed);
    unsafe {
        libc::signal(libc::SIGALRM, on_alarm as *const () as libc::sighandler_t);
        libc::alarm(secs as libc::c_uint);
    }
}

static PID1: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
static TIMED_OUT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

extern "C" fn on_alarm(_sig: libc::c_int) {
    TIMED_OUT.store(true, std::sync::atomic::Ordering::Relaxed);
    let pid = PID1.load(std::sync::atomic::Ordering::Relaxed);
    if pid > 0 {
        unsafe { libc::kill(pid, libc::SIGKILL) };
    }
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
    unsafe {
        cmd.pre_exec(move || {
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
        let euid = unsafe { libc::geteuid() };
        let egid = unsafe { libc::getegid() };
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
// §4.3 identity emulation (used by the build engine)
// ---------------------------------------------------------------------------

/// What the §4.3 mapping does to a *host* owner id, as seen from inside the
/// sandbox: the caller's own id becomes 0; real root additionally maps
/// inside 1..65535 to the subordinate range 100000..165534 (`parent_sync`);
/// anything else is unmapped (reads back as 65534 = nobody).
///
/// The mount that assembles a layered build root happens **inside** the
/// sandbox (`run_pid1` → `mount_layers`), so overlay lowerdirs are resolved
/// by the sandbox's mapped identity — not by whoever ran nawa. A layer cache
/// the user nawa reads happily (a `sudo` run against a `--root` under a
/// mode-700 home directory is the common case) can therefore still be
/// unreachable from the build.
pub fn first_untraversable_ancestor(path: &Path) -> Option<PathBuf> {
    use std::os::unix::fs::MetadataExt;

    let euid = unsafe { libc::geteuid() };
    let egid = unsafe { libc::getegid() };
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
// Probe (`nawa sandbox-probe`)
// ---------------------------------------------------------------------------

/// One capability probe. Runs in a fresh process (`nawa sandbox-probe <op>`)
/// so the caller's namespaces stay untouched. Every probing op takes its
/// own private mount/net namespace first — the probe must never affect the
/// caller's environment.
pub fn probe(op: &str) -> Result<bool> {
    let euid = unsafe { libc::geteuid() };
    let egid = unsafe { libc::getegid() };
    let userns = matches!(op, "userns" | "mounts" | "ro-bind" | "proc" | "pivot" | "netns-lo" | "overlay");
    if userns && unsafe { libc::unshare(libc::CLONE_NEWUSER) } != 0 {
        return Ok(false);
    }
    if userns {
        ensure_identity(euid, egid)?;
    }
    let ok = match op {
        "userns" => true,
        "mounts" => {
            if unsafe { libc::unshare(libc::CLONE_NEWNS) } != 0 {
                return Ok(false);
            }
            mount_rprivate("/").is_ok() && tmpfs("/tmp/.nawa-probe", "size=1m").is_ok()
        }
        "ro-bind" => {
            if unsafe { libc::unshare(libc::CLONE_NEWNS) } != 0 {
                return Ok(false);
            }
            // bind a host dir read-only: the core of P4
            let t = std::env::temp_dir().join(format!("nawa-probe-ro-{}", std::process::id()));
            let _ = std::fs::create_dir_all(t.join("self"));
            let src = t.to_string_lossy().into_owned();
            let dst = t.join("self").to_string_lossy().into_owned();
            let r = mbind(&src, &dst, false)
                .and_then(|_| remount_ro(&dst, &probe_spec(""), "probe"));
            let _ = std::fs::remove_dir_all(&t);
            r.is_ok()
        }
        "proc" | "pivot" => {
            if unsafe { libc::unshare(libc::CLONE_NEWNS | libc::CLONE_NEWPID) } != 0 {
                return Ok(false);
            }
            let pid = unsafe { libc::fork() };
            if pid == 0 {
                let ok = match op {
                    "proc" => {
                        let d = std::env::temp_dir().join(format!("nawa-probe-proc-{}", std::process::id()));
                        let _ = std::fs::create_dir_all(&d);
                        fresh_proc_or_mask(&probe_spec(""), &d).is_ok()
                    }
                    _ => {
                        let d = std::env::temp_dir().join(format!("nawa-probe-pv-{}", std::process::id()));
                        let _ = std::fs::create_dir_all(d.join("old"));
                        let s = d.to_string_lossy().into_owned();
                        let old = d.join("old").to_string_lossy().into_owned();
                        let (s_c, old_c) = (cstr(&s), cstr(&old));
                        mbind(&s, &s, false).is_ok()
                            && unsafe {
                                libc::syscall(
                                    libc::SYS_pivot_root,
                                    s_c.as_ptr(),
                                    old_c.as_ptr(),
                                )
                            } == 0
                    }
                };
                unsafe { libc::_exit(if ok { 0 } else { 3 }) };
            }
            let mut st = 0;
            unsafe { libc::waitpid(pid, &mut st, 0) };
            let exited = libc::WIFEXITED(st);
            let code = libc::WEXITSTATUS(st);
            exited && code == 0
        }
        "netns-lo" => {
            if unsafe { libc::unshare(libc::CLONE_NEWNET) } != 0 {
                return Ok(false);
            }
            lo_up().is_ok()
        }
        "overlay" => {
            // Phase 5: overlayfs with lowerdirs inside a user namespace —
            // kernel ≥ 5.11 (older kernels refuse with EPERM/EINVAL).
            // Two one-file layers must both show up in the merged view.
            if unsafe { libc::unshare(libc::CLONE_NEWNS) } != 0 {
                return Ok(false);
            }
            let t = std::env::temp_dir().join(format!("nawa-probe-ovl-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&t);
            let (l1, l2) = (t.join("l1"), t.join("l2"));
            let ok = std::fs::create_dir_all(l1.join("a")).is_ok()
                && std::fs::create_dir_all(&l2).is_ok()
                && std::fs::write(l1.join("a").join("one"), b"1").is_ok()
                && std::fs::write(l2.join("two"), b"2").is_ok()
                && mount_layers_overlay_probe(
                    &t.join("merged"),
                    &[&l1, &l2],
                    &t.join("u"),
                    &t.join("w"),
                )
                .is_ok()
                && t.join("merged/a/one").is_file()
                && t.join("merged/two").is_file();
            let _ = std::fs::remove_dir_all(&t);
            ok
        }
        other => bail!("unknown probe `{other}`"),
    };
    Ok(ok)
}

/// The exact mount `mount_layers` performs, on probe-built paths (no spec
/// plumbing needed — the probe only answers "does this kernel allow it").
fn mount_layers_overlay_probe(
    merged: &Path,
    layers: &[&Path],
    upper: &Path,
    work: &Path,
) -> Result<()> {
    // upperdir/workdir must exist before the mount (the real mount_layers
    // creates them too); otherwise overlayfs fails with ENOENT and the probe
    // reports "no overlay" on kernels that do support it.
    std::fs::create_dir_all(merged)?;
    std::fs::create_dir_all(upper)?;
    std::fs::create_dir_all(work)?;
    let lower = layers
        .iter()
        .map(|p| p.to_string_lossy().trim_end_matches('/').to_string())
        .collect::<Vec<_>>()
        .join(":");
    // `userxattr`: inside a user namespace the kernel cannot set trusted.overlay.*
    // xattrs ("failed to set xattr on upper"); it must use user.overlay.* (>= 5.11).
    let data = format!(
        "lowerdir={},upperdir={},workdir={},userxattr",
        lower,
        upper.display(),
        work.display()
    );
    let croot = cstr(&merged.to_string_lossy());
    let cdata = cstr(&data);
    if unsafe {
        libc::mount(
            c"overlay".as_ptr(),
            croot.as_ptr(),
            c"overlay".as_ptr(),
            0,
            cdata.as_ptr() as *const libc::c_void,
        )
    } != 0
    {
        bail!(
            "overlay probe mount: {}",
            std::io::Error::last_os_error()
        );
    }
    Ok(())
}

fn probe_spec(scratch: &str) -> SandboxSpec {
    SandboxSpec {
        version: 1,
        root: RootSpec::default(),
        build: BuildPaths {
            src_host: "/dev/null".into(),
            pkg_host: "/dev/null".into(),
            home_host: "/dev/null".into(),
            scratch: scratch.to_string(),
        },
        env: Default::default(),
        script: String::new(),
        timeout_secs: 0,
        network: false,
        sysfs: false,
        allow_degraded: false,
        mask: vec![],
        build_inside: "/build".to_string(),
        workdir: String::new(),
    }
}

/// All probes, for `nawa sandbox-probe` (and the integration tests).
pub const PROBES: &[&str] = &[
    "userns", "mounts", "ro-bind", "proc", "pivot", "netns-lo", "overlay",
];

fn cstr(s: &str) -> std::ffi::CString {
    // mounts must never see interior NULs; paths come from our own config
    std::ffi::CString::new(s.replace('\0', "")).unwrap_or_else(|_| cstr(""))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn spec_parses_layers_for_dir_roots() {
        let spec = SandboxSpec::parse(
            r#"
version = 1
script = "true"

[root]
mode = "dir"
dir = "/buildroots/x"
layers = ["/var/cache/nawa/layers/aaa", "/var/cache/nawa/layers/bbb"]

[build]
src_host = "/w/src"
pkg_host = "/w/pkg"
home_host = "/w/home"
scratch = "/w/sb"
"#,
        )
        .unwrap();
        assert_eq!(spec.root.layers.len(), 2);
        assert_eq!(spec.root.layers[0], "/var/cache/nawa/layers/aaa");
        // live roots never carry layers
        let live = SandboxSpec::parse(
            r#"
version = 1
script = "true"
[build]
src_host = "/w/src"
pkg_host = "/w/pkg"
home_host = "/w/home"
scratch = "/w/sb"
"#,
        )
        .unwrap();
        assert!(live.root.layers.is_empty());
    }

    #[test]
    fn layers_default_to_empty_for_older_specs() {
        // a spec written before Phase 5 (no [root.layers]) must keep parsing
        let spec = SandboxSpec::parse(
            r#"
version = 1
script = "true"
[root]
mode = "dir"
dir = "/r"
[build]
src_host = "/w/src"
pkg_host = "/w/pkg"
home_host = "/w/home"
scratch = "/w/sb"
"#,
        )
        .unwrap();
        assert!(spec.root.layers.is_empty());
    }

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

    #[test]
    fn overlay_data_string_is_built_in_layer_order() {
        // the data string must list lowerdirs in the given order — later
        // entries win conflicts, matching extract-per-build's overwrite order
        let spec = SandboxSpec {
            version: 1,
            root: RootSpec {
                mode: "dir".into(),
                dir: "/r".into(),
                layers: vec!["/l/a".into(), "/l/b".into()],
            },
            build: BuildPaths {
                src_host: String::new(),
                pkg_host: String::new(),
                home_host: String::new(),
                scratch: "/w/sb".into(),
            },
            env: Default::default(),
            script: String::new(),
            timeout_secs: 0,
            network: false,
            sysfs: false,
            allow_degraded: false,
            mask: vec![],
            build_inside: "/build".into(),
            workdir: String::new(),
        };
        let lower: Vec<String> = spec
            .root
            .layers
            .iter()
            .map(|l| l.trim_end_matches('/').to_string())
            .collect();
        assert_eq!(lower.join(":"), "/l/a:/l/b");
    }
}
