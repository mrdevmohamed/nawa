//! Layout and text-leak checks.


use super::elf::ElfInfo;
use super::{LintCtx, LintReport, ALLOWED_TOP, KERNEL_MANAGED, SKELETON_TOP};

pub(crate) fn check_layout(rel: &str, md: &std::fs::Metadata, rep: &mut LintReport) {
    let top = rel.split('/').next().unwrap_or("");
    let is_file = md.is_file();
    let is_link = md.file_type().is_symlink();
    if !ALLOWED_TOP.contains(&top) && !SKELETON_TOP.contains(&top) {
        rep.errors.push(format!(
            "{rel}: outside the allowed trees ({})",
            [ALLOWED_TOP, SKELETON_TOP].concat().join(" ")
        ));
        return;
    }
    if SKELETON_TOP.contains(&top) && rel != top {
        // under a mount point: only `root/.<rcfile>` (a plain file) is legal
        let why = if KERNEL_MANAGED.contains(&top) {
            format!("{top} is kernel-managed — packages may not stage anything there")
        } else {
            format!("{top} is reserved for the site/user — packages may not stage content there")
        };
        if !(is_file && top == "root" && root_dot_rc(rel)) {
            rep.errors.push(format!("{rel}: {why}"));
        }
    }
    if top == "usr" && rel.starts_with("usr/local/") && (is_file || is_link) {
        // the skeleton may create the /usr/local directories; nothing may
        // live in them — that path belongs to the site administrator
        rep.errors.push(format!("{rel}: nothing may be installed into /usr/local"));
    }
    use std::os::unix::fs::PermissionsExt;
    let mode = md.permissions().mode();
    if !md.file_type().is_symlink() && mode & 0o002 != 0 {
        // world-writable dirs are legitimate only with the sticky bit
        // (e.g. /tmp, /var/tmp, /dev/shm)
        let sticky_tmp = md.is_dir() && mode & 0o1000 != 0;
        if !sticky_tmp {
            rep.errors.push(format!(
                "{rel}: world-writable (mode {:o}) — packaging mistake or attack",
                mode & 0o7777
            ));
        }
    }
}

/// The skeleton's /root rc file: exactly two components, `root/<dot-name>`.
pub(crate) fn root_dot_rc(rel: &str) -> bool {
    let mut parts = rel.split('/');
    let top = parts.next() == Some("root");
    match (parts.next(), parts.next()) {
        (Some(second), None) => top && second.starts_with('.'),
        _ => false,
    }
}

pub(crate) fn check_elf(rel: &str, info: &ElfInfo, ctx: &LintCtx, rep: &mut LintReport) {
    // leaked rpath/runpath
    for path in [&info.rpath, &info.runpath].into_iter().flatten() {
        if path.contains("/build/") {
            rep.errors
                .push(format!("{rel}: rpath leaks the build tree: {path}"));
        }
        if let Some(hb) = ctx.host_builddir {
            if !hb.is_empty() && path.contains(hb) {
                rep.errors
                    .push(format!("{rel}: rpath leaks the build machine: {path}"));
            }
        }
    }
    // run-dependency check (Phase 6a: warn)
    for needed in &info.needed {
        if let Some(owner) = ctx.sonames.get(needed) {
            if owner != "SELF" && !ctx.rundeps.iter().any(|d| d == owner) {
                rep.warnings.push(format!(
                    "{rel}: links {needed} (owned by `{owner}`) but depends.run does not list it"
                ));
            }
        }
        // an unindexed soname is usually part of libc/libgcc (present in
        // base-devel, never listed) — not worth warning about
    }
}

pub(crate) fn check_text_leaks(rel: &str, data: &[u8], ctx: &LintCtx, rep: &mut LintReport) {
    let text = String::from_utf8_lossy(data);
    // the two staging pins every recipe sees, plus the per-recipe insides
    // (§5.3): <build_inside>/assets for recipes that declare [assets] —
    // <build_inside>/fetch joins with the ecosystem phases
    let mut needles = vec!["/build/src", "/build/pkg"];
    needles.extend(ctx.extra_leak_paths.iter().copied());
    for n in needles {
        if text.contains(n) {
            rep.errors.push(format!("{rel}: contains the sandbox path {n}"));
        }
    }
    if let Some(hb) = ctx.host_builddir {
        if !hb.is_empty() && text.contains(hb) {
            rep.errors
                .push(format!("{rel}: contains the build machine path {hb}"));
        }
    }
}
