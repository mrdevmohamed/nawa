//! Recipe-local assets (fetch-phase.md §5).
//!
//! Patches, config fragments and scripts that live beside the recipe are
//! snapshotted into `<work>/assets` — a fresh directory *outside* `src/` —
//! and exposed to the build read-only through `%{assetsdir}` /
//! `NAWA_ASSETSDIR` (§5.3). The digest that feeds the build id is computed
//! from the **copy**, in the same pass, so the build can only ever see the
//! bytes that were hashed even if the checkout changes mid-build.
//!
//! ```text
//! recipes/<pkg>/files/...             the checkout (no-follow walkers)
//!         │ snapshot() — copy, normalised modes, hash fed from the bytes
//!         ▼
//! <work>/assets/...                   the staged snapshot, read-only
//!         │ sha256 over records: path \0 x|- \0 size \0 sha256(content)
//!         ▼
//! build_id extra "assets" (§10.3)     probe: from the checkout, before <work>
//!                                     exists, for the --use-cache decision
//! ```

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use crate::recipe::Recipe;

/// A staged assets snapshot: the directory the build sees and the digest
/// recorded in the build id (§5.3 "two moments, one digest").
pub(crate) struct AssetsStaged {
    /// `<work>/assets`, bound read-only at `<build_inside>/assets`.
    pub dir: PathBuf,
    /// sha256 over the snapshot's records (§5.3.2).
    pub digest: String,
}

/// One regular file of an assets tree in the record form of §5.3.2.
struct Record {
    /// Relative path, raw bytes: asset paths need not be UTF-8, and they
    /// must never contain a NUL so records stay byte-sortable.
    rel: Vec<u8>,
    /// The mode contribution is **only the owner execute bit** (§5.3.2):
    /// `pack_recipes` writes that header mode, so a synced tree and a git
    /// checkout differing in every other mode bit still hash the same.
    exec: bool,
    size: u64,
    /// sha256(content), lowercase hex.
    hex: String,
}

/// The declared asset paths, deduplicated in declaration order, as
/// (recipe-dir-relative, absolute host path); included directories are
/// swept recursively by the callers and never followed through symlinks,
/// so a link cannot escape its declared root.
fn include_roots(rec: &Recipe) -> Vec<(PathBuf, PathBuf)> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut out = Vec::new();
    for rel in &rec.assets.include {
        if seen.insert(rel.clone()) {
            out.push((PathBuf::from(rel), rec.dir.join(rel)));
        }
    }
    out
}

/// §5.2 entry rules: only regular files and directories, and a file named
/// `recipe.toml` is refused — `load_all`/`locate` parse that name at any
/// depth, so it must never hide inside a declared path.
fn guard(rec_dir: &Path, host: &Path, ft: std::fs::FileType) -> Result<()> {
    let rel = host.strip_prefix(rec_dir).unwrap_or(host);
    if ft.is_symlink() {
        bail!(
            "{}: symlinks are not allowed in assets (fetch-phase.md §5.2)",
            rel.display()
        );
    }
    if !ft.is_file() && !ft.is_dir() {
        bail!(
            "{}: special files (device/FIFO/socket) are not allowed in assets (§5.2)",
            rel.display()
        );
    }
    if ft.is_file()
        && host
            .file_name()
            .map(|n| n.as_encoded_bytes() == b"recipe.toml")
            .unwrap_or(false)
    {
        bail!(
            "{}: a file named `recipe.toml` inside an asset path would be picked up as a \
             recipe (fetch-phase.md §5.2)",
            rel.display()
        );
    }
    Ok(())
}

/// Open a regular file with `O_NOFOLLOW` (§5.2 "no-follow opens"): a
/// swap-in symlink after the walk fails the open instead of being followed.
fn open_nofollow(path: &Path) -> Result<std::fs::File> {
    use rustix::fs::OFlags;
    let fd = rustix::fs::open(
        path.as_os_str(),
        OFlags::RDONLY | OFlags::NOFOLLOW,
        rustix::fs::Mode::empty(),
    )
    .map_err(|e| anyhow::Error::msg(format!("open {}: {e}", path.display())))?;
    Ok(std::fs::File::from(fd))
}

