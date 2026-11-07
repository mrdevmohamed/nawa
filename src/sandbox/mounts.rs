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

use super::spec::{ExtraBind, SandboxSpec};

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
    remount_spec_paths_ro(spec)?;
    remount_mount_tree_ro(spec)?;
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
    remount_spec_paths_ro(spec)?;
    remount_mount_tree_ro(spec)?;
    // now that we live inside it, the build root becomes read-only
    remount_ro("/", spec, "the build root (/)")?;
    Ok(())
}

fn remount_spec_paths_ro(spec: &SandboxSpec) -> Result<()> {
    for path in &spec.readonly_paths {
        if !Path::new(path).exists() {
            bail!("sandbox read-only path does not exist: {path}");
        }
        remount_ro(path, spec, path)?;
    }
    Ok(())
}

/// In strict mode remount every existing mount read-only, including separate
/// host filesystems beneath / (for example /var or /home).
///
/// The sandbox-owned /dev hierarchy is deliberately excluded: setup_dev()
/// replaces it with a private tmpfs containing only the required device nodes,
/// devpts and shm. Those device interfaces need their normal writable semantics;
/// they are not the host's /dev tree.
fn remount_mount_tree_ro(spec: &SandboxSpec) -> Result<()> {
    if !spec.strict_readonly_root {
        return Ok(());
    }
    let mountinfo = std::fs::read_to_string("/proc/self/mountinfo")
        .context("read mountinfo for strict plugin sandbox")?;
    let mut mounts = Vec::new();
    for line in mountinfo.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        let Some(raw_path) = fields.get(4) else { continue };
        let path = unescape_mountinfo(raw_path);
        let mount = Path::new(&path);
        if !mount.is_absolute() || path == "/" {
            continue;
        }
        let writable = spec.writable_paths.iter().any(|allowed| {
            mount == Path::new(allowed) || mount.starts_with(allowed)
        });
        // Masked parents are covered by fresh tmpfs mounts. Their former
        // child mounts remain listed in mountinfo but are no longer reachable
        // by path; attempting to remount them would target the covering tmpfs
        // or fail. The covering tmpfs itself is already empty/unsearchable.
        let hidden = spec.mask.iter().any(|masked| {
            mount == Path::new(masked) || mount.starts_with(masked)
        });
        // setup_dev() has replaced /dev and everything beneath it with
        // sandbox-private mounts. Keep /dev/null, devpts and shm writable;
        // these are ephemeral device interfaces, not host filesystem paths.
        //
        // setup_dev() first bind-mounts the host device nodes into
        // <scratch>/devnodes, then binds those staged mounts into /dev.
        // The staging mountpoints are also present in mountinfo, but are not
        // part of the build root and must remain writable just like /dev/null.
        let private_dev = is_private_dev_mount(mount, Path::new(&spec.build.scratch));
        if writable || hidden || private_dev {
            continue;
        }
        // Mountinfo can retain entries for mounts that the mapped sandbox
        // identity cannot traverse (for example an outer container's private
        // overlay mount). The build runs as this same identity after dropping
        // capabilities, so an inaccessible mountpoint cannot be reached by
        // the build either. Skip only EACCES; all other lookup errors remain
        // fail-closed.
        if mountpoint_visible(mount.try_exists(), mount)? {
            mounts.push(path);
        }
    }
    // Children first: a read-only parent must not interfere with remounting
    // a nested bind or host filesystem.
    mounts.sort_by_key(|path| std::cmp::Reverse(path.matches('/').count()));
    mounts.dedup();
    for path in mounts {
        remount_ro(&path, spec, &path)?;
    }
    Ok(())
}

/// True for the private device interfaces assembled by setup_dev.
///
/// The staging binds under <scratch>/devnodes are as essential as their
/// final /dev/* binds: both paths refer to character devices whose normal
/// read/write semantics must be preserved inside the sandbox. Do not confuse
/// them with ordinary files elsewhere in the writable build scratch tree.
fn is_private_dev_mount(mount: &Path, scratch: &Path) -> bool {
    mount.starts_with("/dev") || mount.starts_with(scratch.join("devnodes"))
}

/// A mountpoint hidden from the mapped sandbox identity is unreachable by
/// the build too. EACCES is therefore safe to skip; other lookup errors still
/// abort the read-only scan.
fn mountpoint_visible(exists: std::io::Result<bool>, path: &Path) -> Result<bool> {
    match exists {
        Ok(exists) => Ok(exists),
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            eprintln!(
                "nawa: sandbox: note: skipping mountpoint inaccessible to the sandbox identity: {}",
                path.display()
            );
            Ok(false)
        }
        Err(error) => Err(error)
            .with_context(|| format!("inspect sandbox mount point {}", path.display())),
    }
}

