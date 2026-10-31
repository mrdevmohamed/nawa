//! Package slimming: the size work Alpine's `abuild` does after `package()`.
//!
//! Between "the recipe's `install` finished" and "write the archive", the
//! build engine runs three passes over the staged tree (`pkgdir`):
//!
//! 1. **cleanup** — files nobody needs at run time and that only cause file
//!    conflicts between packages: libtool `*.la`, `share/info/dir`,
//!    `charset.alias`, perl's `perllocal.pod` and `.packlist`.
//! 2. **split** — man pages / info / docs move into a sibling archive
//!    `<name>-doc`, gettext catalogs into `<name>-lang`. Both depend on the
//!    main package, so `nawa install bash-doc` just works. A split that
//!    would leave the main package empty is skipped (a documentation-only
//!    recipe stays whole).
//! 3. **strip** — debug symbols and symbol tables are removed from ELF
//!    executables and shared objects (`--strip-unneeded`) and debug info from
//!    static libraries, relocatable objects and kernel modules
//!    (`--strip-debug`). ELF files are recognised by their header, never by
//!    their name; `rustlib`, `/boot`, `/usr/lib/debug`, `/usr/src` and signed
//!    kernel modules are left alone.
//!
//! Everything is deterministic, so reproducible-build (`env` isolation and
//! up) archives stay reproducible. A recipe opts out per pass with a
//! `[slim]` table (`strip = false`, `cleanup = false`, `split = false`); a
//! build profile or `NAWA_SLIM=0` turns the whole thing off.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::BTreeSet;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::humanize::format_size;

/// Bumped whenever the slimming rules change. It is part of the build id, so
/// `--use-cache` never serves an archive made by older rules.
pub const VERSION: u32 = 1;

/// Suffixes of the sub-packages `split` can produce, in the order it makes
/// them (`<name>-doc`, `<name>-lang`).
pub const SUFFIXES: [&str; 2] = ["doc", "lang"];

fn yes() -> bool {
    true
}

/// The recipe's `[slim]` table: every pass is on unless the recipe says no.
#[derive(Debug, Clone, Deserialize)]
pub struct Slim {
    #[serde(default = "yes")]
    pub strip: bool,
    #[serde(default = "yes")]
    pub cleanup: bool,
    #[serde(default = "yes")]
    pub split: bool,
}

impl Default for Slim {
    fn default() -> Self {
        Slim { strip: true, cleanup: true, split: true }
    }
}

/// A sub-package staged in its own directory, ready to be archived.
#[derive(Debug)]
pub struct SubPackage {
    /// `doc` or `lang`
    pub suffix: &'static str,
    /// Staging directory (same layout as `pkgdir`).
    pub dir: PathBuf,
    /// Number of files, symlinks and special files moved into it.
    pub files: usize,
    /// Sum of the regular files' sizes, for `.PKGINFO`.
    pub size: u64,
}

/// What `run` did.
#[derive(Debug, Default)]
pub struct Report {
    pub stripped: usize,
    pub saved: u64,
    pub removed: usize,
    pub subpackages: Vec<SubPackage>,
}

impl Report {
    /// One line for the build log, `None` when nothing happened.
    pub fn summary(&self) -> Option<String> {
        let mut parts: Vec<String> = Vec::new();
        if self.stripped > 0 {
            parts.push(format!(
                "stripped {} file(s), -{}",
                self.stripped,
                format_size(self.saved)
            ));
        }
        if self.removed > 0 {
            parts.push(format!("removed {} junk file(s)", self.removed));
        }
        for sp in &self.subpackages {
            parts.push(format!(
                "split -{} ({} files, {})",
                sp.suffix,
                sp.files,
                format_size(sp.size)
            ));
        }
        if parts.is_empty() {
            None
        } else {
            Some(format!("nawa: slim: {}", parts.join(", ")))
        }
    }
}

/// File name of a sub-package archive, next to the main one:
/// `<name>-<suffix>-<version>-<rel>.nawa.tar.zst`.
pub fn subpackage_file(name: &str, suffix: &str, version: &str, rel: u32) -> String {
    format!("{name}-{suffix}-{version}-{rel}.nawa.tar.zst")
}

