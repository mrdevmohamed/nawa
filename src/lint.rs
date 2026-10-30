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
//! The ELF reader is the `goblin` crate (elf32 + elf64, dynamic sections)
//! instead of a `readelf` subprocess or a hand-rolled byte walker: no
//! dependency on binutils being present, unit-testable, bounds-checked
//! against hostile inputs (malformed ELFs produce ordinary errors) and safe
//! to run on untrusted-looking bytes.

use anyhow::{bail, Context, Result};
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
// ELF reader (goblin): DT_NEEDED / DT_RUNPATH / DT_RPATH / DT_SONAME
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone)]
pub struct ElfInfo {
    pub needed: Vec<String>,
    pub rpath: Option<String>,
    pub runpath: Option<String>,
    /// DT_SONAME, when the file carries one. The run-dep policy itself keys
    /// on DT_NEEDED only; the field is kept (like the reader it replaced)
    /// so the info stays complete for consumers.
    #[allow(dead_code)]
    pub soname: Option<String>,
}

/// Parse an ELF file's dynamic section. `Ok(None)` for non-dynamic ELFs
/// (relocatable objects, core files). Endianness and class are taken from
/// the file; both ELF32 and ELF64 are supported.
///
/// goblin is bounds-checked throughout: truncated headers, oversized field
/// values and dangling string offsets are reported as errors instead of
/// panics — this function must stay safe to call on adversarial bytes.
pub fn parse_elf(data: &[u8]) -> Result<Option<ElfInfo>> {
    if data.len() < 16 || &data[..4] != b"\x7fELF" {
        bail!("not an ELF");
    }
    let elf = goblin::elf::Elf::parse(data).context("malformed ELF file")?;
    // relocatable objects (ET_REL) and kernel core dumps (ET_CORE) never
    // carry an ELF interpreter / dynamic linking info worth auditing;
    // matching the previous reader, they parse to nothing
    if matches!(elf.header.e_type, goblin::elf::header::ET_REL | goblin::elf::header::ET_CORE) {
        return Ok(None);
    }
    let Some(dynamic) = &elf.dynamic else {
        // dynamic executables and shared objects always have PT_DYNAMIC;
        // a stripped/odd file without one has no dependencies to audit
        return Ok(None);
    };
    let info = ElfInfo {
        needed: dynamic
            .get_libraries(&elf.dynstrtab)
            .into_iter()
            .map(str::to_string)
            .collect(),
        // goblin keeps every DT_RPATH/DT_RUNPATH occurrence; the previous
        // reader kept the last non-empty one — same drop-in behaviour
        rpath: last_non_empty(&elf.rpaths),
        runpath: last_non_empty(&elf.runpaths),
        soname: elf
            .soname
            .filter(|s| !s.is_empty())
            .map(str::to_string),
    };
    Ok(Some(info))
}