/// Sweep every declared path of `rec` once. `stage_into` copies each file
/// while hashing it (the snapshot pass); `None` only reads (the checkout
/// probe). Returns the records of all files; relative paths are computed
/// against `rec.dir`, so all includes land in one deterministic namespace.
fn sweep(rec: &Recipe, stage_into: Option<&Path>) -> Result<Vec<Record>> {
    let mut records: Vec<Record> = Vec::new();
    for (rel, host) in include_roots(rec) {
        let walker = walkdir::WalkDir::new(&host)
            .follow_links(false)
            .sort_by_file_name();
        for entry in walker {
            let entry = entry.with_context(|| {
                format!(
                    "scan asset {} (declared as `{}` in the recipe {})",
                    host.display(),
                    rel.display(),
                    rec.package.name
                )
            })?;
            let path = entry.path();
            guard(&rec.dir, path, entry.file_type())?;
            if !entry.file_type().is_file() {
                continue; // directories carry no record (§5.3.2)
            }
            let rel = path.strip_prefix(&rec.dir).with_context(|| {
                format!(
                    "asset path {} escapes the recipe directory {}",
                    path.display(),
                    rec.dir.display()
                )
            })?;
            let md = std::fs::symlink_metadata(path)
                .with_context(|| format!("stat asset {}", path.display()))?;
            let exec = (md.permissions().mode() & 0o100) != 0;

            // content hash: fed from the bytes that land in the *copy* when
            // staging (§5.3.1) or that were read when probing — byte-identical
            let mut h = Sha256::new();
            let size = match stage_into {
                Some(root) => {
                    let dest = root.join(rel);
                    copy_hashed(path, &dest, &mut h)
                        .with_context(|| format!("copy asset into {}", dest.display()))?
                }
                None => hash_file(path, &mut h)?,
            };
            records.push(Record {
                rel: rel.as_os_str().as_bytes().to_vec(),
                exec,
                size,
                hex: hex::encode(h.finalize()),
            });
        }
    }
    if records.is_empty() {
        bail!(
            "recipe {} declares [assets] but none of the include paths contain a regular \
             file — empty directories are not staged (§5.2); check the include entries",
            rec.package.name
        );
    }
    // byte-wise sorted relative-path order (§5.3.2), with the defensive
    // duplicate check: two include entries that fold on each other must
    // never hash one file under both records
    records.sort_by(|a, b| a.rel.cmp(&b.rel));
    for w in records.windows(2) {
        if w[0].rel == w[1].rel {
            let shown = String::from_utf8_lossy(&w[0].rel);
            bail!("recipe {} includes {shown} twice under [assets] include", rec.package.name);
        }
    }
    Ok(records)
}

/// §5.3.2: sha256 over all records, concatenated:
/// `path \0 (x|-) \0 size \0 sha256(content)` per file.
fn digest_of(records: &[Record]) -> String {
    let mut h = Sha256::new();
    for r in records {
        h.update(&r.rel);
        h.update(b"\0");
        h.update(if r.exec { b"x" } else { b"-" });
        h.update(b"\0");
        h.update(r.size.to_string().as_bytes());
        h.update(b"\0");
        h.update(r.hex.as_bytes());
    }
    hex::encode(h.finalize())
}

// ---------------------------------------------------------------------------
// public API
// ---------------------------------------------------------------------------

/// Verify every declared asset path can be staged: it exists, holds no
/// symlink or special file and hides no `recipe.toml` (§5.2). Runs in
/// `build()` **before any download**, so a missing path fails in seconds
/// (§16 Assets 3). The same walker that the snapshot runs later, so validity
/// and the probe digest can never disagree with the staged tree.
pub(crate) fn precheck(rec: &Recipe) -> Result<()> {
    sweep(rec, None).map(|_| ())
}

