//! Post-build checks ("lint") — docs/design/build-isolation.md §7.
//!
//! Runs on the staging dir (`%{pkgdir}`) before the archive is created:
//!
//! 1. **Run-dependency check**: `DT_NEEDED` of every ELF mapped through the
//!    build root's soname index and compared with `depends.run` — this is
//!    what makes `depends.run` trustworthy (Phase 6a: warn).
//! 2. **Leaked build paths**: any file containing `/build/src`, `/build/pkg`
//!    or the host build directory (rpath/runpath, `.la`, pkg-config files,
//!    scripts) is an error.
//! 3. **Layout**: files live only under `bin boot etc lib lib64 opt run sbin
//!    srv usr var`; the FHS mount points (`dev home media mnt proc root sys
//!    tmp`) may be staged as empty directories by the skeleton package but
//!    never with content (`root/.bashrc` is the one exception); bare
//!    directories in `/usr/local` are also skeleton, but no files or links
//!    may land there. World-writable files are errors (sticky dirs like
//!    /tmp are fine).
//!
//! The ELF reader is ~150 lines of pure Rust instead of a `readelf`
//! subprocess: no dependency on binutils being present, unit-testable, and
//! safe to run on untrusted-looking bytes.

use anyhow::{bail, Result};
use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;

/// Top-level trees a package may install files into.
pub const ALLOWED_TOP: &[&str] = &["bin", "boot", "etc", "lib", "lib64", "opt", "run", "sbin", "srv", "usr", "var"];

/// Top-level dirs a package may stage as bare mount points only — the FHS
/// skeleton owned by `base-files`. Content under them is an error:
/// `dev`, `proc`, `sys` and `tmp` are kernel-managed at runtime, and
/// `home`, `media`, `mnt` stay empty for the site. The one documented
/// exception is the root account's shell rc file (`root/.bashrc`), which
/// ships with the skeleton.
pub const SKELETON_TOP: &[&str] = &["dev", "home", "media", "mnt", "proc", "root", "sys", "tmp"];

/// Skeleton trees where not even a subdirectory may be staged — the kernel
/// owns everything below them at runtime.
const KERNEL_MANAGED: &[&str] = &["dev", "proc", "sys", "tmp"];

/// Everything the checks found. Errors abort the build; warnings do not.
#[derive(Debug, Default)]
pub struct LintReport {
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
    pub elf_count: usize,
}

impl LintReport {
    pub fn ok(&self) -> bool {
        self.errors.is_empty()
    }
}

/// Context for [`run`].
pub struct LintCtx<'a> {
    /// The staging dir (`%{pkgdir}`).
    pub pkgdir: &'a Path,
    /// `depends.run` of the recipe under test.
    pub rundeps: &'a [String],
    /// soname → owning package (from the closure index or the installed db).
    pub sonames: &'a BTreeMap<String, String>,
    /// Absolute host build dir — leaking it into a file is an error even
    /// at the `env` level (where `/build/...` does not exist).
    pub host_builddir: Option<&'a str>,
}

pub fn run(ctx: &LintCtx) -> LintReport {
    let mut rep = LintReport::default();
    for entry in walkdir::WalkDir::new(ctx.pkgdir).sort_by_file_name() {
        let Ok(e) = entry else { continue };
        let rel = match e.path().strip_prefix(ctx.pkgdir) {
            Ok(r) if !r.as_os_str().is_empty() => r.to_path_buf(),
            _ => continue,
        };
        let rel_s = rel.to_string_lossy().into_owned();
        let md = match std::fs::symlink_metadata(e.path()) {
            Ok(m) => m,
            Err(_) => continue,
        };

        // ---- layout ----------------------------------------------------
        check_layout(&rel_s, &md, &mut rep);

        // ---- content scans (regular files only) -------------------------
        if !md.is_file() {
            continue;
        }
        let Ok(data) = read_limited(e.path()) else { continue };
        if data.starts_with(b"\x7fELF") {
            rep.elf_count += 1;
            match parse_elf(&data) {
                Ok(Some(info)) => {
                    check_elf(&rel_s, &info, ctx, &mut rep);
                }
                Ok(None) => {}
                Err(err) => rep.warnings.push(format!(
                    "{rel_s}: not a parseable ELF ({err}) — skipped"
                )),
            }
        } else if looks_texty(&data) {
            check_text_leaks(&rel_s, &data, ctx, &mut rep);
        }
    }
    rep
}

