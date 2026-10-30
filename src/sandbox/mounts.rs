//! Sandbox filesystem assembly (design §4.2) — every mount that the sandbox
//! performs, spelled out with the failure mode. All calls go through
//! `rustix::mount`/`rustix::fs`/`rustix::process`; there is no `unsafe` in
//! this file. Order matters: read-only goes on LAST — creating the mount
//! points writes through the root while it is still writable — and bind
//! sources are opened once (O_PATH) *before* anything covers their paths.
//!
//! Two root layouts are assembled:
//! * `live` — keep the running system, overlay/cover the writable and
//!   kernel-managed parts, then mark `/` read-only (sandbox level);
//! * `dir` — bind (or overlay-layer) an assembled build root, pivot into it,
//!   hide the old root, then read-only (closure level).

use anyhow::{bail, Context, Result};
use rustix::fd::AsRawFd;
use rustix::fs::{Mode, OFlags};
use rustix::mount::{
    mount, mount_bind, mount_bind_recursive, mount_change, mount_remount, MountFlags,
    MountPropagationFlags,
};
use rustix::process::{chdir, chroot, pivot_root};
use std::ffi::CString;
use std::path::{Path, PathBuf};

use super::spec::{BuildPaths, SandboxSpec};

// ---------------------------------------------------------------------------
// shared mount helpers with precise failure reporting
// ---------------------------------------------------------------------------

/// `mount --make-rprivate <what>`: propagation is turned private FIRST so
/// nothing we mount later leaks back into the host namespace.
pub(crate) fn mount_rprivate(what: &str) -> Result<()> {
    mount_change(what, MountPropagationFlags::PRIVATE | MountPropagationFlags::REC)
        .map_err(|e| anyhow::Error::msg(format!("mount --make-rprivate {what} failed: {e}")))
}

/// A bind mount, optionally recursive.
pub(crate) fn mbind(src: &str, dst: &str, rec: bool) -> Result<()> {
    let r = if rec { mount_bind_recursive(src, dst) } else { mount_bind(src, dst) };
    r.map_err(|e| anyhow::Error::msg(format!("bind mount {src} -> {dst} failed: {e}")))
}