/// `.PKGINFO` of a sub-package, derived from the main package's: same
/// version, arch and build inputs; its own name and size; it depends on the
/// main package.
pub fn derive_pkginfo(main: &str, main_name: &str, sub_name: &str, size: u64) -> String {
    let mut out = String::new();
    for line in main.lines() {
        if line.starts_with("pkgname = ") {
            out.push_str(&format!("pkgname = {sub_name}\n"));
        } else if line.starts_with("rundeps = ") {
            out.push_str(&format!("rundeps = {main_name}\n"));
        } else if line.starts_with("size = ") {
            out.push_str(&format!("size = {size}\n"));
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// Run the enabled passes over `pkgdir`. Sub-packages are staged under
/// `splitdir/<suffix>`; `splitdir` is wiped first.
pub fn run(pkgdir: &Path, splitdir: &Path, policy: &Slim) -> Result<Report> {
    let mut rep = Report::default();
    if policy.cleanup {
        rep.removed = cleanup(pkgdir);
    }
    if policy.split {
        let _ = std::fs::remove_dir_all(splitdir);
        rep.subpackages = split(pkgdir, splitdir)?;
    }
    if policy.strip {
        strip(pkgdir, &mut rep);
    }
    Ok(rep)
}

// ---------------------------------------------------------------------------
// 1. cleanup
// ---------------------------------------------------------------------------

/// Remove files that are never needed at run time. Returns how many.
pub fn cleanup(pkgdir: &Path) -> usize {
    let mut n = 0;
    for e in walkdir::WalkDir::new(pkgdir).into_iter().filter_map(|e| e.ok()) {
        // symlinks are not `is_file()` here: a link is never junk
        if !e.file_type().is_file() {
            continue;
        }
        let rel = match e.path().strip_prefix(pkgdir) {
            Ok(r) => r,
            Err(_) => continue,
        };
        if is_junk(rel, e.path()) && std::fs::remove_file(e.path()).is_ok() {
            n += 1;
        }
    }
    n
}

fn is_junk(rel: &Path, abs: &Path) -> bool {
    let s = rel.to_string_lossy();
    if matches!(
        &*s,
        "usr/share/info/dir"
            | "usr/info/dir"
            | "usr/lib/charset.alias"
            | "usr/lib64/charset.alias"
            | "lib/charset.alias"
    ) {
        return true;
    }
    let name = rel.file_name().and_then(|n| n.to_str()).unwrap_or("");
    if name == "perllocal.pod" || name == ".packlist" {
        return true;
    }
    if name.ends_with(".la") {
        return is_libtool_archive(abs);
    }
    false
}

/// `*.la` is only removed when it really is a libtool archive.
fn is_libtool_archive(path: &Path) -> bool {
    let Ok(mut f) = File::open(path) else { return false };
    let mut head = [0u8; 1024];
    let n = read_head(&mut f, &mut head);
    String::from_utf8_lossy(&head[..n]).contains("libtool library file")
}

// ---------------------------------------------------------------------------
// 2. split
// ---------------------------------------------------------------------------

struct Kind {
    suffix: &'static str,
    /// directories (relative to the package root) whose content moves
    prefixes: &'static [&'static str],
    /// files under those directories that stay in the main package
    keep: &'static [&'static str],
}

const KINDS: [Kind; 2] = [
    Kind {
        suffix: "doc",
        prefixes: &[
            "usr/share/man",
            "usr/share/info",
            "usr/share/doc",
            "usr/share/gtk-doc",
            "usr/man",
            "usr/info",
            "usr/doc",
        ],
        keep: &[],
    },
    Kind {
        suffix: "lang",
        prefixes: &["usr/share/locale"],
        // glibc's and gettext's alias table is read by every program
        keep: &["usr/share/locale/locale.alias"],
    },
];

/// Move documentation and message catalogs out of `pkgdir` into
/// `splitdir/<suffix>`. A kind that is absent, or that would leave the main
/// package without any file, is skipped.
pub fn split(pkgdir: &Path, splitdir: &Path) -> Result<Vec<SubPackage>> {
    let real_root = pkgdir
        .canonicalize()
        .with_context(|| format!("resolve {}", pkgdir.display()))?;
    let mut out = Vec::new();

    for kind in &KINDS {
        let mut cands: Vec<PathBuf> = Vec::new();
        for prefix in kind.prefixes {
            let root = pkgdir.join(prefix);
            let md = match std::fs::symlink_metadata(&root) {
                Ok(m) => m,
                Err(_) => continue,
            };
            if !md.is_dir() {
                // a symlink or file standing where the directory would be
                cands.push(PathBuf::from(prefix));
                continue;
            }
            // never walk out of the package through a symlinked parent
            match root.canonicalize() {
                Ok(c) if c.starts_with(&real_root) => {}
                _ => continue,
            }
            for e in walkdir::WalkDir::new(&root).sort_by_file_name() {
                let e = e?;
                if e.file_type().is_dir() {
                    continue;
                }
                let rel = e.path().strip_prefix(pkgdir)?.to_path_buf();
                if kind.keep.iter().any(|k| rel == Path::new(k)) {
                    continue;
                }
                cands.push(rel);
            }
        }
        if cands.is_empty() {
            continue;
        }
        if cands.len() >= count_non_dirs(pkgdir) {
            println!(
                "nawa: slim: -{} would leave the package empty — kept in place",
                kind.suffix
            );
            continue;
        }

        let dest_root = splitdir.join(kind.suffix);
        std::fs::create_dir_all(&dest_root)
            .with_context(|| format!("create {}", dest_root.display()))?;
        let mut size = 0u64;
        let mut dir_modes: Vec<(PathBuf, u32)> = Vec::new();
        for rel in &cands {
            let src = pkgdir.join(rel);
            let dst = dest_root.join(rel);
            if let Ok(md) = std::fs::symlink_metadata(&src) {
                if md.is_file() {
                    size += md.len();
                }
            }
            make_parents(pkgdir, &dest_root, rel, &mut dir_modes)?;
            std::fs::rename(&src, &dst)
                .with_context(|| format!("move {} to {}", src.display(), dst.display()))?;
        }
        // directory modes last: a read-only parent would block its children
        for (d, mode) in dir_modes {
            let _ = std::fs::set_permissions(&d, std::fs::Permissions::from_mode(mode));
        }
        prune_emptied_dirs(pkgdir, &cands, kind.prefixes);
        out.push(SubPackage {
            suffix: kind.suffix,
            dir: dest_root,
            files: cands.len(),
            size,
        });
    }
    Ok(out)
}

fn count_non_dirs(root: &Path) -> usize {
    walkdir::WalkDir::new(root)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| !e.file_type().is_dir())
        .count()
}

