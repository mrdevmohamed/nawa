//! Kernel capability probes — `nawa sandbox-probe <op>` (design §4.1).
//!
//! One probe per fresh process: the caller's namespaces stay untouched, and
//! every probing op takes its own private mount/net namespace first, so a
//! probe can never affect the environment it reports about.

use anyhow::{bail, Result};
use rustix::mount::MountFlags;
use rustix::process::{pivot_root, waitpid, Pid, WaitOptions};
use rustix::thread::{unshare_unsafe, UnshareFlags};
use std::path::Path;

use super::init::ensure_identity;
use super::mounts;
use super::spec::SandboxSpec;
use super::sys;

// ---------------------------------------------------------------------------
// the probes
// ---------------------------------------------------------------------------

/// All probes, for `nawa sandbox-probe` (and the integration tests).
pub const PROBES: &[&str] = &[
    "userns", "mounts", "ro-bind", "proc", "pivot", "netns-lo", "overlay",
];

/// Run one probe; `Ok(true)` = supported, `Ok(false)` = unavailable.
pub fn probe(op: &str) -> Result<bool> {
    let euid = sys::geteuid();
    let egid = sys::getegid();
    let userns = matches!(op, "userns" | "mounts" | "ro-bind" | "proc" | "pivot" | "netns-lo" | "overlay");
    if userns {
        // SAFETY: single-threaded probe process; nothing observes unshared
        // descriptors across threads (there are no other threads).
        if unsafe { unshare_unsafe(UnshareFlags::NEWUSER) }.is_err() {
            return Ok(false);
        }
        // self-write the own-uid map the unshare needs to be usable
        ensure_identity(euid, egid)?;
    }
    let ok = match op {
        "userns" => true,
        "mounts" => {
            // SAFETY: as above.
            if unsafe { unshare_unsafe(UnshareFlags::NEWNS) }.is_err() {
                return Ok(false);
            }
            mounts::mount_rprivate("/").is_ok() && mounts::tmpfs("/tmp/.nawa-probe", "size=1m").is_ok()
        }
        "ro-bind" => {
            // SAFETY: as above.
            if unsafe { unshare_unsafe(UnshareFlags::NEWNS) }.is_err() {
                return Ok(false);
            }
            // bind a host dir read-only: the core of the `sandbox` level
            let t = std::env::temp_dir().join(format!("nawa-probe-ro-{}", std::process::id()));
            let _ = std::fs::create_dir_all(t.join("self"));
            let src = t.to_string_lossy().into_owned();
            let dst = t.join("self").to_string_lossy().into_owned();
            let r = mounts::mbind(&src, &dst, false)
                .and_then(|_| mounts::remount_ro(&dst, &probe_spec(""), "probe"));
            let _ = std::fs::remove_dir_all(&t);
            r.is_ok()
        }
        "proc" | "pivot" => {
            // SAFETY: as above.
            if unsafe { unshare_unsafe(UnshareFlags::NEWNS | UnshareFlags::NEWPID) }.is_err() {
                return Ok(false);
            }
            let pid = sys::fork()?;
            if pid == 0 {
                let ok = match op {
                    "proc" => {
                        let d = std::env::temp_dir().join(format!("nawa-probe-proc-{}", std::process::id()));
                        let _ = std::fs::create_dir_all(&d);
                        mounts::fresh_proc_or_mask(&probe_spec(""), &d).is_ok()
                    }
                    _ => {
                        let d = std::env::temp_dir().join(format!("nawa-probe-pv-{}", std::process::id()));
                        let _ = std::fs::create_dir_all(d.join("old"));
                        let s = d.to_string_lossy().into_owned();
                        let old = d.join("old").to_string_lossy().into_owned();
                        // pivot_root requires the future root to be a mount
                        mounts::mbind(&s, &s, false).is_ok()
                            && pivot_root(s.as_str(), old.as_str()).is_ok()
                    }
                };
                sys::exit_now(if ok { 0 } else { 3 });
            }
            let st = waitpid(Pid::from_raw(pid), WaitOptions::empty())
                .ok()
                .flatten();
            st.is_some_and(|(_, st)| st.exited() && st.exit_status() == Some(0))
        }
        "netns-lo" => {
            // SAFETY: as above.
            if unsafe { unshare_unsafe(UnshareFlags::NEWNET) }.is_err() {
                return Ok(false);
            }
            sys::bring_up_loopback().is_ok()
        }
        "overlay" => {
            // Phase 5: overlayfs with lowerdirs inside a user namespace —
            // kernel ≥ 5.11 (older kernels refuse with EPERM/EINVAL).
            // Two one-file layers must both show up in the merged view.
            // SAFETY: as above.
            if unsafe { unshare_unsafe(UnshareFlags::NEWNS) }.is_err() {
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

// The forked probe child reports success as exit 0, failure as exit 3
// (`_exit`, never a flush). The parent maps that back to a bool; a waitpid
// error or "no status" (impossible when blocking) counts as failure, which
// is what the C original's default `st = 0` mapped to as well.

/// The exact mount `mounts::mount_layers` performs, on probe-built paths (no
/// spec plumbing needed — the probe only answers "does this kernel allow it").
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
    let croot = mounts::cstr(&merged.to_string_lossy());
    let cdata = mounts::cstr(&data);
    rustix::mount::mount(
        c"overlay",
        &*croot,
        c"overlay",
        MountFlags::empty(),
        Some(cdata.as_c_str()),
    )
    .map_err(|e| anyhow::Error::msg(format!("overlay probe mount: {e}")))?;
    Ok(())
}

fn probe_spec(scratch: &str) -> SandboxSpec {
    SandboxSpec {
        version: 1,
        root: Default::default(),
        build: super::spec::BuildPaths {
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
