//! `nawa revdep` — find installed files whose shared-library links are broken.
//!
//! Every ELF file owned by an installed package (taken from the db file lists)
//! is read with the goblin-based reader the post-build lint already uses
//! (`lint::parse_elf`); each `DT_NEEDED` entry is then looked up the way the
//! loader would: RPATH/RUNPATH (with `$ORIGIN`) first, then the system library
//! directories (`--lib-dir`, `/etc/ld.so.conf` + includes, `/etc/ld-musl-*.path`
//! and the usual `/lib /usr/lib ...`). A candidate only counts when its ELF
//! header (goblin) has the same class and machine as the file that needs it —
//! like the loader, a 32-bit library never satisfies a 64-bit program.
//! A missing library, a wrong-architecture library or a missing ELF
//! interpreter marks the file — and so the owning package — as broken.
//!
//! With `--unowned` / `--scan-dir` the same checks also run (via `walkdir`)
//! over files that no package owns, e.g. things installed by hand.
//!
//! All lookups happen inside `--root`: `install::resolve_in_root` resolves
//! absolute symlinks relative to the root instead of the host.

use anyhow::Result;
use goblin::elf::header::{ELFCLASS64, EI_CLASS};
use goblin::elf::Elf;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Read;
use std::path::Path;

use crate::commands::glob_match;
use crate::db::Db;
use crate::install::{exists_in_root, resolve_in_root};
use crate::lint::{self, ElfInfo};

const DEFAULT_DIRS: &[&str] = &["/lib", "/usr/lib", "/lib64", "/usr/lib64", "/usr/local/lib"];

/// Where `--unowned` looks when no `--scan-dir` is given.
const DEFAULT_SCAN: &[&str] = &["/usr", "/bin", "/sbin", "/lib", "/lib64", "/opt"];

pub struct Options<'a> {
    /// extra library directories, searched before the system ones
    pub lib_dirs: &'a [String],
    /// `DT_NEEDED` names matching one of these globs are not reported
    pub ignore: &'a [String],
    /// also report symlinks whose target is gone
    pub symlinks: bool,
    /// also scan files no package owns (in `DEFAULT_SCAN` unless `scan_dirs`)
    pub unowned: bool,
    /// directories to scan for unowned files (implies `unowned`)
    pub scan_dirs: &'a [String],
}

#[derive(Debug)]
pub struct BrokenFile {
    pub path: String,
    pub missing: Vec<String>,
    /// the ELF interpreter, when it does not exist in the root
    pub interp: Option<String>,
}

#[derive(Debug, Default)]
pub struct PkgReport {
    pub files: Vec<BrokenFile>,
    pub dead_links: Vec<String>,
}

impl PkgReport {
    pub fn is_clean(&self) -> bool {
        self.files.is_empty() && self.dead_links.is_empty()
    }
}

#[derive(Debug, Default)]
pub struct Report {
    /// broken packages only
    pub packages: BTreeMap<String, PkgReport>,
    /// broken files that no package owns (only filled with `--unowned`)
    pub unowned: PkgReport,
    pub unowned_checked: bool,
    pub unowned_files: usize,
    pub scanned_packages: usize,
    pub elf_files: usize,
    /// ELF magic but goblin could not parse it
    pub unparseable: usize,
}

impl Report {
    pub fn is_clean(&self) -> bool {
        self.packages.is_empty() && self.unowned.is_clean()
    }
}

// ---- library search path -----------------------------------------------------

/// Library search directories: `--lib-dir`, `/etc/ld.so.conf` (+ includes),
/// `/etc/ld-musl-*.path`, then the defaults. First occurrence wins.
pub fn system_dirs(root: &Path, extra: &[String]) -> Vec<String> {
    let mut all: Vec<String> = extra.to_vec();
    all.extend(ld_so_conf(root, "etc/ld.so.conf", 0));
    all.extend(musl_paths(root));
    all.extend(DEFAULT_DIRS.iter().map(|s| s.to_string()));
    let mut seen = HashSet::new();
    all.retain(|d| seen.insert(d.clone()));
    all
}

