//! Package archives: `.nawa.tar.zst`
//!
//! Layout inside the archive:
//! ```text
//! .PKGINFO          key = value metadata
//! payload/...       the package files, rooted at the target filesystem
//! ```

use anyhow::{Context, Result};
use std::fs::File;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path};
use std::time::SystemTime;

pub const PKGINFO: &str = ".PKGINFO";
pub const PAYLOAD: &str = "payload";
pub const HOOKS: &str = ".HOOKS";

/// Archive normalisation for reproducible builds
/// (docs/design/build-isolation.md §6): every entry is owned by `root:root`
/// with an empty user/group name and an mtime clamped to `source_date_epoch`.
/// The archive layout itself (entry order, zstd level 3, single thread)
/// is already deterministic.
#[derive(Debug, Clone, Copy)]
pub struct Normalize {
    pub source_date_epoch: u64,
}

/// Install/remove hooks shipped inside an archive as `.HOOKS` (TOML).
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Hooks {
    #[serde(default)]
    pub post_install: String,
    #[serde(default)]
    pub pre_remove: String,
    #[serde(default)]
    pub post_remove: String,
}

impl Hooks {
    pub fn is_empty(&self) -> bool {
        self.post_install.trim().is_empty()
            && self.pre_remove.trim().is_empty()
            && self.post_remove.trim().is_empty()
    }
}

/// Create a package archive from a staging directory (`pkgdir`).
#[allow(dead_code)] // convenience wrapper; the build engine uses create_with_normalisation
pub fn create(pkgdir: &Path, pkginfo: &str, out: &Path) -> Result<()> {
    create_with_normalisation(pkgdir, pkginfo, None, out, None)
}

/// Like `create`, additionally storing `.HOOKS` when any hook is non-empty.
pub fn create_with_hooks(
    pkgdir: &Path,
    pkginfo: &str,
    hooks: Option<&Hooks>,
    out: &Path,
) -> Result<()> {
    create_with_normalisation(pkgdir, pkginfo, hooks, out, None)
}

/// `create_with_hooks` + entry normalisation (uid/gid 0, empty uname/gname,
/// `mtime = min(mtime, source_date_epoch)`), used from the `env` isolation
/// level up so the same recipe + inputs always hash to the same archive.
pub fn create_normalized(
    pkgdir: &Path,
    pkginfo: &str,
    hooks: Option<&Hooks>,
    out: &Path,
    norm: &Normalize,
) -> Result<()> {
    create_with_normalisation(pkgdir, pkginfo, hooks, out, Some(norm))
}

fn create_with_normalisation(
    pkgdir: &Path,
    pkginfo: &str,
    hooks: Option<&Hooks>,
    out: &Path,
    norm: Option<&Normalize>,
) -> Result<()> {
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let f = File::create(out).with_context(|| format!("create {}", out.display()))?;
    let enc = zstd::Encoder::new(f, 3)?;
    let mut builder = tar::Builder::new(enc);
    // archive symlinks as symlinks — following them would dereference
    // e.g. /etc/mtab -> /proc/mounts and corrupt the stream (procfs reports
    // st_size=0 while reads return full content, breaking the tar layout)
    builder.follow_symlinks(false);

    // .PKGINFO first
    let mut header = tar::Header::new_gnu();
    header.set_size(pkginfo.len() as u64);
    header.set_mode(0o644);
    if let Some(n) = norm {
        header.set_uid(0);
        header.set_gid(0);
        header.set_mtime(n.source_date_epoch);
    }
    header.set_cksum();
    builder.append_data(&mut header, PKGINFO, pkginfo.as_bytes())?;

    // .HOOKS (optional, proper TOML so multi-line scripts survive)
    if let Some(h) = hooks {
        if !h.is_empty() {
            let body = toml::to_string(h)?;
            let mut hh = tar::Header::new_gnu();
            hh.set_size(body.len() as u64);
            hh.set_mode(0o644);
            if let Some(n) = norm {
                hh.set_uid(0);
                hh.set_gid(0);
                hh.set_mtime(n.source_date_epoch);
            }
            hh.set_cksum();
            builder.append_data(&mut hh, HOOKS, body.as_bytes())?;
        }
    }

    // payload tree
    for entry in walkdir::WalkDir::new(pkgdir).sort_by_file_name() {
        let e = entry?;
        let rel = e.path().strip_prefix(pkgdir)?;
        if rel.as_os_str().is_empty() {
            continue;
        }
        let arc_name = Path::new(PAYLOAD).join(rel);
        match norm {
            None => builder.append_path_with_name(e.path(), &arc_name)?,
            Some(n) => append_normalized(&mut builder, e.path(), &arc_name, n)?,
        }
    }

    let enc = builder.into_inner()?;
    enc.finish()?;
    Ok(())
}