/// Layout rules (§7.3): files belong under `ALLOWED_TOP`; the FHS mount
/// points (`SKELETON_TOP`) are staged as directories only — any content under
/// them (`/tmp` leftovers, `/dev` rules, `/home` files, …) is an error, with
/// `root/.<rcfile>` as the skeleton's one exception. `/usr/local` may exist
/// as empty directories (the skeleton) but never receive files or links.
/// World-writable modes are errors unless the entry is a sticky directory
/// (/tmp, /var/tmp, /dev/shm).
fn check_layout(rel: &str, md: &std::fs::Metadata, rep: &mut LintReport) {
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
fn root_dot_rc(rel: &str) -> bool {
    let mut parts = rel.split('/');
    let top = parts.next() == Some("root");
    match (parts.next(), parts.next()) {
        (Some(second), None) => top && second.starts_with('.'),
        _ => false,
    }
}

fn check_elf(rel: &str, info: &ElfInfo, ctx: &LintCtx, rep: &mut LintReport) {
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

fn check_text_leaks(rel: &str, data: &[u8], ctx: &LintCtx, rep: &mut LintReport) {
    let text = String::from_utf8_lossy(data);
    let needles = ["/build/src", "/build/pkg"];
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

/// Read at most 4 MiB — enough for any config/script/library text the
/// checks need, without loading giant payloads into memory.
fn read_limited(path: &Path) -> std::io::Result<Vec<u8>> {
    let mut f = std::fs::File::open(path)?;
    let mut buf = Vec::new();
    let mut cap = 4 * 1024 * 1024;
    let mut chunk = vec![0u8; 64 * 1024];
    loop {
        let n = f.read(&mut chunk)?;
        if n == 0 || cap == 0 {
            break;
        }
        let take = n.min(cap);
        buf.extend_from_slice(&chunk[..take]);
        cap -= take;
    }
    Ok(buf)
}

fn looks_texty(data: &[u8]) -> bool {
    // a NUL in the first 512 bytes almost always means binary data; ELF is
    // already handled above
    !data.iter().take(512).any(|&b| b == 0)
}

// ---------------------------------------------------------------------------
// Minimal ELF reader (DT_NEEDED / DT_RUNPATH / DT_RPATH / DT_SONAME)
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone)]
pub struct ElfInfo {
    pub needed: Vec<String>,
    pub rpath: Option<String>,
    pub runpath: Option<String>,
    pub soname: Option<String>,
}

const DT_NULL: u64 = 0;
const DT_NEEDED: u64 = 1;
const DT_STRTAB: u64 = 5;
const DT_STRSZ: u64 = 10;
const DT_SONAME: u64 = 14;
const DT_RPATH: u64 = 15;
const DT_RUNPATH: u64 = 29;
const PT_LOAD: u32 = 1;
const PT_DYNAMIC: u32 = 2;

struct Cursor<'a> {
    data: &'a [u8],
    le: bool,
}

impl<'a> Cursor<'a> {
    fn u16(&self, off: usize) -> u64 {
        let b = self.get(off, 2);
        if self.le {
            u16::from_le_bytes([b[0], b[1]]) as u64
        } else {
            u16::from_be_bytes([b[0], b[1]]) as u64
        }
    }
    fn u32(&self, off: usize) -> u64 {
        let b = self.get(off, 4);
        if self.le {
            u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as u64
        } else {
            u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as u64
        }
    }
    fn u64(&self, off: usize) -> u64 {
        let b = self.get(off, 8);
        if self.le {
            u64::from_le_bytes(b.try_into().unwrap())
        } else {
            u64::from_be_bytes(b.try_into().unwrap())
        }
    }
    fn get(&self, off: usize, len: usize) -> &'a [u8] {
        &self.data[off..off + len]
    }
}