/// Create the parent directories of `rel` under `dst_root`, remembering the
/// mode each one should end up with (copied from the source directory).
fn make_parents(
    src_root: &Path,
    dst_root: &Path,
    rel: &Path,
    dir_modes: &mut Vec<(PathBuf, u32)>,
) -> Result<()> {
    let Some(parent) = rel.parent() else { return Ok(()) };
    let mut cur = PathBuf::new();
    for comp in parent.components() {
        cur.push(comp);
        let d = dst_root.join(&cur);
        if d.is_dir() {
            continue;
        }
        std::fs::create_dir(&d).with_context(|| format!("create {}", d.display()))?;
        if let Ok(md) = std::fs::metadata(src_root.join(&cur)) {
            dir_modes.push((d, md.permissions().mode() & 0o7777));
        }
    }
    Ok(())
}

/// Remove the directories the move emptied — only those: a directory that
/// was already empty (a skeleton mount point) is left alone, and nothing
/// above the split prefixes (`usr/share`, ...) is ever touched.
fn prune_emptied_dirs(pkgdir: &Path, moved: &[PathBuf], prefixes: &[&str]) {
    let mut dirs: BTreeSet<PathBuf> = BTreeSet::new();
    for rel in moved {
        let mut p = rel.parent();
        while let Some(d) = p {
            if d.as_os_str().is_empty() || !prefixes.iter().any(|x| d.starts_with(*x)) {
                break;
            }
            dirs.insert(d.to_path_buf());
            p = d.parent();
        }
    }
    let mut ordered: Vec<PathBuf> = dirs.into_iter().collect();
    ordered.sort_by_key(|d| std::cmp::Reverse(d.components().count()));
    for d in ordered {
        // `remove_dir` only succeeds on an empty directory
        let _ = std::fs::remove_dir(pkgdir.join(d));
    }
}

// ---------------------------------------------------------------------------
// 3. strip
// ---------------------------------------------------------------------------

/// Trailer the kernel appends to a signed module; stripping would break it.
const MODULE_SIG_MARKER: &[u8] = b"~Module signature appended~\n";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    /// executable or shared object (ET_EXEC / ET_DYN)
    Linked,
    /// relocatable object: `.o`, `.ko` (ET_REL)
    Relocatable,
    /// `ar` archive named `*.a`
    StaticLib,
}