fn last_non_empty(paths: &[&str]) -> Option<String> {
    paths.iter().rev().find(|p| !p.is_empty()).map(|p| (*p).to_string())
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

    /// Byte writer for fixture headers, honouring the ELF data encoding.
    struct Endian {
        le: bool,
    }
    impl Endian {
        fn u16(&self, v: u16) -> Vec<u8> {
            if self.le { v.to_le_bytes().to_vec() } else { v.to_be_bytes().to_vec() }
        }
        fn u32(&self, v: u32) -> Vec<u8> {
            if self.le { v.to_le_bytes().to_vec() } else { v.to_be_bytes().to_vec() }
        }
        fn u64(&self, v: u64) -> Vec<u8> {
            if self.le { v.to_le_bytes().to_vec() } else { v.to_be_bytes().to_vec() }
        }
    }

    /// Hand-built ELF (32- or 64-bit, LE or BE) shared object with the given
    /// dynamic entries. Layout: ehdr | phdr[PT_LOAD, PT_DYNAMIC] | dyn |
    /// strtab, with the single PT_LOAD mapping file offset == vaddr so the
    /// vaddr→offset translation is the identity — exactly enough for any ELF
    /// reader to find DT_NEEDED etc.
    fn elf_fixture(
        class: u8,  // 1 = ELF32, 2 = ELF64
        data: u8,   // 1 = little endian, 2 = big endian
        needed: &[&str],
        runpath: Option<&str>,
        rpath: Option<&str>,
        soname: Option<&str>,
    ) -> Vec<u8> {
        const DT_NEEDED: u64 = 1;
        const DT_STRTAB: u64 = 5;
        const DT_STRSZ: u64 = 10;
        const DT_SONAME: u64 = 14;
        const DT_RPATH: u64 = 15;
        const DT_RUNPATH: u64 = 29;
        const DT_NULL: u64 = 0;
        const PT_LOAD: u32 = 1;
        const PT_DYNAMIC: u32 = 2;

        let is64 = class == 2;
        let (eh_len, ph_len, dyn_entry_len) = if is64 { (64, 56, 16) } else { (52, 32, 8) };
        let e = Endian { le: data == 1 };

        // 1) string table — its layout is independent of everything else
        let mut strtab: Vec<u8> = vec![0];
        let mut add = |s: &str| -> u64 {
            let off = strtab.len() as u64;
            strtab.extend_from_slice(s.as_bytes());
            strtab.push(0);
            off
        };
        let needed_offs: Vec<u64> = needed.iter().map(|n| add(n)).collect();
        let runpath_off = runpath.map(&mut add);
        let rpath_off = rpath.map(&mut add);
        let soname_off = soname.map(&mut add);

        // 2) dynamic section: NEEDED* [RUNPATH] [RPATH] [SONAME] STRTAB STRSZ NULL
        let n_dyn =
            needed.len() + runpath.is_some() as usize + rpath.is_some() as usize + soname.is_some() as usize + 3;
        let dyn_len = n_dyn * dyn_entry_len;
        // identity mapping: vaddr == file offset in this fixture
        let strtab_off: u64 = (eh_len + 2 * ph_len + dyn_len) as u64;
        let file_len = eh_len + 2 * ph_len + dyn_len + strtab.len();

        let mut dyn_bytes: Vec<u8> = Vec::with_capacity(dyn_len);
        let push = |tag: u64, val: u64, out: &mut Vec<u8>| {
            if is64 {
                out.extend_from_slice(&e.u64(tag));
                out.extend_from_slice(&e.u64(val));
            } else {
                out.extend_from_slice(&e.u32(tag as u32));
                out.extend_from_slice(&e.u32(val as u32));
            }
        };
        for off in &needed_offs {
            push(DT_NEEDED, *off, &mut dyn_bytes);
        }
        if let Some(off) = runpath_off {
            push(DT_RUNPATH, off, &mut dyn_bytes);
        }
        if let Some(off) = rpath_off {
            push(DT_RPATH, off, &mut dyn_bytes);
        }
        if let Some(off) = soname_off {
            push(DT_SONAME, off, &mut dyn_bytes);
        }
        push(DT_STRTAB, strtab_off, &mut dyn_bytes); // identity: vaddr == offset
        push(DT_STRSZ, strtab.len() as u64, &mut dyn_bytes);
        push(DT_NULL, 0, &mut dyn_bytes);
        debug_assert_eq!(dyn_bytes.len(), dyn_len);

        // 3) ELF header
        let mut v = vec![0u8; eh_len];
        v[..4].copy_from_slice(b"\x7fELF");
        v[4] = class;
        v[5] = data; // data encoding
        v[6] = 1; // version
        v[16..18].copy_from_slice(&e.u16(3 /* ET_DYN */));
        let machine = if is64 { e.u16(62 /* EM_X86_64 */) } else { e.u16(3 /* EM_386 */) };
        v[18..20].copy_from_slice(&machine);
        if is64 {
            v[32..40].copy_from_slice(&e.u64(eh_len as u64)); // e_phoff
            v[54..56].copy_from_slice(&e.u16(ph_len as u16)); // e_phentsize
            v[56..58].copy_from_slice(&e.u16(2)); // e_phnum
        } else {
            v[28..32].copy_from_slice(&e.u32(eh_len as u32)); // e_phoff
            v[42..44].copy_from_slice(&e.u16(ph_len as u16)); // e_phentsize
            v[44..46].copy_from_slice(&e.u16(2)); // e_phnum
        }

        // 4) program headers
        let ph64 = |ptype: u32, offset: u64, vaddr: u64, filesz: u64, out: &mut Vec<u8>| {
            out.extend_from_slice(&e.u32(ptype)); // p_type
            out.extend_from_slice(&e.u32(4)); // p_flags (PF_R)
            out.extend_from_slice(&e.u64(offset));
            out.extend_from_slice(&e.u64(vaddr));
            out.extend_from_slice(&e.u64(vaddr)); // p_paddr
            out.extend_from_slice(&e.u64(filesz));
            out.extend_from_slice(&e.u64(filesz)); // p_memsz
            out.extend_from_slice(&e.u64(0x1000)); // p_align
        };
        let ph32 = |ptype: u32, offset: u64, vaddr: u64, filesz: u64, out: &mut Vec<u8>| {
            out.extend_from_slice(&e.u32(ptype));
            out.extend_from_slice(&e.u32(offset as u32));
            out.extend_from_slice(&e.u32(vaddr as u32));
            out.extend_from_slice(&e.u32(vaddr as u32)); // p_paddr
            out.extend_from_slice(&e.u32(filesz as u32));
            out.extend_from_slice(&e.u32(filesz as u32)); // p_memsz
            out.extend_from_slice(&e.u32(4)); // p_flags (PF_R)
            out.extend_from_slice(&e.u32(0x1000)); // p_align
        };
        let dyn_off = (eh_len + 2 * ph_len) as u64;
        if is64 {
            ph64(PT_LOAD, 0, 0, file_len as u64, &mut v); // identity mapping
            ph64(PT_DYNAMIC, dyn_off, dyn_off, dyn_len as u64, &mut v);
        } else {
            ph32(PT_LOAD, 0, 0, file_len as u64, &mut v);
            ph32(PT_DYNAMIC, dyn_off, dyn_off, dyn_len as u64, &mut v);
        }

        v.extend_from_slice(&dyn_bytes);
        v.extend_from_slice(&strtab);
        v
    }

    /// The old call site's shorthand: ELF64 little endian.
    fn elf64_fixture(needed: &[&str], runpath: Option<&str>, soname: Option<&str>) -> Vec<u8> {
        elf_fixture(2, 1, needed, runpath, None, soname)
    }

    #[test]
    fn elf64_fixture_parses() {
        let v = elf64_fixture(&["libz.so.1", "libc.so.6"], Some("/build/src/lib"), None);
        let info = parse_elf(&v).unwrap().expect("dynamic ELF");
        assert_eq!(info.needed, vec!["libz.so.1", "libc.so.6"]);
        assert_eq!(info.runpath.as_deref(), Some("/build/src/lib"));
        assert!(info.rpath.is_none(), "only runpath was written");
    }

    #[test]
    fn elf64_fixture_without_runpath() {
        let v = elf64_fixture(&["libc.so.6"], None, Some("libprobe.so.1"));
        let info = parse_elf(&v).unwrap().expect("dynamic ELF");
        assert_eq!(info.needed, vec!["libc.so.6"]);
        assert!(info.runpath.is_none());
        assert_eq!(info.soname.as_deref(), Some("libprobe.so.1"));
    }

    /// DT_RPATH is the pre-2005 field; legacy toolchains still emit it.
    #[test]
    fn rpath_is_reported_like_runpath() {
        let v = elf_fixture(2, 1, &["libc.so.6"], None, Some("/old-rpath"), None);
        let info = parse_elf(&v).unwrap().expect("dynamic ELF");
        assert_eq!(info.rpath.as_deref(), Some("/old-rpath"));
        assert_eq!(info.runpath.as_deref(), None);
    }

    /// ELF32 parses the same fields, little AND big endian — goblin's
    /// class/endianness handling must hold for both.
    #[test]
    fn elf32_fixtures_parse_in_both_endiannesses() {
        for (name, data_encoding) in [("LE", 1u8), ("BE", 2u8)] {
            let v = elf_fixture(
                1,
                data_encoding,
                &["libssl.so.3", "libc.so.6"],
                Some("/build/pkg/lib"),
                None,
                Some("libcrypto.so.3"),
            );
            let info = parse_elf(&v)
                .unwrap_or_else(|e| panic!("ELF32 {name} must parse: {e}"))
                .expect("dynamic ELF");
            assert_eq!(info.needed, vec!["libssl.so.3", "libc.so.6"]);
            assert_eq!(info.runpath.as_deref(), Some("/build/pkg/lib"));
            assert_eq!(info.soname.as_deref(), Some("libcrypto.so.3"));
        }
    }

    /// A big-endian ELF64 reads identically to its LE twin.
    #[test]
    fn elf64_be_fixture_parses() {
        let v = elf_fixture(2, 2, &["libm.so.6"], None, None, None);
        let info = parse_elf(&v).unwrap().expect("dynamic ELF");
        assert_eq!(info.needed, vec!["libm.so.6"]);
    }

    /// Truncations, mutated header bytes and extreme field values must all
    /// come back as ordinary errors / empty info, never a panic (the reader
    /// runs over untrusted staged files).
    #[test]
    fn malformed_elf_inputs_never_panic() {
        let good = elf64_fixture(&["libc.so.6"], Some("/r"), Some("s.so.1"));

        // 1) every truncation length
        for n in 0..=good.len() {
            let _ = parse_elf(&good[..n]);
        }

        // 2) every EL header byte mutated in isolation
        for i in 0..64 {
            for delta in [1u8, 0x7f, 0xff] {
                let mut m = good.clone();
                m[i] = m[i].wrapping_add(delta);
                let _ = parse_elf(&m);
            }
        }

        // 3) extreme header field values (offset 32..40 = e_phoff on ELF64,
        //    54..56 = e_phentsize, 56..58 = e_phnum)
        let mut m = good.clone();
        m[32..40].copy_from_slice(&u64::MAX.to_le_bytes()); // e_phoff far OOB
        assert!(parse_elf(&m).is_err(), "huge e_phoff is an error");
        let mut m = good.clone();
        m[56..58].copy_from_slice(&0xffffu16.to_le_bytes()); // e_phnum
        let _ = parse_elf(&m); // must not panic
        let mut m = good.clone();
        m[54..56].copy_from_slice(&0xffffu16.to_le_bytes()); // e_phentsize
        let _ = parse_elf(&m); // must not panic
    }

    /// Dangling string-table addresses must not panic the parser.
    #[test]
    fn dangling_string_offsets_do_not_panic() {
        let good = elf64_fixture(&["libc.so.6"], Some("/r"), None);
        let dyn_off = 64 + 2 * 56; // fixture layout: dyn section right after both phdrs
        // DT_STRTAB holds the vaddr; point it far out of bounds
        let mut m = good.clone();
        m[dyn_off + 8..dyn_off + 16].copy_from_slice(&0xffff_ffff_u64.to_le_bytes());
        let _ = parse_elf(&m);
        // and every DT_NEEDED val into the dynamic section bytes themselves
        // (in bounds, but not a real string-table position)
        let mut m = good.clone();
        let garbage = (dyn_off + 10) as u64; // mid-dyn-section byte position
        m[dyn_off + 8..dyn_off + 16].copy_from_slice(&garbage.to_le_bytes());
        let _ = parse_elf(&m);
    }

    /// Relocatable objects and core files have no interpreter-driven
    /// dynamic info to audit (matching the previous reader).
    #[test]
    fn rel_and_core_objects_parse_to_none() {
        let mut rel = elf64_fixture(&["libc.so.6"], None, None);
        rel[16..18].copy_from_slice(&1u16.to_le_bytes()); // ET_REL
        assert!(matches!(parse_elf(&rel), Ok(None)));
        let mut core = rel.clone();
        core[16..18].copy_from_slice(&4u16.to_le_bytes()); // ET_CORE
        assert!(matches!(parse_elf(&core), Ok(None)));
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