/// Parse an ELF file's dynamic section. `Ok(None)` for non-dynamic ELFs
/// (relocatable objects, core files). Endianness and class are taken from
/// the file; both ELF32 and ELF64 are supported.
pub fn parse_elf(data: &[u8]) -> Result<Option<ElfInfo>> {
    if data.len() < 16 || &data[..4] != b"\x7fELF" {
        bail!("not an ELF");
    }
    let class = data[4];
    let le = match data[5] {
        1 => true,
        2 => false,
        other => bail!("unknown ELF data encoding {other}"),
    };
    let c = Cursor { data, le };
    let (is64, _) = match class {
        1 => (false, ()),
        2 => (true, ()),
        other => bail!("unknown ELF class {other}"),
    };

    let (e_type, phoff, phentsize, phnum) = if is64 {
        (c.u16(16), c.u64(32) as usize, c.u16(54) as usize, c.u16(56) as usize)
    } else {
        (c.u16(16), c.u32(28) as usize, c.u16(42) as usize, c.u16(44) as usize)
    };
    // 1 = relocatable, 4 = core — no dynamic section to read
    if e_type == 1 || e_type == 4 {
        return Ok(None);
    }

    // collect PT_LOAD (vaddr→offset) and find PT_DYNAMIC
    let mut loads: Vec<(u64, u64, u64)> = Vec::new(); // (vaddr, filesz, offset)
    let mut dyn_off: Option<usize> = None;
    let mut dyn_len: usize = 0;
    for i in 0..phnum {
        let p = phoff + i * phentsize;
        if p + phentsize > data.len() {
            bail!("program header {i} out of bounds");
        }
        let p_type = c.u32(p) as u32;
        if is64 {
            let offset = c.u64(p + 8);
            let vaddr = c.u64(p + 16);
            let filesz = c.u64(p + 32);
            if p_type == PT_LOAD {
                loads.push((vaddr, filesz, offset));
            } else if p_type == PT_DYNAMIC {
                dyn_off = Some(offset as usize);
                dyn_len = filesz as usize;
            }
        } else {
            let offset = c.u32(p + 4);
            let vaddr = c.u32(p + 8);
            let filesz = c.u32(p + 16);
            if p_type == PT_LOAD {
                loads.push((vaddr, filesz, offset));
            } else if p_type == PT_DYNAMIC {
                dyn_off = Some(offset as usize);
                dyn_len = filesz as usize;
            }
        }
    }
    let Some(doff) = dyn_off else { return Ok(None) };
    if doff + dyn_len > data.len() {
        bail!("dynamic section out of bounds");
    }

    let vaddr_to_off = |v: u64| -> Option<usize> {
        for (vaddr, filesz, offset) in &loads {
            if v >= *vaddr && v < vaddr + filesz {
                return Some((offset + (v - vaddr)) as usize);
            }
        }
        None
    };

    let mut info = ElfInfo::default();
    let entry_size = if is64 { 16 } else { 8 };
    // first pass: collect the raw entries and locate the string table —
    // entry order in the section is not guaranteed (our own fixtures put
    // DT_STRTAB last, some strip/patch tools reorder entries)
    let mut raw: Vec<(u64, u64)> = Vec::new();
    let mut off = doff;
    let end = doff + dyn_len;
    while off + entry_size <= end {
        let (tag, val) = if is64 {
            (c.u64(off), c.u64(off + 8))
        } else {
            (c.u32(off), c.u32(off + 4))
        };
        if tag == DT_NULL {
            break;
        }
        raw.push((tag, val));
        off += entry_size;
    }
    // string table address and size are only final after the whole scan
    let mut strtab: Option<usize> = None;
    let mut strsz: usize = 0;
    for (tag, val) in &raw {
        match *tag {
            DT_STRTAB => strtab = vaddr_to_off(*val),
            DT_STRSZ => strsz = *val as usize,
            _ => {}
        }
    }
    // second pass: the string-dependent entries
    for (tag, val) in &raw {
        match *tag {
            DT_NEEDED => {
                if let Some(s) = dyn_string(&c, strtab, strsz, *val) {
                    info.needed.push(s);
                }
            }
            DT_SONAME => info.soname = dyn_string(&c, strtab, strsz, *val),
            DT_RPATH => info.rpath = dyn_string(&c, strtab, strsz, *val),
            DT_RUNPATH => info.runpath = dyn_string(&c, strtab, strsz, *val),
            _ => {}
        }
    }
    Ok(Some(info))
}

/// Read a NUL-terminated string from the dynamic string table.
fn dyn_string(c: &Cursor<'_>, strtab: Option<usize>, strsz: usize, off: u64) -> Option<String> {
    let base = strtab?;
    let start = base.checked_add(off as usize)?;
    if strsz > 0 && off as usize >= strsz {
        return None;
    }
    let bytes = c.data.get(start..)?;
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    let s = &bytes[..end];
    if s.is_empty() {
        return None;
    }
    Some(String::from_utf8_lossy(s).into_owned())
}

// ---------------------------------------------------------------------------
// build id (§7)
// ---------------------------------------------------------------------------