fn read_head(f: &mut File, buf: &mut [u8]) -> usize {
    let mut n = 0;
    while n < buf.len() {
        match f.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    n
}

/// Recognise strip candidates by their first bytes.
fn classify(path: &Path) -> Option<Class> {
    let mut f = File::open(path).ok()?;
    let mut head = [0u8; 20];
    let n = read_head(&mut f, &mut head);
    if n >= 18 && head[..4] == *b"\x7fELF" {
        let t = match head[5] {
            1 => u16::from_le_bytes([head[16], head[17]]),
            2 => u16::from_be_bytes([head[16], head[17]]),
            _ => return None,
        };
        return match t {
            1 => Some(Class::Relocatable),
            2 | 3 => Some(Class::Linked),
            _ => None,
        };
    }
    if n >= 8 && head[..8] == *b"!<arch>\n" && path.extension().is_some_and(|e| e == "a") {
        return Some(Class::StaticLib);
    }
    None
}

fn module_is_signed(path: &Path) -> bool {
    let Ok(mut f) = File::open(path) else { return false };
    if f.seek(SeekFrom::End(-(MODULE_SIG_MARKER.len() as i64))).is_err() {
        return false;
    }
    let mut tail = vec![0u8; MODULE_SIG_MARKER.len()];
    let n = read_head(&mut f, &mut tail);
    n == tail.len() && tail == MODULE_SIG_MARKER
}

/// Trees nawa never strips: kernel images, kept debug info, sources, and
/// Rust's sysroot (its `.rlib`s carry metadata `strip` must not touch).
fn skip_strip(rel: &Path) -> bool {
    rel.starts_with("boot")
        || rel.starts_with("usr/lib/debug")
        || rel.starts_with("usr/src")
        || rel.components().any(|c| c.as_os_str() == "rustlib")
}

enum Run {
    Done,
    Failed(String),
    Missing,
}

/// Strip ONE file out of place: `strip -o <tmp> <file>`, and only a clean run
/// replaces the original. `strip` can exit 0 yet write a broken file (binutils
/// 2.43 on rustc's `librustc_driver`/`libLLVM`: "section `.text` can't be
/// allocated in segment" on stderr, output segfaults on load), so ANY output on
/// stderr counts as failure and the untouched original stays in place.
fn strip_one(flag: &str, file: &Path) -> Run {
    let name = file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let tmp = file.with_file_name(format!(".{name}.nawa-strip"));
    let _ = std::fs::remove_file(&tmp);
    let out = Command::new("strip")
        .arg("-p") // keep mtimes
        .arg(flag)
        .arg("-o")
        .arg(&tmp)
        .arg(file)
        .stdin(Stdio::null())
        .output();
    let fail = |why: String| {
        let _ = std::fs::remove_file(&tmp);
        Run::Failed(why)
    };
    match out {
        Ok(o) if o.status.success() && o.stderr.iter().all(|b| b.is_ascii_whitespace()) => {
            let mode = match std::fs::symlink_metadata(file) {
                Ok(m) => m.permissions().mode() & 0o7777,
                Err(e) => return fail(e.to_string()),
            };
            if let Err(e) = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode)) {
                return fail(e.to_string());
            }
            match std::fs::rename(&tmp, file) {
                Ok(()) => Run::Done,
                Err(e) => fail(e.to_string()),
            }
        }
        Ok(o) => {
            let err = String::from_utf8_lossy(&o.stderr);
            let first = err.lines().find(|l| !l.trim().is_empty()).unwrap_or("strip failed");
            fail(format!("{first} (original kept)"))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let _ = std::fs::remove_file(&tmp);
            Run::Missing
        }
        Err(e) => fail(e.to_string()),
    }
}

fn total_size(files: &[PathBuf]) -> u64 {
    files
        .iter()
        .filter_map(|p| std::fs::symlink_metadata(p).ok())
        .map(|m| m.len())
        .sum()
}

/// Strip `files` with `flag`, file by file; a file that fails is reported and
/// left as it was, so one odd file never hides the rest. Returns
/// (files stripped, bytes saved), or `None` when `strip` is not installed.
fn strip_group(files: &[PathBuf], flag: &str) -> Option<(usize, u64)> {
    if files.is_empty() {
        return Some((0, 0));
    }
    let before = total_size(files);
    let mut ok = 0usize;
    for p in files {
        match strip_one(flag, p) {
            Run::Done => ok += 1,
            Run::Missing => return None,
            Run::Failed(why) => {
                eprintln!("nawa: slim: warning: not stripping {}: {why}", p.display())
            }
        }
    }
    Some((ok, before.saturating_sub(total_size(files))))
}