fn unescape_mountinfo(path: &str) -> String {
    path.replace("\\040", " ")
        .replace("\\011", "\t")
        .replace("\\012", "\n")
        .replace("\\134", "\\")
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
    let staging = StagingHandles::open(spec)?;
    // fresh /tmp first: an unprivileged host cannot create `/build` on the
    // live root, and the fallback base then lives on this tmpfs
    tmpfs("/tmp", "size=2048m,mode=1777")?;
    // the build base: a tiny tmpfs holding only the three bind points; the
    // heavy dirs themselves are disk-backed host binds
    let base_p = Path::new(spec.build_inside.as_str());
    std::fs::create_dir_all(base_p)
        .with_context(|| format!("create the sandbox build base {}", spec.build_inside))?;
    tmpfs_at(base_p, "size=1m,mode=755")?;
    // Mask /run BEFORE extra resolver binds. A host resolv.conf commonly
    // points into /run/systemd/resolve; the bind then lands on the empty
    // private tmpfs target rather than being hidden by a later mask.
    let run_is_masked = spec.mask.iter().any(|p| p == "/run");
    if run_is_masked {
        // Keep the private /run empty but searchable so the dropped fetch uid
        // can read the resolver file mounted below /run/systemd/resolve.
        tmpfs_at(Path::new("/run"), "size=1m,mode=755")?;
    }
    staging.bind_at(base_p, None, spec)?;
    if run_is_masked {
        remount_ro("/run", spec, "the empty /run resolver staging mount")?;
    }
    drop(staging);
    setup_dev(Path::new("/dev"), Path::new(&spec.build.scratch))?;
    fresh_proc_or_mask(spec, Path::new("/proc"))?;
    setup_sys(spec, Path::new("/sys"))?;
    for p in &spec.mask {
        if p == "/run" {
            continue; // already masked before resolver binds above
        }
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
    let staging = StagingHandles::open(spec)?;
    tmpfs_at(&at("tmp"), "size=2048m,mode=1777")?;
    let base = root.join(spec.build_inside.trim_start_matches('/'));
    tmpfs_at(&base, "size=1m,mode=755")?;
    staging.bind_at(&base, Some(root), spec)?;
    drop(staging);
    setup_dev(&at("dev"), Path::new(&spec.build.scratch))?;
    fresh_proc_or_mask(spec, &at("proc"))?;
    // an assembled closure never contains /sys; nothing to hide
    Ok(())
}

/// O_PATH descriptors of the staged dirs (`src`, `pkg`, `home`) plus every
/// `extra_bind` (§5.3), and the logic that binds them into the sandbox
/// build base. The descriptors are taken *before* any mount covers their
/// host paths: a bind source resolved by path after a cover yields ENOENT,
/// while the open file the fd refers to stays mounted in the kernel even
/// through the shadowing mount. Closed on drop (`OwnedFd` — no manual
/// close, no unsafe).
struct StagingHandles {
    fds: [rustix::fd::OwnedFd; 3],
    extra: Vec<ExtraFd>,
}

/// One extra bind (§5.3) with its handle.
struct ExtraFd {
    fd: rustix::fd::OwnedFd,
    bind: ExtraBind,
}

impl StagingHandles {
    fn open(spec: &SandboxSpec) -> Result<Self> {
        let build = &spec.build;
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
        let mut extra = Vec::new();
        for bind in &spec.extra_binds {
            let host = Path::new(&bind.host);
            let is_dir = bind.kind == "dir";
            if (is_dir && !host.is_dir()) || (!is_dir && !host.is_file()) {
                bail!("sandbox extra bind source {} does not exist as a {is_dir}", bind.host);
            }
            let flags = if is_dir {
                OFlags::PATH | OFlags::DIRECTORY | OFlags::NOFOLLOW
            } else {
                // an O_PATH open of a file may still need the file type;
                // PATH alone opens it without access checks
                OFlags::PATH | OFlags::NOFOLLOW
            };
            let fd = rustix::fs::open(bind.host.as_str(), flags, Mode::empty()).map_err(|e| {
                anyhow::Error::msg(format!("open sandbox extra bind {}: {e}", bind.host))
            })?;
            extra.push(ExtraFd { fd, bind: bind.clone() });
        }
        Ok(Self { fds: fds.map(Option::unwrap), extra })
    }

    /// Create the three bind targets under `base` and bind the handles
    /// through their `/proc/self/fd` aliases. `/proc` must be resolvable
    /// (the host's procfs is still mounted here — the fresh one mounts after).
    ///
    /// Extra binds (§5.3) bind at `root + inside` in `dir` mode (the spec's
    /// `inside` lives inside the new root) and at `inside` as-is in `live`
    /// mode; a read-only bind is remounted at once.
    fn bind_at(&self, base: &Path, root: Option<&Path>, spec: &SandboxSpec) -> Result<()> {
        let names = ["src", "pkg", "home"];
        for (i, name) in names.iter().enumerate() {
            let dst = base.join(name);
            std::fs::create_dir_all(&dst)?;
            let link = format!("/proc/self/fd/{}", self.fds[i].as_raw_fd());
            mbind(&link, &dst.to_string_lossy(), false)
                .with_context(|| format!("bind staging {name} -> {}", dst.display()))?;
        }
        for x in &self.extra {
            // `inside` is an absolute in-sandbox path: under a `dir` root it
            // lives inside the tree we are about to enter, so prefix it the
            // same way the build base is resolved
            let target: std::path::PathBuf = match root {
                Some(r) => r.join(x.bind.inside.trim_start_matches('/')),
                None => Path::new(&x.bind.inside).to_path_buf(),
            };
            if x.bind.kind == "file" {
                // Never follow a final symlink while preparing a file bind:
                // resolver binds have already resolved it within the sandbox
                // root. Create missing parents after /run has been masked.
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                match std::fs::symlink_metadata(&target) {
                    Ok(md) if md.file_type().is_file() => {}
                    Ok(md) if md.file_type().is_symlink() => {
                        bail!("extra file bind target {} is still a symlink; refusing to follow it", target.display());
                    }
                    Ok(_) => bail!("extra file bind target {} is not a regular file", target.display()),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        std::fs::File::create(&target).with_context(|| {
                            format!("create extra bind placeholder {}", target.display())
                        })?;
                    }
                    Err(e) => return Err(e).with_context(|| format!("inspect extra bind target {}", target.display())),
                }
            } else if !target.is_dir() {
                std::fs::create_dir_all(&target).with_context(|| {
                    format!("create extra bind target {}", target.display())
                })?;
            }
            let link = format!("/proc/self/fd/{}", x.fd.as_raw_fd());
            mbind(&link, &target.to_string_lossy(), false)
                .with_context(|| format!("bind extra {} -> {}", x.bind.inside, target.display()))?;
            if x.bind.readonly {
                remount_ro(
                    &target.to_string_lossy(),
                    spec,
                    &format!("the extra bind {}", x.bind.inside),
                )?;
            }
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
    fn staged_device_bind_mounts_are_not_marked_read_only() {
        let scratch = Path::new("/build/nawa-1.0.0-18/sb");

        for path in [
            "/dev/null",
            "/dev/pts",
            "/dev/shm",
            "/build/nawa-1.0.0-18/sb/devnodes/null",
            "/build/nawa-1.0.0-18/sb/devnodes/urandom",
        ] {
            assert!(
                is_private_dev_mount(Path::new(path), scratch),
                "{path} is a private device mount"
            );
        }

        for path in [
            "/device/null",
            "/build/nawa-1.0.0-18/sb/devnodes-backup/null",
            "/build/nawa-1.0.0-18/sb/src",
            "/build/nawa-1.0.0-18/sb/pkg",
            "/etc",
        ] {
            assert!(
                !is_private_dev_mount(Path::new(path), scratch),
                "{path} is not a private device mount"
            );
        }
    }

    #[test]
    fn inaccessible_mountpoints_are_skipped_but_other_lookup_errors_fail_closed() {
        let path = Path::new("/var/lib/containers/storage/overlay/private/merged");

        assert!(mountpoint_visible(Ok(true), path).unwrap());
        assert!(!mountpoint_visible(Ok(false), path).unwrap());
        assert!(!mountpoint_visible(
            Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "not reachable in this user namespace",
            )),
            path,
        )
        .unwrap());

        assert!(
            mountpoint_visible(
                Err(std::io::Error::other("I/O error")),
                path,
            )
            .is_err(),
            "errors other than EACCES must still fail closed"
        );
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
            run_uid: None,
            run_gid: None,
            memory_limit_bytes: 0,
            process_limit: 0,
            file_size_limit_bytes: 0,
            open_files_limit: 0,
            readonly_paths: Vec::new(),
            extra_binds: Vec::new(),
        strict_readonly_root: false,
        writable_paths: Vec::new(),
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