/// `build_id = sha256(recipe.toml bytes ‖ source sha256 ‖ sorted closure
/// archive sha256s ‖ profile flags)` — "is this archive up to date?"
/// becomes a hash comparison.
pub fn build_id(
    recipe_bytes: &[u8],
    source_sha256: &str,
    closure_archives: &[std::path::PathBuf],
    profile_flags: &str,
) -> String {
    use sha2::Digest;
    let mut h = sha2::Sha256::new();
    h.update(recipe_bytes);
    h.update(b"\0");
    h.update(source_sha256.as_bytes());
    h.update(b"\0");
    // order-insensitive by construction: the sorted archive set is the input
    let mut archives: Vec<String> = closure_archives
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    archives.sort();
    let closure_archives: Vec<std::path::PathBuf> =
        archives.into_iter().map(std::path::PathBuf::from).collect();
    for a in &closure_archives {
        h.update(a.to_string_lossy().as_bytes());
        // the archive *name* pins the version; its *content* is pinned by
        // the signature chain — hashing content here would make every
        // rebuild of a dependency invalidate every dependent
        h.update(b"\0");
    }
    h.update(b"\0");
    h.update(profile_flags.as_bytes());
    hex::encode(h.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn unique_temp_dir(prefix: &str) -> PathBuf {
        static NEXT_ID: AtomicU64 = AtomicU64::new(0);
        std::env::temp_dir().join(format!(
            "{prefix}-{}-{}",
            std::process::id(),
            NEXT_ID.fetch_add(1, Ordering::Relaxed)
        ))
    }

    #[test]
    fn build_id_is_stable_and_input_sensitive() {
        let a1 = PathBuf::from("/x/base-devel-1-1.nawa.tar.zst");
        let a2 = PathBuf::from("/x/gcc-14-1.nawa.tar.zst");
        let b1 = build_id(b"recipe", "abc", &[a1.clone(), a2.clone()], "-O2");
        let b2 = build_id(b"recipe", "abc", &[a2.clone(), a1.clone()], "-O2");
        assert_eq!(b1, b2, "archive order must not matter");
        assert_ne!(b1, build_id(b"recipe2", "abc", &[a1.clone(), a2.clone()], "-O2"));
        assert_ne!(b1, build_id(b"recipe", "abd", std::slice::from_ref(&a2), "-O2"));
        assert_ne!(b1, build_id(b"recipe", "abc", &[a1, a2], "-O3"));
    }

    #[test]
    fn skeleton_staging_is_legal_but_content_is_not() {
        // base-files owns the FHS mount points: empty dirs + the root rc file
        let mut rep = LintReport::default();
        for (rel, md) in [
            ("dev", fake_dir_md(0o755)),
            ("home", fake_dir_md(0o755)),
            ("media", fake_dir_md(0o755)),
            ("mnt", fake_dir_md(0o755)),
            ("proc", fake_dir_md(0o755)),
            ("root", fake_dir_md(0o750)),
            ("sys", fake_dir_md(0o755)),
            ("tmp", fake_dir_md(0o1777)), // sticky passes the world-writable check
            ("root/.bashrc", fake_md(0o644)),
            ("root/.profile", fake_md(0o644)),
            ("usr/local/bin", fake_dir_md(0o755)), // the /usr/local skeleton
        ] {
            check_layout(rel, &md, &mut rep);
        }
        assert!(rep.ok(), "FHS skeleton must pass: {:?}", rep.errors);

        // ... but ANY content under a mount point is an error
        let mut rep = LintReport::default();
        check_layout("tmp/stray", &fake_md(0o755), &mut rep);        // leftover file
        check_layout("dev/rule", &fake_md(0o644), &mut rep);         // udev rule
        check_layout("proc/self", &fake_dir_md(0o555), &mut rep);    // even a dir
        check_layout("home/user", &fake_md(0o644), &mut rep);        // site user
        check_layout("mnt/out", &fake_dir_md(0o755), &mut rep);      // nested mount
        check_layout("root/notes", &fake_md(0o644), &mut rep);       // non-dotfile
        check_layout("root/.ssh", &fake_dir_md(0o700), &mut rep);    // dot dir
        check_layout("usr/local/bin/tool", &fake_md(0o755), &mut rep); // content
        for e in &rep.errors {
            assert!(
                e.contains("kernel-managed")
                    || e.contains("reserved for the site/user")
                    || e.contains("/usr/local"),
                "unexpected: {e}"
            );
        }
        assert_eq!(rep.errors.len(), 8, "all eight are errors: {:?}", rep.errors);

        // an unknown top-level tree still fails with both lists in the msg
        let mut rep = LintReport::default();
        check_layout("opt2/x", &fake_md(0o644), &mut rep);
        assert!(!rep.ok());
        assert!(rep.errors[0].contains("outside the allowed trees"));
        assert!(rep.errors[0].contains("proc"), "mentions the skeleton dirs");
    }

    #[test]
    fn layout_checks() {
        let mut rep = LintReport::default();
        check_layout("usr/bin/tool", &fake_md(0o755), &mut rep);
        assert!(rep.ok());
        check_layout("usr/local/bin/tool", &fake_md(0o755), &mut rep);
        assert!(!rep.ok(), "no /usr/local");
        let mut rep = LintReport::default();
        check_layout("tmp/stray", &fake_md(0o755), &mut rep);
        assert!(!rep.ok(), "no stray /tmp in the payload");
        let mut rep = LintReport::default();
        check_layout("etc/shadow", &fake_md(0o666), &mut rep);
        assert!(!rep.ok(), "world-writable files are errors (0666)");
        let mut rep = LintReport::default();
        check_layout("etc/shadow", &fake_md(0o777), &mut rep);
        assert!(!rep.ok(), "world-writable files are errors (0777)");
        let mut rep = LintReport::default();
        check_layout("usr/bin/tool", &fake_md(0o755), &mut rep);
        check_layout("etc/conf", &fake_md(0o640), &mut rep);
        assert!(rep.ok(), "sane modes pass");
    }

    fn fake_md(mode: u32) -> std::fs::Metadata {
        use std::os::unix::fs::PermissionsExt;
        let dir = unique_temp_dir("nawa-lint-md");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("x"), b"x").unwrap();
        std::fs::set_permissions(dir.join("x"), std::fs::Permissions::from_mode(mode)).unwrap();
        let m = std::fs::metadata(dir.join("x")).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        m
    }

    fn fake_dir_md(mode: u32) -> std::fs::Metadata {
        use std::os::unix::fs::PermissionsExt;
        let dir = unique_temp_dir("nawa-lint-dmd");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(mode)).unwrap();
        let m = std::fs::metadata(&dir).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        m
    }

    // ---- ELF fixtures --------------------------------------------------

    /// Hand-built ELF64 LE shared object with the given dynamic entries.
    /// Layout: ehdr(64) | phdr[PT_LOAD, PT_DYNAMIC] | dyn | strtab, with the
    /// single PT_LOAD mapping file offset == vaddr so vaddr_to_off is the
    /// identity — exactly enough for the parser to find DT_NEEDED etc.
    fn elf64_fixture(needed: &[&str], runpath: Option<&str>, soname: Option<&str>) -> Vec<u8> {
        const EH: usize = 64;
        const PH: usize = 56;
        // 1) string table first — its layout is independent of everything else
        let mut strtab: Vec<u8> = vec![0];
        let mut str = |s: &str| -> u64 {
            let off = strtab.len() as u64;
            strtab.extend_from_slice(s.as_bytes());
            strtab.push(0);
            off
        };
        let needed_offs: Vec<u64> = needed.iter().map(|n| str(n)).collect();
        let runpath_off = runpath.map(&mut str);
        let soname_off = soname.map(str);

        // 2) dynamic section: NEEDED* [RUNPATH] [SONAME] STRTAB STRSZ NULL
        let n_dyn = needed.len() + runpath.is_some() as usize + soname.is_some() as usize + 3;
        let dyn_len = n_dyn * 16;
        let strtab_off = (EH + 2 * PH + dyn_len) as u64;
        let file_len = strtab_off as usize + strtab.len();

        let mut dyn_bytes: Vec<u8> = Vec::with_capacity(dyn_len);
        let mut push = |tag: u64, val: u64| {
            dyn_bytes.extend_from_slice(&tag.to_le_bytes());
            dyn_bytes.extend_from_slice(&val.to_le_bytes());
        };
        for off in needed_offs {
            push(DT_NEEDED, off);
        }
        if let Some(off) = runpath_off {
            push(DT_RUNPATH, off);
        }
        if let Some(off) = soname_off {
            push(DT_SONAME, off);
        }
        push(DT_STRTAB, strtab_off); // identity: vaddr == file offset
        push(DT_STRSZ, strtab.len() as u64);
        push(DT_NULL, 0);
        debug_assert_eq!(dyn_bytes.len(), dyn_len);

        // 3) ELF header
        let mut v = vec![0u8; EH];
        v[..4].copy_from_slice(b"\x7fELF");
        v[4] = 2; // 64-bit
        v[5] = 1; // little endian
        v[6] = 1; // version
        v[16..18].copy_from_slice(&3u16.to_le_bytes()); // ET_DYN
        v[18..20].copy_from_slice(&62u16.to_le_bytes()); // EM_X86_64
        v[32..40].copy_from_slice(&(EH as u64).to_le_bytes()); // e_phoff
        v[54..56].copy_from_slice(&(PH as u16).to_le_bytes()); // e_phentsize
        v[56..58].copy_from_slice(&2u16.to_le_bytes()); // e_phnum

        // 4) program headers
        let ph = |ptype: u32, offset: u64, vaddr: u64, filesz: u64, out: &mut Vec<u8>| {
            out.extend_from_slice(&ptype.to_le_bytes());
            out.extend_from_slice(&4u32.to_le_bytes()); // PF_R
            out.extend_from_slice(&offset.to_le_bytes());
            out.extend_from_slice(&vaddr.to_le_bytes());
            out.extend_from_slice(&vaddr.to_le_bytes()); // paddr
            out.extend_from_slice(&filesz.to_le_bytes());
            out.extend_from_slice(&filesz.to_le_bytes()); // memsz
            out.extend_from_slice(&0x1000u64.to_le_bytes()); // align
        };
        ph(PT_LOAD, 0, 0, file_len as u64, &mut v); // identity mapping
        ph(PT_DYNAMIC, (EH + 2 * PH) as u64, (EH + 2 * PH) as u64, dyn_len as u64, &mut v);

        v.extend_from_slice(&dyn_bytes);
        v.extend_from_slice(&strtab);
        v
    }

    #[test]
    fn elf64_fixture_parses() {
        let v = elf64_fixture(&["libz.so.1", "libc.so.6"], Some("/build/src/lib"), None);
        let info = parse_elf(&v).unwrap().expect("dynamic ELF");
        assert_eq!(info.needed, vec!["libz.so.1", "libc.so.6"]);
        assert_eq!(info.runpath.as_deref(), Some("/build/src/lib"));
    }

    #[test]
    fn elf64_fixture_without_runpath() {
        let v = elf64_fixture(&["libc.so.6"], None, Some("libprobe.so.1"));
        let info = parse_elf(&v).unwrap().expect("dynamic ELF");
        assert_eq!(info.needed, vec!["libc.so.6"]);
        assert!(info.runpath.is_none());
        assert_eq!(info.soname.as_deref(), Some("libprobe.so.1"));
    }

    #[test]
    fn non_elf_and_truncated_are_rejected() {
        assert!(parse_elf(b"#!/bin/sh\necho hi\n").is_err());
        assert!(parse_elf(b"\x7fELF").is_err());
    }

    #[test]
    fn lint_finds_leaks_and_missing_rundeps() {
        let base = std::env::temp_dir().join(format!("nawa-lint-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let pkg = base.join("pkg");
        std::fs::create_dir_all(pkg.join("usr/bin")).unwrap();
        std::fs::create_dir_all(pkg.join("usr/lib/pkgconfig")).unwrap();
        std::fs::write(pkg.join("usr/bin/tool"), b"\x7fELF-not-really").unwrap();
        std::fs::write(
            pkg.join("usr/lib/pkgconfig/zlib.pc"),
            b"prefix=/usr\nlibdir=/build/src/zlib\n",
        )
        .unwrap();

        let mut sonames = BTreeMap::new();
        sonames.insert("libz.so.1".to_string(), "zlib".to_string());
        let ctx = LintCtx {
            pkgdir: &pkg,
            rundeps: &[], // missing zlib!
            sonames: &sonames,
            host_builddir: Some("/home/z/big-build"),
        };
        // a real (fixture) ELF linking libz goes through the parser path
        let elf = elf64_fixture(&["libz.so.1"], None, None);
        std::fs::write(pkg.join("usr/bin/tool"), &elf).unwrap();

        let rep = run(&ctx);
        assert!(
            rep.warnings.iter().any(|w| w.contains("libz.so.1") && w.contains("zlib")),
            "missing rundep warning: {:?}",
            rep.warnings
        );
        assert!(
            rep.errors.iter().any(|e| e.contains("/build/src")),
            "leaked build path: {:?}",
            rep.errors
        );
        let _ = std::fs::remove_dir_all(&base);
    }
}