fn strip(pkgdir: &Path, rep: &mut Report) {
    let mut linked: Vec<PathBuf> = Vec::new();
    let mut debug_only: Vec<PathBuf> = Vec::new();
    for e in walkdir::WalkDir::new(pkgdir).into_iter().filter_map(|e| e.ok()) {
        if !e.file_type().is_file() {
            continue;
        }
        let rel = match e.path().strip_prefix(pkgdir) {
            Ok(r) => r,
            Err(_) => continue,
        };
        if skip_strip(rel) {
            continue;
        }
        match classify(e.path()) {
            Some(Class::Linked) => linked.push(e.path().to_path_buf()),
            Some(Class::Relocatable) => {
                let is_module = rel.extension().is_some_and(|x| x == "ko");
                if is_module && module_is_signed(e.path()) {
                    continue;
                }
                debug_only.push(e.path().to_path_buf());
            }
            Some(Class::StaticLib) => debug_only.push(e.path().to_path_buf()),
            None => {}
        }
    }
    let groups = [(&linked, "--strip-unneeded"), (&debug_only, "--strip-debug")];
    for (files, flag) in groups {
        match strip_group(files, flag) {
            Some((n, saved)) => {
                rep.stripped += n;
                rep.saved += saved;
            }
            None => {
                eprintln!("nawa: slim: note: `strip` not found — skipping symbol stripping");
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("nawa-slim-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn touch(root: &Path, rel: &str, body: &[u8]) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    #[test]
    fn classifies_by_header_not_by_name() {
        let d = tmp("classify");
        let mut hdr = vec![0u8; 64];
        hdr[..4].copy_from_slice(b"\x7fELF");
        hdr[4] = 2; // 64-bit
        hdr[5] = 1; // little endian
        hdr[6] = 1;
        hdr[16] = 3; // ET_DYN
        touch(&d, "shared", &hdr);
        assert_eq!(classify(&d.join("shared")), Some(Class::Linked));
        hdr[16] = 1; // ET_REL
        touch(&d, "object", &hdr);
        assert_eq!(classify(&d.join("object")), Some(Class::Relocatable));
        hdr[16] = 4; // ET_CORE
        touch(&d, "core", &hdr);
        assert_eq!(classify(&d.join("core")), None);
        touch(&d, "libx.a", b"!<arch>\nmembers");
        assert_eq!(classify(&d.join("libx.a")), Some(Class::StaticLib));
        touch(&d, "fake.a", b"just text, not an archive");
        assert_eq!(classify(&d.join("fake.a")), None);
        touch(&d, "script.sh", b"#!/bin/sh\n");
        assert_eq!(classify(&d.join("script.sh")), None);
    }

    #[test]
    fn signed_modules_are_recognised() {
        let d = tmp("sig");
        let mut body = vec![1u8; 100];
        body.extend_from_slice(MODULE_SIG_MARKER);
        touch(&d, "signed.ko", &body);
        touch(&d, "plain.ko", &[1u8; 100]);
        touch(&d, "tiny.ko", b"x");
        assert!(module_is_signed(&d.join("signed.ko")));
        assert!(!module_is_signed(&d.join("plain.ko")));
        assert!(!module_is_signed(&d.join("tiny.ko")));
    }

    #[test]
    fn strip_skips_kernel_debug_and_rust_trees() {
        assert!(skip_strip(Path::new("boot/vmlinuz")));
        assert!(skip_strip(Path::new("usr/lib/debug/x.debug")));
        assert!(skip_strip(Path::new("usr/lib/rustlib/x86_64/lib/libstd.rlib")));
        assert!(!skip_strip(Path::new("usr/bin/bash")));
        assert!(!skip_strip(Path::new("usr/lib/libz.so.1")));
    }

    #[test]
    fn cleanup_removes_junk_only() {
        let d = tmp("cleanup");
        touch(&d, "usr/lib/libz.la", b"# libz.la - a libtool library file\n");
        touch(&d, "usr/lib/other.la", b"not a libtool file\n");
        touch(&d, "usr/share/info/dir", b"x");
        touch(&d, "usr/share/info/bash.info", b"x");
        touch(&d, "usr/lib/charset.alias", b"x");
        touch(&d, "usr/lib/perl5/5.40/perllocal.pod", b"x");
        touch(&d, "usr/bin/tool", b"x");
        assert_eq!(cleanup(&d), 4);
        assert!(!d.join("usr/lib/libz.la").exists());
        assert!(d.join("usr/lib/other.la").exists());
        assert!(!d.join("usr/share/info/dir").exists());
        assert!(d.join("usr/share/info/bash.info").exists());
        assert!(!d.join("usr/lib/charset.alias").exists());
        assert!(d.join("usr/bin/tool").exists());
    }

    #[test]
    fn split_moves_docs_and_locales_and_keeps_the_rest() {
        let d = tmp("split");
        let s = tmp("split-out");
        touch(&d, "usr/bin/tool", b"bin");
        touch(&d, "usr/share/man/man1/tool.1", b"man");
        touch(&d, "usr/share/doc/tool/README", b"doc");
        touch(&d, "usr/share/locale/de/LC_MESSAGES/tool.mo", b"mo");
        touch(&d, "usr/share/locale/locale.alias", b"alias");
        std::fs::create_dir_all(d.join("usr/share/skeleton-dir")).unwrap();

        let subs = split(&d, &s).unwrap();
        let names: Vec<&str> = subs.iter().map(|x| x.suffix).collect();
        assert_eq!(names, ["doc", "lang"]);
        assert_eq!(subs[0].files, 2);
        assert_eq!(subs[1].files, 1);

        assert!(d.join("usr/bin/tool").exists());
        assert!(!d.join("usr/share/man").exists());
        assert!(!d.join("usr/share/doc").exists());
        assert!(!d.join("usr/share/locale/de").exists());
        // kept: the alias table, and a directory that was already empty
        assert!(d.join("usr/share/locale/locale.alias").exists());
        assert!(d.join("usr/share/skeleton-dir").is_dir());
        assert!(s.join("doc/usr/share/man/man1/tool.1").is_file());
        assert!(s.join("lang/usr/share/locale/de/LC_MESSAGES/tool.mo").is_file());
    }

    #[test]
    fn split_never_empties_a_package() {
        let d = tmp("keep");
        let s = tmp("keep-out");
        touch(&d, "usr/share/man/man7/pages.7", b"m");
        touch(&d, "usr/share/man/man1/other.1", b"m");
        let subs = split(&d, &s).unwrap();
        assert!(subs.is_empty());
        assert!(d.join("usr/share/man/man7/pages.7").is_file());
    }

    #[test]
    fn split_with_nothing_to_move_is_a_no_op() {
        let d = tmp("nothing");
        let s = tmp("nothing-out");
        touch(&d, "usr/bin/tool", b"bin");
        assert!(split(&d, &s).unwrap().is_empty());
    }

    #[test]
    fn subpackage_pkginfo_depends_on_the_main_package() {
        let main = "pkgname = bash\npkgver = 5.2\npkgrel = 1\narch = x86_64\n\
                    rundeps = glibc,ncurses\nsize = 900\nbuild_id = abc\n";
        let sub = derive_pkginfo(main, "bash", "bash-doc", 42);
        assert_eq!(
            sub,
            "pkgname = bash-doc\npkgver = 5.2\npkgrel = 1\narch = x86_64\n\
             rundeps = bash\nsize = 42\nbuild_id = abc\n"
        );
        assert_eq!(
            subpackage_file("bash", "doc", "5.2", 1),
            "bash-doc-5.2-1.nawa.tar.zst"
        );
    }

    #[test]
    fn recipe_slim_table_defaults_to_everything_on() {
        let s: Slim = toml::from_str("").unwrap();
        assert!(s.strip && s.cleanup && s.split);
        let s: Slim = toml::from_str("strip = false\n").unwrap();
        assert!(!s.strip && s.cleanup && s.split);
    }

    #[test]
    fn strip_failure_keeps_the_original() {
        // not an ELF: strip errors, the file must survive byte for byte and
        // no temp file may be left behind
        let d = tmp("stripfail");
        touch(&d, "bad.so", b"definitely not an object file");
        let f = d.join("bad.so");
        match strip_one("--strip-unneeded", &f) {
            Run::Failed(_) | Run::Missing => {}
            Run::Done => panic!("strip of a non-ELF must not report success"),
        }
        assert_eq!(std::fs::read(&f).unwrap(), b"definitely not an object file");
        assert!(!d.join(".bad.so.nawa-strip").exists());
    }
}