/// Names in `<root>/<dir>` matching `pat`, sorted.
fn matching_files(root: &Path, dir: &Path, pat: &str) -> Vec<String> {
    let Ok(rd) = std::fs::read_dir(root.join(dir)) else {
        return Vec::new();
    };
    let mut names: Vec<String> = rd
        .filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| glob_match(pat, n))
        .collect();
    names.sort();
    names
}

fn ld_so_conf(root: &Path, rel: &str, depth: u32) -> Vec<String> {
    let mut out = Vec::new();
    if depth > 8 {
        return out;
    }
    let Ok(text) = std::fs::read_to_string(root.join(rel)) else {
        return out;
    };
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        let include = line
            .strip_prefix("include")
            .filter(|r| r.starts_with(char::is_whitespace));
        if let Some(pat) = include {
            let pat = pat.trim();
            let pat_rel = match pat.strip_prefix('/') {
                Some(p) => p.to_string(),
                None => format!("etc/{pat}"),
            };
            let p = Path::new(&pat_rel);
            let (Some(dir), Some(file_pat)) = (p.parent(), p.file_name().and_then(|f| f.to_str()))
            else {
                continue;
            };
            for n in matching_files(root, dir, file_pat) {
                let sub = dir.join(n);
                out.extend(ld_so_conf(root, &sub.to_string_lossy(), depth + 1));
            }
        } else if line.starts_with('/') {
            out.push(line.to_string());
        }
    }
    out
}

/// musl's `/etc/ld-musl-<arch>.path`: directories separated by `:` or newlines.
fn musl_paths(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    for f in matching_files(root, Path::new("etc"), "ld-musl-*.path") {
        let Ok(text) = std::fs::read_to_string(root.join("etc").join(f)) else {
            continue;
        };
        out.extend(
            text.split([':', '\n'])
                .map(str::trim)
                .filter(|p| p.starts_with('/'))
                .map(String::from),
        );
    }
    out
}

/// Directory part of an absolute path (`/usr/bin/curl` → `/usr/bin`).
fn origin_of(path: &str) -> &str {
    match path.rsplit_once('/') {
        Some(("", _)) | None => "/",
        Some((d, _)) => d,
    }
}

/// The file's own search path. Like the loader, DT_RPATH is ignored when
/// DT_RUNPATH exists; `$ORIGIN` is expanded, other `$VAR`s and relative
/// entries are skipped.
fn own_dirs(info: &ElfInfo, origin: &str) -> Vec<String> {
    let Some(src) = info.runpath.as_ref().or(info.rpath.as_ref()) else {
        return Vec::new();
    };
    src.split(':')
        .map(|p| p.replace("${ORIGIN}", origin).replace("$ORIGIN", origin))
        .filter(|p| p.starts_with('/') && !p.contains('$'))
        .collect()
}

// ---- the checker ---------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq)]
enum Lib {
    Found,
    WrongArch,
    Missing,
}

enum Finding {
    Broken(BrokenFile),
    DeadLink(String),
}

fn record(pr: &mut PkgReport, f: Option<Finding>) {
    match f {
        Some(Finding::Broken(b)) => pr.files.push(b),
        Some(Finding::DeadLink(l)) => pr.dead_links.push(l),
        None => {}
    }
}

struct Scanner<'a> {
    root: &'a Path,
    opts: &'a Options<'a>,
    sys: Vec<String>,
    /// (soname, is_64, machine) → result of the system-directory lookup
    cache: HashMap<(String, bool, u16), Lib>,
    elf_files: usize,
    unparseable: usize,
}

impl<'a> Scanner<'a> {
    fn new(root: &'a Path, opts: &'a Options<'a>) -> Self {
        Scanner {
            root,
            opts,
            sys: system_dirs(root, opts.lib_dirs),
            cache: HashMap::new(),
            elf_files: 0,
            unparseable: 0,
        }
    }

    /// Is `path` (inside the root) a loadable ELF of the same class/machine?
    fn probe(&self, path: &str, info: &ElfInfo) -> Lib {
        let Some(host) = resolve_in_root(self.root, path) else {
            return Lib::Missing;
        };
        let Ok(f) = std::fs::File::open(host) else {
            return Lib::Missing;
        };
        let mut head = Vec::new();
        if f.take(64).read_to_end(&mut head).is_err() {
            return Lib::Missing;
        }
        match Elf::parse_header(&head) {
            Ok(h) if (h.e_ident[EI_CLASS] == ELFCLASS64) == info.is_64 && h.e_machine == info.machine => {
                Lib::Found
            }
            Ok(_) => Lib::WrongArch,
            Err(_) => Lib::Missing, // not an ELF (linker script, directory, ...)
        }
    }