/// Append one filesystem entry with a fully normalised header: uid/gid 0,
/// empty uname/gname, `mtime = min(mtime, source_date_epoch)`. The entry
/// type, mode and symlink target still come from the staged file.
fn append_normalized(
    builder: &mut tar::Builder<zstd::Encoder<'_, File>>,
    path: &Path,
    arc_name: &Path,
    norm: &Normalize,
) -> Result<()> {
    let md = std::fs::symlink_metadata(path)?;
    let mut h = tar::Header::new_gnu();
    h.set_entry_type(entry_type(&md));
    h.set_mode(md.permissions().mode() & 0o7777);
    h.set_uid(0);
    h.set_gid(0);
    // uname/gname stay empty (GNU header zero-fill = empty strings)
    let mtime = md
        .modified()
        .ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    h.set_mtime(mtime.min(norm.source_date_epoch));
    let name = arc_name.to_string_lossy().into_owned();
    if md.file_type().is_symlink() {
        let target = std::fs::read_link(path)?;
        h.set_size(0);
        h.set_link_name(target)?;
        h.set_cksum();
        builder.append_data(&mut h, name, std::io::empty())?;
    } else if md.is_dir() {
        h.set_size(0);
        h.set_cksum();
        builder.append_data(&mut h, name, std::io::empty())?;
    } else {
        h.set_size(md.len());
        h.set_cksum();
        let f = File::open(path)
            .with_context(|| format!("open {} for archiving", path.display()))?;
        builder.append_data(&mut h, name, f)?;
    }
    Ok(())
}

fn entry_type(md: &std::fs::Metadata) -> tar::EntryType {
    use std::os::unix::fs::FileTypeExt;
    let ft = md.file_type();
    if ft.is_symlink() {
        tar::EntryType::Symlink
    } else if ft.is_dir() {
        tar::EntryType::Directory
    } else if ft.is_fifo() {
        tar::EntryType::Fifo
    } else if ft.is_char_device() {
        tar::EntryType::Char
    } else if ft.is_block_device() {
        tar::EntryType::Block
    } else {
        tar::EntryType::Regular
    }
}

pub struct Extracted {
    pub pkginfo: String,
}

/// Extract a package archive into `root` (files land under `root/payload/...`
/// is NOT the case — payload entries are re-rooted directly into `root`).
pub fn extract(archive: &Path, root: &Path) -> Result<Extracted> {
    std::fs::create_dir_all(root)?;
    let f = File::open(archive).with_context(|| format!("open {}", archive.display()))?;
    let dec = zstd::Decoder::new(f)?;
    let mut ar = tar::Archive::new(dec);
    let mut pkginfo = String::new();

    for entry in ar.entries()? {
        let mut entry = entry?;
        let p = entry.path()?.to_path_buf();
        let name = p.to_string_lossy().to_string();

        if name == PKGINFO {
            entry.read_to_string(&mut pkginfo)?;
            continue;
        }
        let rel = match p.strip_prefix(PAYLOAD) {
            Ok(r) if !r.as_os_str().is_empty() => r.to_path_buf(),
            _ => continue, // skip payload root dir and unknown entries
        };
        extract_entry_into(&mut entry, &rel, root)?;
    }

    if pkginfo.is_empty() {
        anyhow::bail!("archive {} has no .PKGINFO", archive.display());
    }
    Ok(Extracted { pkginfo })
}

/// Unpack one payload entry (already re-rooted to `rel`) into `root`, with
/// the same safety checks `extract` applies. Also used by the closure
/// assembler, which indexes files as it streams them out of the archive.
pub fn extract_entry_into<R: Read>(
    entry: &mut tar::Entry<'_, R>,
    rel: &Path,
    root: &Path,
) -> Result<()> {
    // never trust paths coming out of an archive
    safe_relative(rel)?;
    if entry.header().entry_type().is_hard_link() {
        anyhow::bail!("hard-link entries are not supported: {}", rel.display());
    }
    reject_symlinked_parents(root, rel)?;
    let dest = root.join(rel);
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // a symlink planted by an earlier entry must not be written through
    if let Ok(md) = std::fs::symlink_metadata(&dest) {
        if md.file_type().is_symlink() && !entry.header().entry_type().is_symlink() {
            std::fs::remove_file(&dest)?;
        }
    }
    entry.unpack(&dest).with_context(|| format!("unpack {}", dest.display()))?;
    // tar-rs defers directory permission changes to the end of a full
    // `Archive::unpack` — which never happens with our per-entry pattern.
    // Apply directory modes explicitly so e.g. /tmp keeps its sticky bit.
    if entry.header().entry_type().is_dir() {
        if let Ok(mode) = entry.header().mode() {
            let _ = std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(mode));
        }
    }
    Ok(())
}