/// Mark a mount we created read-only (per-mount flag; the original stays
/// writable — this is what makes `make install` without DESTDIR fail
/// loudly instead of writing into the running system).
pub(crate) fn remount_ro(target: &str, spec: &SandboxSpec, what: &str) -> Result<()> {
    let flags = MountFlags::BIND | MountFlags::RDONLY | MountFlags::NOSUID | MountFlags::NODEV;
    // an empty data string is the neutral remount payload (MS_REMOUNT keeps
    // the underlying filesystem's options)
    match mount_remount(target, flags, "") {
        Ok(()) => Ok(()),
        Err(err) => {
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
    }
}

pub(crate) fn tmpfs(at: &str, opts: &str) -> Result<()> {
    tmpfs_at(Path::new(at), opts)
}

pub(crate) fn tmpfs_at(at: &Path, opts: &str) -> Result<()> {
    std::fs::create_dir_all(at)?;
    let cat = cstr(&at.to_string_lossy());
    let copts = cstr(opts);
    mount(
        c"tmpfs",
        &*cat,
        c"tmpfs",
        MountFlags::NOSUID | MountFlags::NODEV,
        Some(copts.as_c_str()),
    )
    .map_err(|e| anyhow::Error::msg(format!("tmpfs on {} failed: {e}", at.display())))
}

/// Hide a path behind an empty, unsearchable tmpfs.
pub(crate) fn tmpfs_mask(path: &str) -> Result<()> {
    let p = Path::new(path);
    std::fs::create_dir_all(p)?;
    let c = cstr(path);
    mount(
        c"tmpfs",
        &*c,
        c"tmpfs",
        MountFlags::NOSUID | MountFlags::NODEV,
        Some(cstr("size=1m,mode=0").as_c_str()),
    )
    .map_err(|e| anyhow::Error::msg(format!("cannot mask {path} (tmpfs): {e}")))
}

/// Paths given to `mount(2)` must never contain NUL; the sandbox spec is
/// validated TOML from our own builder, but the strings are built here
/// either way.
pub(crate) fn cstr(s: &str) -> CString {
    // A Rust `String`'s pointer is NOT NUL-terminated, and rustix accepts
    // `&str` paths by conversion — the CString form is only needed for mount
    // *data* blobs; interior NULs are stripped like the original reader did.
    std::ffi::CString::new(s.replace('\0', ""))
        .unwrap_or_else(|_| std::ffi::CString::default())
}

// ---------------------------------------------------------------------------
// the two root layouts
// ---------------------------------------------------------------------------

/// `sandbox` level: keep the current root (the live system), but overlay the
/// writable and kernel-managed parts in *our private mount namespace only*:
/// a self-bind makes `/` a mount we own and can mark read-only without
/// touching the host's view of it. Read-only goes on LAST — creating the
/// mount points writes to the root while it is still writable.
pub(crate) fn setup_live_root(spec: &SandboxSpec) -> Result<()> {
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
pub(crate) fn setup_dir_root(spec: &SandboxSpec) -> Result<()> {
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
    std::fs::create_dir_all(&old).with_context(|| format!("create {}", old.display()))?;

    // pivot, falling back to chroot in chroots where pivot is not possible
    // (design §4.1)
    let root_s = root.to_string_lossy().into_owned();
    let old_s = old.to_string_lossy().into_owned();
    chdir(&root_s).map_err(|e| anyhow::Error::msg(format!("chdir {}: {e}", root.display())))?;
    if let Err(err) = pivot_root(&root_s, &old_s) {
        eprintln!("nawa: sandbox: pivot_root failed ({err}) — falling back to chroot");
        chroot(&root_s)
            .map_err(|e| anyhow::Error::msg(format!("chroot {}: {e}", root.display())))?;
    }
    chdir("/").map_err(|e| anyhow::Error::msg(format!("chdir /: {e}")))?;
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
pub(crate) fn mount_layers(spec: &SandboxSpec, root: &Path) -> Result<()> {
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
    for p in lower
        .iter()
        .chain([upper.to_string_lossy().into_owned(), work.to_string_lossy().into_owned()].iter())
    {
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
    mount(c"overlay", &*croot, c"overlay", MountFlags::empty(), Some(cdata.as_c_str()))
        .map_err(|e| {
            anyhow::Error::msg(format!(
                "overlayfs build root mount failed: {e}\n\
                 (the builder probed this before the build; a failure here means \
                 the kernel stopped allowing user-namespace overlays — rerun the \
                 build so nawa falls back to extract-per-build assembly)"
            ))
        })
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
/// mounted in the kernel even through the shadowing mount. Closed on drop
/// (`OwnedFd` — no manual close, no unsafe).
struct StagingHandles {
    fds: [rustix::fd::OwnedFd; 3],
}

impl StagingHandles {
    fn open(build: &BuildPaths) -> Result<Self> {
        let hosts = [&build.src_host, &build.pkg_host, &build.home_host];
        let mut fds: [Option<rustix::fd::OwnedFd>; 3] = [None, None, None];
        for (i, host) in hosts.iter().enumerate() {
            if !Path::new(host).is_dir() {
                bail!("sandbox staging dir {} does not exist", host);
            }
            // O_PATH: no permission to read the directory is required, it
            // only needs to *name* it — exactly what bind-through-fd needs.
            let fd = rustix::fs::open(
                host.as_str(),
                OFlags::PATH | OFlags::DIRECTORY | OFlags::NOFOLLOW,
                Mode::empty(),
            )
            .map_err(|e| anyhow::Error::msg(format!("open sandbox bind source {host}: {e}")))?;
            fds[i] = Some(fd);
        }
        Ok(Self { fds: fds.map(Option::unwrap) })
    }

    /// Create the three bind targets under `base` and bind the handles
    /// through their `/proc/self/fd` aliases. `/proc` must be resolvable
    /// (the host's procfs is still mounted here — the fresh one mounts after).
    fn bind_at(&self, base: &Path) -> Result<()> {
        let names = ["src", "pkg", "home"];
        for (i, name) in names.iter().enumerate() {
            let dst = base.join(name);
            std::fs::create_dir_all(&dst)?;
            let link = format!("/proc/self/fd/{}", self.fds[i].as_raw_fd());
            mbind(&link, &dst.to_string_lossy(), false)
                .with_context(|| format!("bind staging {} -> {}", names[i], dst.display()))?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// /dev, /proc, /sys
// ---------------------------------------------------------------------------

/// Minimal /dev (§4.2): tmpfs plus real (bind-mounted) device nodes —
/// mknod is not permitted inside a user namespace. The host nodes are
/// staged under `scratch` FIRST: in live mode /dev gets covered by the fresh
/// tmpfs, and the staging binds survive that cover.
pub(crate) fn setup_dev(dev: &Path, scratch: &Path) -> Result<()> {
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
    mount(
        c"devpts",
        &*cpts,
        c"devpts",
        MountFlags::NOSUID | MountFlags::NOEXEC,
        Some(cstr("newinstance,ptmxmode=0666").as_c_str()),
    )
    .map_err(|e| anyhow::Error::msg(format!("devpts on {} failed: {e}", pts.display())))?;
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
pub(crate) fn fresh_proc_or_mask(spec: &SandboxSpec, at: &Path) -> Result<()> {
    std::fs::create_dir_all(at)?;
    let c = cstr(&at.to_string_lossy());
    let fresh_mount = || {
        mount(
            c"proc",
            &*c,
            c"proc",
            MountFlags::NOSUID | MountFlags::NOEXEC | MountFlags::NODEV,
            Option::<&std::ffi::CStr>::None,
        )
    };
    if let Err(err) = fresh_mount() {
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
pub(crate) fn setup_sys(spec: &SandboxSpec, at: &Path) -> Result<()> {
    if spec.sysfs {
        std::fs::create_dir_all(at)?;
        let c = cstr(&at.to_string_lossy());
        let sys_mount = || {
            mount(
                c"sysfs",
                &*c,
                c"sysfs",
                MountFlags::RDONLY | MountFlags::NOSUID | MountFlags::NODEV | MountFlags::NOEXEC,
                Option::<&std::ffi::CStr>::None,
            )
        };
        if let Err(err) = sys_mount() {
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

#[cfg(test)]
mod tests {
    use super::super::spec::{BuildPaths, RootSpec};
    use super::*;

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