    fn search(&self, dirs: &[String], name: &str, info: &ElfInfo) -> Lib {
        let mut wrong = false;
        for d in dirs {
            match self.probe(&format!("{d}/{name}"), info) {
                Lib::Found => return Lib::Found,
                Lib::WrongArch => wrong = true,
                Lib::Missing => {}
            }
        }
        if wrong {
            Lib::WrongArch
        } else {
            Lib::Missing
        }
    }

    fn lookup(&mut self, name: &str, own: &[String], info: &ElfInfo) -> Lib {
        if name.contains('/') {
            return self.probe(name, info);
        }
        if self.search(own, name, info) == Lib::Found {
            return Lib::Found;
        }
        let key = (name.to_string(), info.is_64, info.machine);
        if let Some(r) = self.cache.get(&key) {
            return *r;
        }
        let r = self.search(&self.sys, name, info);
        self.cache.insert(key, r);
        r
    }

    /// Check one path (absolute, inside the root).
    fn check(&mut self, abs: &str) -> Option<Finding> {
        let host = self.root.join(abs.trim_start_matches('/'));
        let md = std::fs::symlink_metadata(&host).ok()?;
        if md.file_type().is_symlink() {
            let dead = self.opts.symlinks && !exists_in_root(self.root, abs);
            return dead.then(|| Finding::DeadLink(abs.to_string()));
        }
        if !md.is_file() {
            return None;
        }
        // cheap magic check first, the rest of the file only for ELFs
        let mut fh = std::fs::File::open(&host).ok()?;
        let mut data = vec![0u8; 4];
        if fh.read_exact(&mut data).is_err() || !data.starts_with(b"\x7fELF") {
            return None;
        }
        fh.read_to_end(&mut data).ok()?;
        self.elf_files += 1;
        let info = match lint::parse_elf(&data) {
            Ok(Some(i)) => i,
            Ok(None) => return None,
            Err(_) => {
                self.unparseable += 1;
                return None;
            }
        };

        let own = own_dirs(&info, origin_of(abs));
        let mut missing = Vec::new();
        for lib in &info.needed {
            if self.opts.ignore.iter().any(|g| glob_match(g, lib)) {
                continue;
            }
            match self.lookup(lib, &own, &info) {
                Lib::Found => {}
                Lib::WrongArch => missing.push(format!("{lib} (wrong architecture)")),
                Lib::Missing => missing.push(lib.clone()),
            }
        }
        let interp = info.interp.clone().filter(|i| !exists_in_root(self.root, i));
        let broken = !missing.is_empty() || interp.is_some();
        broken.then(|| Finding::Broken(BrokenFile { path: abs.to_string(), missing, interp }))
    }
}

fn scan_dirs(opts: &Options) -> Vec<String> {
    if !opts.scan_dirs.is_empty() {
        opts.scan_dirs.to_vec()
    } else if opts.unowned {
        DEFAULT_SCAN.iter().map(|s| s.to_string()).collect()
    } else {
        Vec::new()
    }
}