/// The checkout-side digest (§5.3 "two moments, one digest"): computed by
/// `--use-cache` before `<work>` exists, by reading the recipe tree. The
/// digest *recorded* in the archive comes from `snapshot`; they differ only
/// if the checkout changes in between, and then the recorded id describes
/// what was actually built.
pub(crate) fn probe_digest(rec: &Recipe) -> Result<String> {
    Ok(digest_of(&sweep(rec, None)?))
}

/// Copy every declared path into `<work>/assets` and compute the digest
/// from the copy (§5.3). The snapshot is left read-only — files 0444 or
/// 0555 by the owner-execute bit, directories 0555 — which at
/// `none`/`env` isolation is the whole story (`chmod`, best effort, no
/// security boundary), and below the sandbox bind a harmless belt.
pub(crate) fn snapshot(rec: &Recipe, work: &Path) -> Result<AssetsStaged> {
    let dir = work.join("assets");
    // <work> is wiped on every build, but a prior failed run inside the
    // same build must never leave its bytes behind. The wipe needs a walk:
    // a previous snapshot left every directory at 0555, and unlink(2)
    // needs a writable *parent*, so the dirs are opened up first.
    if dir.exists() {
        for entry in walkdir::WalkDir::new(&dir).follow_links(false) {
            let entry = entry.with_context(|| format!("scan {}", dir.display()))?;
            if entry.file_type().is_dir() {
                std::fs::set_permissions(
                    entry.path(),
                    std::fs::Permissions::from_mode(0o700),
                )
                .with_context(|| format!("chmod {}", entry.path().display()))?;
            }
        }
        std::fs::remove_dir_all(&dir)
            .with_context(|| format!("clear the stale snapshot {}", dir.display()))?;
    }
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("create assets snapshot {}", dir.display()))?;

    let records = sweep(rec, Some(&dir))?;

    // file modes were set by the copy (0444 / 0555 through the owner-execute
    // bit); directory modes go last — a 0555 parent would block further
    // mkdir(2) — so every scratch dir of the copy ends at exactly 0555
    for entry in walkdir::WalkDir::new(&dir).follow_links(false) {
        let entry = entry
            .with_context(|| format!("walk {}", dir.display()))?;
        if entry.file_type().is_dir() {
            std::fs::set_permissions(entry.path(), std::fs::Permissions::from_mode(0o555))
                .with_context(|| format!("chmod {}", entry.path().display()))?;
        }
    }

    Ok(AssetsStaged { dir, digest: digest_of(&records) })
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// Stream-copy `src -> dst` with a no-follow source open, feeding every
/// byte through `h`; returns the copied size. The copy lands at the
/// normalised mode of §5.3.1: 0444, or 0555 when the source carries the
/// owner-execute bit. mtimes are not preserved: assets never feed
/// `SOURCE_DATE_EPOCH` (§6) and the archive normalises modes itself.
fn copy_hashed(src: &Path, dst: &Path, h: &mut Sha256) -> Result<u64> {
    // the parent of the copy does not exist in the snapshot yet (staging
    // copies whole trees, not per-directory)
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create {}", parent.display()))?;
    }
    let src_md = std::fs::symlink_metadata(src)
        .with_context(|| format!("stat {}", src.display()))?;
    let exec = (src_md.permissions().mode() & 0o100) != 0;
    let mut r = open_nofollow(src)?;
    let mut w = std::fs::File::create(dst)
        .with_context(|| format!("create {}", dst.display()))?;
    let mut buf = [0u8; 64 * 1024];
    let mut size: u64 = 0;
    loop {
        let n = r.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
        std::io::Write::write_all(&mut w, &buf[..n])?;
        size += n as u64;
    }
    std::fs::set_permissions(dst, std::fs::Permissions::from_mode(if exec { 0o555 } else { 0o444 }))
        .with_context(|| format!("chmod {}", dst.display()))?;
    Ok(size)
}

/// Hash the bytes of one file in place (the probe path).
fn hash_file(src: &Path, h: &mut Sha256) -> Result<u64> {
    let mut f = open_nofollow(src)?;
    let mut buf = [0u8; 64 * 1024];
    let mut size: u64 = 0;
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
        size += n as u64;
    }
    Ok(size)
}

