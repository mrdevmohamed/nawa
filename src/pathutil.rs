//! Path containment helpers (P2 dedupe).
//!
//! Several callers need "is this path under that root, following symlinks?"
//! with slightly different error handling (bail vs skip). The helpers here
//! isolate the filesystem walk so the semantics stay identical while the
//! canonicalization logic lives in one place.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

/// Resolve `path` to its nearest existing ancestor, following symlinks,
/// returning an error if a dangling/looping symlink is encountered that
/// cannot be shown to stay inside `root`.
///
/// This is the core loop used by [`ensure_inside_root`] and mirrors the
/// containment checks in `slim::split` and `plugins::manifest` with a
/// stricter, ancestor-aware variant for the install path.
pub(crate) fn resolve_ancestor_containing(path: &Path) -> Result<PathBuf> {
    let mut cur = path;
    loop {
        match std::fs::canonicalize(cur) {
            Ok(p) => return Ok(p),
            Err(_) => {
                if std::fs::symlink_metadata(cur).map(|m| m.is_symlink()).unwrap_or(false) {
                    bail!("refusing to write through unresolvable symlink {}", cur.display());
                }
                match cur.parent() {
                    Some(p) => cur = p,
                    None => return Ok(cur.to_path_buf()),
                }
            }
        }
    }
}

/// Refuse to touch `path` when resolving it (following symlinks) would leave
/// `root`. The target root is a *tree*, not a chroot: nawa runs from another
/// root (e.g. a build chroot installing into `/sysroot`), so an absolute
/// symlink inside the target such as `usr/lib64 -> /usr/lib` resolves to the
/// RUNNING system's `/usr/lib` and an install would silently rewrite that
/// instead. Relative links that stay inside `root` (merged-usr `lib -> usr/lib`)
/// are fine; with `root` = `/` everything is inside.
pub(crate) fn ensure_inside_root(root: &Path, path: &Path) -> Result<()> {
    let real_root = std::fs::canonicalize(root)
        .with_context(|| format!("resolve root {}", root.display()))?;
    let resolved = resolve_ancestor_containing(path).with_context(|| {
        format!(
            "refusing to write through unresolvable symlink {} (target root {})",
            path.display(),
            root.display()
        )
    })?;
    if !resolved.starts_with(&real_root) {
        bail!(
            "refusing to write through symlink: {} resolves to {}, outside the target root {}",
            path.display(),
            resolved.display(),
            real_root.display()
        );
    }
    Ok(())
}

/// Simple containment check without ancestor resolution: both paths must exist.
/// Returns `true` iff `canonicalize(path)` starts with `canonicalize(root)`.
/// Used by `slim::split` and `plugins::manifest` where the caller decides
/// whether a failure is a skip (`continue`) or an error (`bail!`).
pub(crate) fn is_under_root(root: &Path, path: &Path) -> Result<bool> {
    let real_root = std::fs::canonicalize(root)
        .with_context(|| format!("resolve root {}", root.display()))?;
    let real_path = std::fs::canonicalize(path)
        .with_context(|| format!("resolve path {}", path.display()))?;
    Ok(real_path.starts_with(&real_root))
}