/// Scan `names` (empty = every installed package), plus unowned files when
/// asked to.
pub fn scan(root: &Path, db: &Db, names: &[String], opts: &Options) -> Result<Report> {
    let targets: Vec<String> = if names.is_empty() {
        db.installed()?.into_iter().map(|e| e.name).collect()
    } else {
        names.to_vec()
    };
    let mut sc = Scanner::new(root, opts);
    let mut rep = Report::default();

    for name in &targets {
        rep.scanned_packages += 1;
        let mut pr = PkgReport::default();
        for f in db.files(name)? {
            let abs = format!("/{}", f.trim_start_matches('/'));
            record(&mut pr, sc.check(&abs));
        }
        if !pr.is_clean() {
            rep.packages.insert(name.clone(), pr);
        }
    }

    let dirs = scan_dirs(opts);
    if !dirs.is_empty() {
        rep.unowned_checked = true;
        let owned: HashSet<String> = db
            .file_owners()?
            .into_keys()
            .map(|k| k.trim_start_matches('/').to_string())
            .collect();
        let mut seen: HashSet<String> = HashSet::new();
        for d in &dirs {
            let start = root.join(d.trim_start_matches('/'));
            // symlinked roots (/bin -> usr/bin) are not followed: the real
            // directory is scanned through its own name
            let walk = walkdir::WalkDir::new(&start)
                .follow_links(false)
                .follow_root_links(false);
            for e in walk.into_iter().flatten() {
                if e.file_type().is_dir() {
                    continue;
                }
                let Ok(rel) = e.path().strip_prefix(root) else {
                    continue;
                };
                let rel = rel.to_string_lossy().into_owned();
                if owned.contains(&rel) || !seen.insert(rel.clone()) {
                    continue;
                }
                rep.unowned_files += 1;
                let found = sc.check(&format!("/{rel}"));
                record(&mut rep.unowned, found);
            }
        }
    }

    rep.elf_files = sc.elf_files;
    rep.unparseable = sc.unparseable;
    Ok(rep)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Entry;

    fn tmp(tag: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("nawa-revdep-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn entry(name: &str) -> Entry {
        Entry {
            name: name.into(),
            version: "1".into(),
            rel: 1,
            arch: String::new(),
            reason: "explicit".into(),
            run: vec![],
            install_date: 0,
            size: 0,
        }
    }

    /// A bare ELF header (all `Header::parse` needs): little-endian, ET_DYN.
    fn fake_header(class64: bool, machine: u16) -> Vec<u8> {
        let mut h = vec![0u8; 64];
        h[..4].copy_from_slice(b"\x7fELF");
        h[4] = if class64 { 2 } else { 1 };
        h[5] = 1; // little endian
        h[6] = 1; // EI_VERSION
        h[16..18].copy_from_slice(&3u16.to_le_bytes()); // ET_DYN
        h[18..20].copy_from_slice(&machine.to_le_bytes());
        h[20..24].copy_from_slice(&1u32.to_le_bytes()); // e_version
        h
    }

    fn opts<'a>(unowned: bool, scan_dirs: &'a [String]) -> Options<'a> {
        Options { lib_dirs: &[], ignore: &[], symlinks: true, unowned, scan_dirs }
    }

    #[test]
    fn origin_and_own_dirs() {
        assert_eq!(origin_of("/usr/bin/curl"), "/usr/bin");
        assert_eq!(origin_of("/curl"), "/");
        let info = ElfInfo {
            rpath: Some("/ignored".into()),
            runpath: Some("$ORIGIN/../lib:/opt/x:$LIB/y:relative".into()),
            ..Default::default()
        };
        assert_eq!(own_dirs(&info, "/usr/bin"), vec!["/usr/bin/../lib", "/opt/x"]);
        let only_rpath = ElfInfo { rpath: Some("/a:/b".into()), ..Default::default() };
        assert_eq!(own_dirs(&only_rpath, "/x"), vec!["/a", "/b"]);
    }

    #[test]
    fn ld_so_conf_and_musl_paths_come_before_defaults() {
        let root = tmp("ldconf");
        std::fs::create_dir_all(root.join("etc/ld.so.conf.d")).unwrap();
        std::fs::write(root.join("etc/ld.so.conf"), "# c\ninclude ld.so.conf.d/*.conf\n/opt/base/lib\n").unwrap();
        std::fs::write(root.join("etc/ld.so.conf.d/a.conf"), "/opt/a/lib\n").unwrap();
        std::fs::write(root.join("etc/ld.so.conf.d/skip.txt"), "/never\n").unwrap();
        std::fs::write(root.join("etc/ld-musl-x86_64.path"), "/opt/m1/lib:/opt/m2/lib\n/opt/m3/lib\n").unwrap();
        let d = system_dirs(&root, &["/extra".to_string()]);
        assert_eq!(
            &d[..6],
            ["/extra", "/opt/a/lib", "/opt/base/lib", "/opt/m1/lib", "/opt/m2/lib", "/opt/m3/lib"]
        );
        assert!(d.contains(&"/usr/lib".to_string()) && !d.contains(&"/never".to_string()));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn library_architecture_must_match() {
        let root = tmp("arch");
        std::fs::create_dir_all(root.join("usr/lib")).unwrap();
        std::fs::write(root.join("usr/lib/libgood.so.1"), fake_header(true, 62)).unwrap(); // x86-64
        std::fs::write(root.join("usr/lib/lib32.so.1"), fake_header(false, 3)).unwrap(); // i386
        std::fs::write(root.join("usr/lib/libarm.so.1"), fake_header(true, 183)).unwrap(); // aarch64
        std::fs::write(root.join("usr/lib/libtext.so"), b"GROUP ( libgood.so.1 )").unwrap();
        // absolute symlink must resolve inside the root
        std::os::unix::fs::symlink("/usr/lib/libgood.so.1", root.join("usr/lib/liblink.so.1")).unwrap();
        let o = opts(false, &[]);
        let mut sc = Scanner::new(&root, &o);
        let me = ElfInfo { is_64: true, machine: 62, ..Default::default() };
        assert_eq!(sc.lookup("libgood.so.1", &[], &me), Lib::Found);
        assert_eq!(sc.lookup("liblink.so.1", &[], &me), Lib::Found);
        assert_eq!(sc.lookup("lib32.so.1", &[], &me), Lib::WrongArch);
        assert_eq!(sc.lookup("libarm.so.1", &[], &me), Lib::WrongArch);
        assert_eq!(sc.lookup("libtext.so", &[], &me), Lib::Missing);
        assert_eq!(sc.lookup("libnope.so", &[], &me), Lib::Missing);
        // the same names for a 32-bit program flip the verdicts
        let me32 = ElfInfo { is_64: false, machine: 3, ..Default::default() };
        assert_eq!(sc.lookup("lib32.so.1", &[], &me32), Lib::Found);
        assert_eq!(sc.lookup("libgood.so.1", &[], &me32), Lib::WrongArch);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn dead_symlinks_and_unowned_files() {
        let root = tmp("links");
        std::fs::create_dir_all(root.join("usr/bin")).unwrap();
        std::fs::create_dir_all(root.join("usr/lib")).unwrap();
        std::fs::write(root.join("usr/lib/real"), b"x").unwrap();
        // absolute targets: must be looked up under the root, not on the host
        std::os::unix::fs::symlink("/usr/lib/real", root.join("usr/bin/ok")).unwrap();
        std::os::unix::fs::symlink("/usr/lib/nope", root.join("usr/bin/dead")).unwrap();
        std::os::unix::fs::symlink("/usr/lib/gone", root.join("usr/bin/orphan")).unwrap();
        let db = Db::new(&root);
        db.add(
            &entry("demo"),
            &["/usr/bin/ok".to_string(), "/usr/bin/dead".to_string(), "/usr/lib/real".to_string()],
            &HashMap::new(),
        )
        .unwrap();

        let rep = scan(&root, &db, &[], &opts(false, &[])).unwrap();
        assert_eq!(rep.scanned_packages, 1);
        assert_eq!(rep.packages["demo"].dead_links, vec!["/usr/bin/dead"]);
        assert!(!rep.unowned_checked && rep.unowned.is_clean());

        // --unowned: the orphan shows up under "unowned", owned files are not repeated
        let rep = scan(&root, &db, &[], &opts(true, &[])).unwrap();
        assert!(rep.unowned_checked);
        assert_eq!(rep.unowned.dead_links, vec!["/usr/bin/orphan"]);
        assert_eq!(rep.packages["demo"].dead_links, vec!["/usr/bin/dead"]);
        assert!(!rep.is_clean());

        // --scan-dir limits where we look
        let dirs = vec!["/usr/lib".to_string()];
        let rep = scan(&root, &db, &[], &opts(false, &dirs)).unwrap();
        assert!(rep.unowned_checked && rep.unowned.is_clean());

        let off = Options { symlinks: false, ..opts(true, &[]) };
        assert!(scan(&root, &db, &[], &off).unwrap().is_clean());
        let _ = std::fs::remove_dir_all(&root);
    }
}