// ---------------------------------------------------------------------------
// unit tests: the sweep/digest primitives (the end-to-end flow is covered
// by tests/assets.rs with real `nawa build` runs)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake checkout under `<root>/recipes/<tag>` with the given files and
    /// modes; the recipe dir is returned.
    fn checkout(root: &Path, tag: &str, files: &[(&str, &[u8], u32)]) -> PathBuf {
        let dir = root.join("recipes").join(tag);
        for (rel, data, mode) in files {
            let p = dir.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, data).unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(*mode)).unwrap();
        }
        dir
    }

    /// A recipe through the real parser (so validation ran), pointed at
    /// `dir` the way `Recipe::load` sets it.
    fn recipe_of(dir: &Path, includes: &[&str]) -> Recipe {
        let list: Vec<String> = includes.iter().map(|s| format!("\"{s}\"")).collect();
        let raw = format!(
            r#"
[package]
name = "asset"
version = "1"
rel = 1
arch = ["x86_64"]
source = "local:/nawa-schema-2-requires-a-newer-nawa"
source_date_epoch = 7

[assets]
include = [{}]

[build]
install = "true"
"#,
            list.join(", ")
        );
        let mut rec: Recipe = toml::from_str(&raw).unwrap();
        rec.raw = raw;
        rec.dir = dir.to_path_buf();
        rec
    }

    fn mode_of(p: &Path) -> u32 {
        std::fs::symlink_metadata(p).unwrap().permissions().mode() & 0o7777
    }

    #[test]
    fn snapshot_copies_declared_paths_and_hashes_records() {
        let d = scratch("snap");
        let dir = checkout(
            &d,
            "asset",
            &[
                ("files/patches/0001.patch", b"the patch\n", 0o644),
                ("files/scripts/run.sh", b"#!/bin/sh\n", 0o755),
                ("other-prefix/und.txt", b"not declared\n", 0o644),
            ],
        );
        let rec = recipe_of(&dir, &["files/patches", "files/scripts/run.sh"]);
        let work = d.join("work");
        std::fs::create_dir_all(&work).unwrap();

        let staged = snapshot(&rec, &work).unwrap();
        // embedded layout: relative paths from the recipe root
        assert_eq!(
            std::fs::read(staged.dir.join("files/patches/0001.patch")).unwrap(),
            b"the patch\n"
        );
        assert_eq!(
            std::fs::read(staged.dir.join("files/scripts/run.sh")).unwrap(),
            b"#!/bin/sh\n"
        );
        // undeclared neighbours are not visible
        assert!(!staged.dir.join("other-prefix").exists(), "undeclared sibling");
        // read-only: dirs 0555, files 0444 / 0555 by the exec bit
        assert_eq!(mode_of(&staged.dir), 0o555);
        assert_eq!(mode_of(&staged.dir.join("files")), 0o555);
        assert_eq!(mode_of(&staged.dir.join("files/patches/0001.patch")), 0o444);
        assert_eq!(mode_of(&staged.dir.join("files/scripts/run.sh")), 0o555);

        // the digest is stable across re-snapshots
        let again = snapshot(&rec, &work).unwrap();
        assert_eq!(staged.digest, again.digest);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn digest_uses_only_the_owner_execute_bit() {
        let d = scratch("modes");
        // the two checkouts differ in every mode bit except the owner-exec
        let a = checkout(&d, "a", &[("f", b"one\n", 0o644), ("x.sh", b"run\n", 0o755)]);
        let b = checkout(&d, "b", &[("f", b"one\n", 0o666), ("x.sh", b"run\n", 0o777)]);
        let rec_a = recipe_of(&a, &["f", "x.sh"]);
        let rec_b = recipe_of(&b, &["f", "x.sh"]);
        // synced tar mode vs git checkout: same digest
        assert_eq!(probe_digest(&rec_a).unwrap(), probe_digest(&rec_b).unwrap());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn changing_the_bytes_changes_the_digest() {
        let d = scratch("sbin");
        let dir = checkout(&d, "asset", &[("files/f", b"one\n", 0o644)]);
        let rec = recipe_of(&dir, &["files"]);
        let first = probe_digest(&rec).unwrap();
        std::fs::write(dir.join("files/f"), b"two now\n").unwrap();
        assert_ne!(probe_digest(&rec).unwrap(), first, "new bytes, new digest");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn probe_and_snapshot_agree() {
        let d = scratch("agree");
        let dir = checkout(
            &d,
            "asset",
            &[
                ("files/patches/a.patch", b"a\n", 0o644),
                ("files/patches/b.patch", b"b\n", 0o755),
            ],
        );
        let rec = recipe_of(&dir, &["files"]);
        let work = d.join("work");
        std::fs::create_dir_all(&work).unwrap();
        assert_eq!(probe_digest(&rec).unwrap(), snapshot(&rec, &work).unwrap().digest);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn empty_directories_are_skipped() {
        let d = scratch("emptyd");
        let dir = checkout(&d, "asset", &[("files/keep/f", b"x\n", 0o644)]);
        // a second, truly empty directory inside the declared tree
        std::fs::create_dir_all(dir.join("files/void")).unwrap();
        let rec = recipe_of(&dir, &["files"]);
        let staged = snapshot(&rec, &d.join("work")).unwrap();
        assert!(!staged.dir.join("files/void").exists());
        assert!(staged.dir.join("files/keep/f").is_file());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn missing_include_and_symlinks_fail() {
        let d = scratch("badpath");
        let dir = checkout(&d, "asset", &[("files/f", b"x\n", 0o644)]);
        std::os::unix::fs::symlink("f", dir.join("files/bad")).unwrap();

        // a missing include root
        let missing = recipe_of(&dir, &["files/nowhere"]);
        let err = precheck(&missing).unwrap_err().to_string();
        assert!(err.contains("scan asset"), "got: {err}");
        // a symlink inside the declared tree
        let tree = recipe_of(&dir, &["files"]);
        assert!(precheck(&tree).is_err(), "symlink refused");
        // and an entirely symlinked include path
        std::os::unix::fs::symlink("files", dir.join("linkout")).unwrap();
        let root = recipe_of(&dir, &["linkout"]);
        assert!(precheck(&root).is_err(), "a symlinked include root is refused");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn special_files_and_recipe_toml_are_refused() {
        let d = scratch("weird");
        let dir = checkout(&d, "asset", &[("files/f", b"x\n", 0o644)]);
        let rec = recipe_of(&dir, &["files"]);

        // FIFO inside the declared tree
        let fifo = dir.join("files/fifo");
        {
            let c = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
            let r = unsafe { libc::mkfifo(c.as_ptr(), 0o644) };
            assert_eq!(r, 0, "mkfifo fixture");
        }
        assert!(precheck(&rec).is_err(), "FIFO refused");
        std::fs::remove_file(&fifo).unwrap();

        // recipe.toml inside the declared tree
        std::fs::write(dir.join("files/recipe.toml"), b"[package]\nname = \"x\"\n").unwrap();
        assert!(precheck(&rec).is_err(), "nested recipe.toml refused");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn folded_includes_are_rejected() {
        let d = scratch("dupin");
        let dir = checkout(&d, "asset", &[("f/a", b"x\n", 0o644), ("f/b", b"y\n", 0o644)]);
        // `f` and `f/b` cover the same file in one sweep
        let rec = recipe_of(&dir, &["f", "f/b"]);
        assert!(
            precheck(&rec).unwrap_err().to_string().contains("twice"),
            "folded include refused"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn record_order_is_byte_wise_and_repeatable() {
        let d = scratch("order");
        let dir = checkout(
            &d,
            "asset",
            &[
                ("files/A", b"a\n", 0o644),
                ("files/a", b"b\n", 0o644),
                ("files/patch/x", b"c\n", 0o444),
            ],
        );
        let rec = recipe_of(&dir, &["files"]);
        assert_eq!(probe_digest(&rec).unwrap(), probe_digest(&rec).unwrap());
        let _ = std::fs::remove_dir_all(&d);
    }

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("nawa-assets-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }
}