/// Reject absolute paths and `..` components in an archive-relative path.
fn safe_relative(rel: &Path) -> Result<()> {
    for c in rel.components() {
        match c {
            // control characters (newlines!) would corrupt the line-based file
            // lists in the package database
            Component::Normal(n) if n.to_string_lossy().chars().any(|c| c.is_control()) => {
                anyhow::bail!("control character in archive path: {:?}", rel)
            }
            Component::Normal(_) | Component::CurDir => {}
            _ => anyhow::bail!("unsafe path in archive: {}", rel.display()),
        }
    }
    Ok(())
}

/// Refuse to extract below a directory that is (already) a symlink inside
/// `root` — otherwise `a -> /etc` followed by `a/passwd` escapes the root.
fn reject_symlinked_parents(root: &Path, rel: &Path) -> Result<()> {
    let mut cur = root.to_path_buf();
    if let Some(parent) = rel.parent() {
        for c in parent.components() {
            cur.push(c.as_os_str());
            if let Ok(md) = std::fs::symlink_metadata(&cur) {
                if md.file_type().is_symlink() {
                    anyhow::bail!("archive writes through a symlink: {}", rel.display());
                }
            }
        }
    }
    Ok(())
}

/// Parse `key = value` lines from a .PKGINFO string.
pub fn parse_pkginfo(s: &str) -> Result<std::collections::HashMap<String, String>> {
    let mut map = std::collections::HashMap::new();
    for line in s.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (k, v) = match line.split_once('=') {
            Some(kv) => kv,
            None => continue,
        };
        map.insert(k.trim().to_string(), v.trim().to_string());
    }
    match map.get("pkgname") {
        None => anyhow::bail!(".PKGINFO missing pkgname"),
        Some(n) => crate::db::check_name(n)?,
    }
    if let Some(v) = map.get("pkgver") {
        if !crate::db::valid_version(v) {
            anyhow::bail!(".PKGINFO has an invalid pkgver `{}`", v.escape_debug());
        }
    }
    Ok(map)
}

/// Read the optional `.HOOKS` entry (empty hooks when absent).
pub fn read_hooks(archive: &Path) -> Result<Hooks> {
    let f = File::open(archive)?;
    let dec = zstd::Decoder::new(f)?;
    let mut ar = tar::Archive::new(dec);
    for entry in ar.entries()? {
        let mut entry = entry?;
        if entry.path()?.to_string_lossy() == HOOKS {
            let mut s = String::new();
            entry.read_to_string(&mut s)?;
            return toml::from_str(&s).context("parse .HOOKS");
        }
    }
    Ok(Hooks::default())
}

/// Convenience used by `nawa repo-create`: read only .PKGINFO without
/// extracting payload.
pub fn read_pkginfo(archive: &Path) -> Result<String> {
    let f = File::open(archive)?;
    let dec = zstd::Decoder::new(f)?;
    let mut ar = tar::Archive::new(dec);
    for entry in ar.entries()? {
        let mut entry = entry?;
        if entry.path()?.to_string_lossy() == PKGINFO {
            let mut s = String::new();
            entry.read_to_string(&mut s)?;
            return Ok(s);
        }
    }
    anyhow::bail!("no .PKGINFO in {}", archive.display())
}



#[cfg(test)]
mod tests {
    use super::*;

