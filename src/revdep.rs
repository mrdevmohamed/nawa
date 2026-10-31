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
//! Besides `scan`: `affected` (what an install/update/remove can have broken,
//! used for the automatic check), `suggest` (which package provides a missing
//! library: file lists of locally built archives, then a recipe-name match)
//! and `Report::to_json` (machine-readable output).
//!
//! All lookups happen inside `--root`: `install::resolve_in_root` resolves
//! absolute symlinks relative to the root instead of the host.

use anyhow::Result;
use goblin::elf::header::{ELFCLASS64, EI_CLASS};
use goblin::elf::Elf;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
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

/// A `DT_NEEDED` entry that could not be satisfied.
#[derive(Debug, Clone, PartialEq)]
pub struct Missing {
    pub name: String,
    /// a library of that name exists, but for another ELF class/machine
    pub wrong_arch: bool,
}

#[derive(Debug)]
pub struct BrokenFile {
    pub path: String,
    pub missing: Vec<Missing>,
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

    /// Names of the libraries that are not there at all (what `suggest` can
    /// look for; a wrong-architecture library is not "missing").
    pub fn missing_libs(&self) -> BTreeSet<String> {
        self.packages
            .values()
            .chain(std::iter::once(&self.unowned))
            .flat_map(|pr| pr.files.iter())
            .flat_map(|f| f.missing.iter())
            .filter(|m| !m.wrong_arch)
            .map(|m| m.name.clone())
            .collect()
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
                Lib::WrongArch => missing.push(Missing { name: lib.clone(), wrong_arch: true }),
                Lib::Missing => missing.push(Missing { name: lib.clone(), wrong_arch: false }),
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

// ---- what a change can have broken ------------------------------------------

/// Packages worth re-checking after `changed` were installed, updated or
/// removed: the ones still installed, plus every installed package that
/// lists one of them in its run-dependencies (nawa derives those from
/// `DT_NEEDED`, so they are exactly the programs linked against them).
pub fn affected(db: &Db, changed: &[String]) -> Result<Vec<String>> {
    let installed = db.installed()?;
    let names: HashSet<&str> = installed.iter().map(|e| e.name.as_str()).collect();
    let mut out: BTreeSet<String> = changed
        .iter()
        .filter(|c| names.contains(c.as_str()))
        .cloned()
        .collect();
    for e in &installed {
        if e.run.iter().any(|r| changed.contains(r)) {
            out.insert(e.name.clone());
        }
    }
    Ok(out.into_iter().collect())
}

/// `pkgname` of a package archive (`.PKGINFO`), if it can be read.
pub fn archive_name(archive: &Path) -> Option<String> {
    let info = crate::archive::read_pkginfo(archive).ok()?;
    crate::archive::parse_pkginfo(&info).ok()?.remove("pkgname")
}

// ---- which package provides a missing library ----------------------------------

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Source {
    /// the file list of a locally built archive contains the library
    Archive,
    /// no file list known: a recipe is named like the library (`libfoo.so.1` → `foo`)
    Recipe,
}

impl Source {
    pub fn as_str(&self) -> &'static str {
        match self {
            Source::Archive => "archive",
            Source::Recipe => "recipe",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Provider {
    pub package: String,
    pub source: Source,
    pub installed: bool,
}

/// missing library name → candidate providers
pub type Suggestions = BTreeMap<String, Vec<Provider>>;

/// Look for providers of `missing`: first the file lists of the archives in
/// `out_dir` (authoritative), then — for what is still unexplained — recipes
/// whose name matches the library name.
pub fn suggest(
    missing: &BTreeSet<String>,
    out_dir: &Path,
    recipes: &[String],
    installed: &HashSet<String>,
) -> Suggestions {
    let mut sug = Suggestions::new();
    if missing.is_empty() {
        return sug;
    }
    for (lib, pkgs) in archive_providers(out_dir, missing) {
        for p in pkgs {
            let installed = installed.contains(&p);
            sug.entry(lib.clone())
                .or_default()
                .push(Provider { package: p, source: Source::Archive, installed });
        }
    }
    for lib in missing {
        if sug.contains_key(lib) {
            continue;
        }
        for cand in name_guesses(lib) {
            if recipes.contains(&cand) {
                let installed = installed.contains(&cand);
                let v = sug.entry(lib.clone()).or_default();
                if !v.iter().any(|p| p.package == cand) {
                    v.push(Provider { package: cand, source: Source::Recipe, installed });
                }
            }
        }
    }
    sug
}

/// `libpng16.so.16` → `png16`, `libpng16`, `png`, `libpng`.
fn name_guesses(lib: &str) -> Vec<String> {
    let stem = lib.strip_prefix("lib").unwrap_or(lib);
    let stem = stem.split(".so").next().unwrap_or(stem);
    if stem.is_empty() {
        return Vec::new();
    }
    let bare = stem.trim_end_matches(|c: char| c.is_ascii_digit() || c == '-' || c == '.');
    let mut out: Vec<String> = Vec::new();
    for s in [stem, bare] {
        if s.is_empty() {
            continue;
        }
        for c in [s.to_string(), format!("lib{s}")] {
            if !out.contains(&c) {
                out.push(c);
            }
        }
    }
    out
}

/// library name → packages whose archive in `dir` ships it.
fn archive_providers(dir: &Path, wanted: &BTreeSet<String>) -> BTreeMap<String, Vec<String>> {
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let Ok(rd) = std::fs::read_dir(dir) else {
        return out;
    };
    let mut files: Vec<std::path::PathBuf> = rd
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.to_string_lossy().ends_with(".nawa.tar.zst"))
        .collect();
    files.sort();
    for f in files {
        let Some(libs) = archive_libs(&f, wanted) else {
            continue;
        };
        if libs.is_empty() {
            continue;
        }
        let Some(pkg) = archive_name(&f) else {
            continue;
        };
        for l in libs {
            let v = out.entry(l).or_default();
            if !v.contains(&pkg) {
                v.push(pkg.clone());
            }
        }
    }
    out
}

/// Which of `wanted` does this archive ship? (same library-path rule as the
/// soname index of the closure builder: `closure::soname_of`)
fn archive_libs(archive: &Path, wanted: &BTreeSet<String>) -> Option<Vec<String>> {
    let f = std::fs::File::open(archive).ok()?;
    let dec = zstd::Decoder::new(f).ok()?;
    let mut ar = tar::Archive::new(dec);
    let mut found = Vec::new();
    for entry in ar.entries().ok()? {
        let Ok(entry) = entry else {
            break;
        };
        let Ok(p) = entry.path() else {
            continue;
        };
        let Ok(rel) = p.strip_prefix(crate::archive::PAYLOAD) else {
            continue;
        };
        let rel_s = rel.to_string_lossy();
        let rel_s: &str = &rel_s;
        if crate::closure::soname_of(rel_s).is_some() {
            let base = rel_s.rsplit('/').next().unwrap_or(rel_s);
            if wanted.contains(base) {
                found.push(base.to_string());
            }
        }
    }
    Some(found)
}

// ---- JSON ----------------------------------------------------------------------

fn jstr(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('"');
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

fn jlist<T>(items: impl IntoIterator<Item = T>, f: impl Fn(T) -> String) -> String {
    format!("[{}]", items.into_iter().map(f).collect::<Vec<_>>().join(","))
}

fn json_file(f: &BrokenFile, sug: &Suggestions) -> String {
    let missing = jlist(&f.missing, |m| {
        let providers = sug.get(&m.name).map(Vec::as_slice).unwrap_or(&[]);
        let providers = jlist(providers, |p| {
            format!(
                "{{\"package\":{},\"source\":{},\"installed\":{}}}",
                jstr(&p.package),
                jstr(p.source.as_str()),
                p.installed
            )
        });
        format!(
            "{{\"name\":{},\"wrong_arch\":{},\"providers\":{providers}}}",
            jstr(&m.name),
            m.wrong_arch
        )
    });
    let interp = f.interp.as_deref().map_or("null".to_string(), jstr);
    format!("{{\"path\":{},\"missing\":{missing},\"interp\":{interp}}}", jstr(&f.path))
}

impl Report {
    /// The whole report as one JSON document (for scripts and CI).
    pub fn to_json(&self, sug: &Suggestions) -> String {
        let pkgs = jlist(&self.packages, |(name, pr)| {
            format!(
                "{{\"name\":{},\"files\":{},\"dead_links\":{}}}",
                jstr(name),
                jlist(&pr.files, |f| json_file(f, sug)),
                jlist(&pr.dead_links, |l| jstr(l))
            )
        });
        let unowned = if self.unowned_checked {
            format!(
                "{{\"files\":{},\"dead_links\":{}}}",
                jlist(&self.unowned.files, |f| json_file(f, sug)),
                jlist(&self.unowned.dead_links, |l| jstr(l))
            )
        } else {
            "null".to_string()
        };
        let rebuild = jlist(self.packages.keys(), |k| jstr(k));
        format!(
            "{{\"ok\":{},\"scanned\":{{\"packages\":{},\"elf_files\":{},\"unowned_files\":{},\"unparseable\":{}}},\"packages\":{pkgs},\"unowned\":{unowned},\"rebuild\":{rebuild}}}",
            self.is_clean(),
            self.scanned_packages,
            self.elf_files,
            self.unowned_files,
            self.unparseable
        )
    }
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

    #[test]
    fn affected_is_the_package_plus_its_dependents() {
        let root = tmp("affected");
        let db = Db::new(&root);
        let files = vec!["/usr/share/x".to_string()];
        db.add(&entry("a"), &files, &HashMap::new()).unwrap();
        db.add(&Entry { run: vec!["a".into()], ..entry("b") }, &files, &HashMap::new()).unwrap();
        db.add(&Entry { run: vec!["b".into()], ..entry("c") }, &files, &HashMap::new()).unwrap();
        db.add(&Entry { run: vec!["gone".into()], ..entry("d") }, &files, &HashMap::new()).unwrap();
        assert_eq!(affected(&db, &["a".to_string()]).unwrap(), vec!["a", "b"]);
        // a removed package is no longer installed: only its dependents remain
        assert_eq!(affected(&db, &["gone".to_string()]).unwrap(), vec!["d"]);
        assert!(affected(&db, &["nobody".to_string()]).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn name_guesses_cover_common_library_names() {
        assert_eq!(name_guesses("libz.so.1"), vec!["z", "libz"]);
        assert_eq!(name_guesses("libpng16.so.16"), vec!["png16", "libpng16", "png", "libpng"]);
        assert_eq!(name_guesses("libglib-2.0.so.0"), vec!["glib-2.0", "libglib-2.0", "glib", "libglib"]);
        assert!(name_guesses(".so").is_empty());
    }

    #[test]
    fn suggestions_come_from_archive_file_lists_then_recipe_names() {
        let base = tmp("suggest");
        let out = base.join("out");
        std::fs::create_dir_all(&out).unwrap();
        let st = base.join("st");
        std::fs::create_dir_all(st.join("usr/lib")).unwrap();
        std::fs::write(st.join("usr/lib/libfoo.so.1"), b"x").unwrap();
        std::fs::write(st.join("usr/lib/other.txt"), b"x").unwrap();
        let pkginfo = "pkgname = foo\npkgver = 1.0\npkgrel = 1\narch = x86_64\nsize = 1\n";
        crate::archive::create(&st, pkginfo, &out.join("foo-1.0-1.nawa.tar.zst")).unwrap();

        let missing: BTreeSet<String> =
            ["libfoo.so.1", "libbar.so.2", "libnope.so"].iter().map(|s| s.to_string()).collect();
        let recipes = vec!["bar".to_string(), "zzz".to_string()];
        let installed: HashSet<String> = ["foo".to_string()].into_iter().collect();
        let sug = suggest(&missing, &out, &recipes, &installed);
        assert_eq!(
            sug["libfoo.so.1"],
            vec![Provider { package: "foo".into(), source: Source::Archive, installed: true }]
        );
        assert_eq!(
            sug["libbar.so.2"],
            vec![Provider { package: "bar".into(), source: Source::Recipe, installed: false }]
        );
        assert!(!sug.contains_key("libnope.so"));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn json_output_is_escaped_and_complete() {
        assert_eq!(jstr("a\"b\\c\n\u{1}"), "\"a\\\"b\\\\c\\n\\u0001\"");
        let mut rep = Report { scanned_packages: 2, elf_files: 5, ..Default::default() };
        rep.packages.insert(
            "curl".into(),
            PkgReport {
                files: vec![BrokenFile {
                    path: "/usr/bin/curl".into(),
                    missing: vec![Missing { name: "libz.so.1".into(), wrong_arch: false }],
                    interp: None,
                }],
                dead_links: vec![],
            },
        );
        let mut sug = Suggestions::new();
        sug.insert(
            "libz.so.1".into(),
            vec![Provider { package: "zlib".into(), source: Source::Recipe, installed: false }],
        );
        assert_eq!(rep.missing_libs().into_iter().collect::<Vec<_>>(), vec!["libz.so.1"]);
        assert_eq!(
            rep.to_json(&sug),
            r#"{"ok":false,"scanned":{"packages":2,"elf_files":5,"unowned_files":0,"unparseable":0},"packages":[{"name":"curl","files":[{"path":"/usr/bin/curl","missing":[{"name":"libz.so.1","wrong_arch":false,"providers":[{"package":"zlib","source":"recipe","installed":false}]}],"interp":null}],"dead_links":[]}],"unowned":null,"rebuild":["curl"]}"#
        );
        let clean = Report::default().to_json(&Suggestions::new());
        assert!(clean.starts_with("{\"ok\":true,") && clean.ends_with("\"rebuild\":[]}"));
    }
}
