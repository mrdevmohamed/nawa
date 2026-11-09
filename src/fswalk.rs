//! Shared sorted directory walks.
//!
//! The staged package tree (`pkgdir`) is walked by lint, slim (cleanup/split/
//! strip) and archive in the same sorted order. This module centralises the
//! `WalkDir::new(root).sort_by_file_name()` pattern so a change to sorting or
//! error handling (e.g. `follow_links`) is made once.

use std::path::{Path, PathBuf};

/// All entries under `root` (excluding `root` itself) in lexicographic file-name
/// order, with walk errors skipped. Matches the `lint` / `slim` passes that
/// silently ignore a disappearing file mid-build.
#[inline]
pub(crate) fn entries_sorted_ok(root: &Path) -> Vec<walkdir::DirEntry> {
    walkdir::WalkDir::new(root)
        .sort_by_file_name()
        .into_iter()
        .filter_map(|e| e.ok())
        .collect()
}

/// Sorted relative paths for regular files under `root`.
#[allow(dead_code)]
#[inline]
pub(crate) fn relative_files_sorted(root: &Path) -> Vec<PathBuf> {
    entries_sorted_ok(root)
        .into_iter()
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| {
            e.path()
                .strip_prefix(root)
                .ok()
                .filter(|r| !r.as_os_str().is_empty())
                .map(|r| r.to_path_buf())
        })
        .collect()
}

/// Sorted `Result` entries (errors preserved) for callers that fail on a walk
/// error (e.g. `archive` where a permission error should abort the build).
#[inline]
pub(crate) fn entries_sorted(root: &Path) -> Vec<std::result::Result<walkdir::DirEntry, walkdir::Error>> {
    walkdir::WalkDir::new(root)
        .sort_by_file_name()
        .into_iter()
        .collect()
}
