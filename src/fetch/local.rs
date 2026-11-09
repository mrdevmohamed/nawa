//! Local file copy (`cp -a` parity) without shelling out.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

pub(crate) fn copy_local(src: &Path, dest: &Path) -> Result<()> {
    use std::collections::HashMap;
    use std::os::unix::fs::MetadataExt;

    if !src.is_dir() {
        std::fs::copy(src, dest).with_context(|| format!("copy {}", src.display()))?;
        copy_file_times(src, dest)?;
        return Ok(());
    }
    std::fs::create_dir_all(dest).with_context(|| format!("create {}", dest.display()))?;
    let as_root = crate::diag::is_root();
    let mut seen: HashMap<(u64, u64), PathBuf> = HashMap::new(); // (dev, ino) -> first copy
    let mut dir_times: Vec<(PathBuf, PathBuf)> = Vec::new();
    for entry in walkdir::WalkDir::new(src).follow_links(false) {
        let e = entry.with_context(|| format!("walk {}", src.display()))?;
        let md = e.path().symlink_metadata()?;
        let rel = e.path().strip_prefix(src)?;
        let dst = dest.join(rel);
        let ft = md.file_type();
        if ft.is_dir() {
            std::fs::create_dir_all(&dst).with_context(|| format!("create {}", dst.display()))?;
            if as_root {
                preserve_owner(&dst, &md);
            }
            dir_times.push((e.path().to_path_buf(), dst));
        } else if ft.is_symlink() {
            let target = std::fs::read_link(e.path())?;
            let _ = std::fs::remove_file(&dst);
            std::os::unix::fs::symlink(&target, &dst)
                .with_context(|| format!("symlink {} -> {}", dst.display(), target.display()))?;
            copy_file_times(e.path(), &dst)?;
        } else if ft.is_file() {
            let key = (md.dev(), md.ino());
            match seen.get(&key) {
                Some(first) => {
                    let _ = std::fs::remove_file(&dst);
                    std::fs::hard_link(first, &dst)
                        .with_context(|| format!("hard link {} -> {}", first.display(), dst.display()))?;
                    // shares an inode with the first copy, which already has
                    // its times set — nothing to do
                }
                None => {
                    std::fs::copy(e.path(), &dst)
                        .with_context(|| format!("copy {}", e.path().display()))?;
                    seen.insert(key, dst.clone());
                    copy_file_times(e.path(), &dst)?;
                }
            }
            if as_root {
                preserve_owner(&dst, &md);
            }
        } else {
            crate::nawa_warn!("skipping special file {}", e.path().display());
        }
    }
    // directory timestamps go last: writing children bumps parent mtimes
    for (s, d) in dir_times.iter().rev() {
        copy_file_times(s, d)?;
    }
    Ok(())
}

fn preserve_owner(dst: &Path, md: &std::fs::Metadata) {
    use std::os::unix::fs::MetadataExt;
    use rustix::fs::{Gid, Uid};
    // best effort, like cp -a on a filesystem that ignores ownership
    let _ = rustix::fs::chown(
        dst,
        Some(Uid::from_raw(md.uid())),
        Some(Gid::from_raw(md.gid())),
    );
}

fn copy_file_times(src: &Path, dst: &Path) -> Result<()> {
    let md = std::fs::symlink_metadata(src).with_context(|| format!("stat {}", src.display()))?;
    let modified = md.modified().unwrap_or(std::time::UNIX_EPOCH);
    let mtime = filetime::FileTime::from_system_time(modified);
    let atime = filetime::FileTime::from_system_time(md.accessed().unwrap_or(modified));
    filetime::set_symlink_file_times(dst, atime, mtime)
        .with_context(|| format!("set times on {}", dst.display()))
}