    /// Regression: symlinks in the staging tree must be archived AS symlinks.
    /// Previously `tar::Builder` defaulted to follow_symlinks(true), which
    /// dereferenced links (e.g. /etc/mtab -> /proc/mounts) and produced
    /// inconsistent tar streams that failed to extract.
    #[test]
    fn symlinks_are_not_dereferenced() {
        let base = std::env::temp_dir().join(format!("nawa-archive-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let staging = base.join("pkg");
        std::fs::create_dir_all(staging.join("etc")).unwrap();
        std::fs::create_dir_all(staging.join("tmp")).unwrap();
        std::fs::write(staging.join("etc/real.txt"), b"real content\n").unwrap();
        std::os::unix::fs::symlink("/proc/mounts", staging.join("etc/mtab")).unwrap();
        std::os::unix::fs::symlink("real.txt", staging.join("etc/relative-link")).unwrap();
        std::fs::set_permissions(
            staging.join("tmp"),
            std::fs::Permissions::from_mode(0o1777),
        )
        .unwrap();

        let archive = base.join("test.nawa.tar.zst");
        create(&staging, "pkgname = t", &archive).unwrap();

        let out = base.join("out");
        extract(&archive, &out).unwrap();

        let mtab = std::fs::symlink_metadata(out.join("etc/mtab")).unwrap();
        assert!(mtab.is_symlink(), "etc/mtab was dereferenced into a file");
        assert_eq!(
            std::fs::read_link(out.join("etc/mtab")).unwrap().to_str().unwrap(),
            "/proc/mounts"
        );
        assert_eq!(
            std::fs::read_link(out.join("etc/relative-link")).unwrap().to_str().unwrap(),
            "real.txt"
        );
        assert_eq!(std::fs::read(out.join("etc/real.txt")).unwrap(), b"real content\n");

        // directory modes (incl. sticky bit) survive the roundtrip
        let tmp_mode = std::fs::metadata(out.join("tmp")).unwrap().permissions().mode();
        assert_eq!(tmp_mode & 0o7777, 0o1777, "sticky bit lost on extracted dir");

        let _ = std::fs::remove_dir_all(&base);
    }

    /// Regression: `..` in payload paths and writes through planted symlinks
    /// must be rejected, nothing may land outside the extraction root.
    #[test]
    fn extract_rejects_traversal_and_symlink_escape() {
        let base = std::env::temp_dir().join(format!("nawa-archive-evil-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();

        // 1) `payload/../../escape.txt`
        let evil = base.join("traversal.nawa.tar.zst");
        {
            let f = File::create(&evil).unwrap();
            let enc = zstd::Encoder::new(f, 3).unwrap();
            let mut b = tar::Builder::new(enc);
            let mut h = tar::Header::new_gnu();
            let name = b"payload/../../escape.txt";
            h.as_old_mut().name[..name.len()].copy_from_slice(name);
            h.set_size(1);
            h.set_mode(0o644);
            h.set_cksum();
            b.append(&h, &b"x"[..]).unwrap();
            b.into_inner().unwrap().finish().unwrap();
        }
        let out = base.join("out1");
        assert!(extract(&evil, &out).is_err());
        assert!(!base.join("escape.txt").exists());

        // 2) symlink `payload/a -> <outside>` then `payload/a/pwn`
        let outside = base.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        let evil2 = base.join("symlink.nawa.tar.zst");
        {
            let f = File::create(&evil2).unwrap();
            let enc = zstd::Encoder::new(f, 3).unwrap();
            let mut b = tar::Builder::new(enc);
            let mut h = tar::Header::new_gnu();
            h.set_entry_type(tar::EntryType::Symlink);
            h.set_size(0);
            h.set_mode(0o777);
            h.set_link_name(&outside).unwrap();
            h.set_cksum();
            b.append_data(&mut h, "payload/a", std::io::empty()).unwrap();
            let mut h2 = tar::Header::new_gnu();
            h2.set_size(1);
            h2.set_mode(0o644);
            h2.set_cksum();
            b.append_data(&mut h2, "payload/a/pwn", &b"x"[..]).unwrap();
            b.into_inner().unwrap().finish().unwrap();
        }
        let out2 = base.join("out2");
        assert!(extract(&evil2, &out2).is_err());
        assert!(!outside.join("pwn").exists());

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn pkginfo_rejects_unsafe_name_and_version() {
        assert!(parse_pkginfo("pkgname = ../../etc\npkgver = 1\n").is_err());
        assert!(parse_pkginfo("pkgname = ok\npkgver = 1/2\n").is_err());
        assert!(parse_pkginfo("pkgname = ok\npkgver = 1.0\n").is_ok());
    }

    #[test]
    fn extract_rejects_newline_in_file_name() {
        let base = std::env::temp_dir().join(format!("nawa-archive-nl-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let a = base.join("nl.nawa.tar.zst");
        {
            let f = File::create(&a).unwrap();
            let enc = zstd::Encoder::new(f, 3).unwrap();
            let mut b = tar::Builder::new(enc);
            let mut h = tar::Header::new_gnu();
            h.set_size(0);
            h.set_mode(0o644);
            b.append_data(&mut h, "payload/foo\n/etc/passwd", std::io::empty()).unwrap();
            let mut h2 = tar::Header::new_gnu();
            h2.set_size(0);
            h2.set_mode(0o644);
            b.append_data(&mut h2, ".PKGINFO", std::io::empty()).unwrap();
            b.into_inner().unwrap().finish().unwrap();
        }
        assert!(extract(&a, &base.join("out")).is_err());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn hooks_roundtrip_multiline_with_quotes() {
        let base = std::env::temp_dir().join(format!("nawa-archive-hooks-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let st = base.join("pkg");
        std::fs::create_dir_all(&st).unwrap();
        std::fs::write(st.join("f"), b"x").unwrap();
        let hooks = Hooks {
            post_install: "echo \"hi\"\nexit 0".into(),
            pre_remove: "true".into(),
            post_remove: String::new(),
        };
        let a = base.join("t.nawa.tar.zst");
        create_with_hooks(&st, "pkgname = t", Some(&hooks), &a).unwrap();
        let got = read_hooks(&a).unwrap();
        assert_eq!(got.post_install, "echo \"hi\"\nexit 0");
        assert_eq!(got.pre_remove, "true");
        // archives without hooks read back empty
        let b = base.join("u.nawa.tar.zst");
        create(&st, "pkgname = u", &b).unwrap();
        assert!(read_hooks(&b).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Normalised archives are byte-identical no matter who built them or
    /// when: same content, different owners/mtimes ⇒ same sha256.
    #[test]
    fn normalised_archives_are_reproducible() {
        let base = std::env::temp_dir().join(format!("nawa-archive-repro-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let norm = Normalize { source_date_epoch: 1_700_000_000 };

        let mut hashes = Vec::new();
        for (i, (uid, age)) in [(1000u32, 86400u64), (12345, 3600)].iter().enumerate() {
            let st = base.join(format!("pkg{i}"));
            std::fs::create_dir_all(st.join("usr/bin")).unwrap();
            std::fs::write(st.join("usr/bin/prog"), b"#!/bin/sh\necho hi\n").unwrap();
            std::os::unix::fs::symlink("prog", st.join("usr/bin/prog-link")).unwrap();
            // fresh + future mtimes must both clamp to the epoch
            let t = std::time::SystemTime::UNIX_EPOCH
                + std::time::Duration::from_secs(norm.source_date_epoch + age);
            let f = std::fs::File::options().write(true).open(st.join("usr/bin/prog")).unwrap();
            f.set_modified(t).unwrap();
            drop(f);
            nix_chown(&st, *uid);

            let a = base.join(format!("out{i}.nawa.tar.zst"));
            create_normalized(&st, "pkgname = t\n", None, &a, &norm).unwrap();
            hashes.push(sha256_file(&a));
        }
        assert_eq!(hashes[0], hashes[1], "normalised archives must be identical");

        // entry header sanity: uid/gid 0, clamped mtime, empty uname/gname
        let f = File::open(base.join("out0.nawa.tar.zst")).unwrap();
        let mut ar = tar::Archive::new(zstd::Decoder::new(f).unwrap());
        let mut checked = 0;
        for e in ar.entries().unwrap() {
            let e = e.unwrap();
            assert_eq!(e.header().uid().unwrap(), 0);
            assert_eq!(e.header().gid().unwrap(), 0);
            if e.header().entry_type() == tar::EntryType::Regular {
                assert!(e.header().mtime().unwrap() <= norm.source_date_epoch);
                checked += 1;
            }
        }
        assert!(checked > 0);
        let _ = std::fs::remove_dir_all(&base);
    }

    fn nix_chown(dir: &Path, uid: u32) {
        use std::os::unix::ffi::OsStrExt;
        for e in walkdir::WalkDir::new(dir).into_iter().flatten() {
            let c = std::ffi::CString::new(e.path().as_os_str().as_bytes()).unwrap();
            unsafe {
                libc::chown(c.as_ptr(), uid, uid);
            }
        }
    }

    fn sha256_file(p: &Path) -> String {
        use sha2::Digest;
        let mut h = sha2::Sha256::new();
        h.update(std::fs::read(p).unwrap());
        hex::encode(h.finalize())
    }
}
