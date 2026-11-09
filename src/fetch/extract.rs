//! Source tarball extraction (replaces external `tar`).

use anyhow::{bail, Context, Result};
use std::io::{BufWriter, Read, Seek};
use std::path::{Component, Path, PathBuf};

/// Supported source archive formats, detected by magic bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceFormat {
    Tar,
    TarGz,
    TarBz2,
    TarXz,
    TarZst,
}

/// Recognise a source archive by content, not by file name: read the first
/// block, look for a tar header directly (uncompressed tar) or behind the
/// gzip/xz/bzip2/zstd magic. Returns `None` for everything else — a bare
/// `.gz` of a single file is *not* an archive and stays one (see build.rs).
pub fn detect_source_format(path: &Path) -> Option<SourceFormat> {
    let mut f = std::fs::File::open(path).ok()?;
    let mut head = [0u8; 512];
    let n = read_up_to(&mut f, &mut head)?;
    if n >= 512 && is_tar_header(&head) {
        return Some(SourceFormat::Tar);
    }
    let magic = &head[..n.min(6)];
    f.rewind().ok()?;
    if magic.starts_with(&[0xfd, 0x37, 0x7a, 0x58, 0x5a, 0x00]) {
        // XzReader is a plain streaming `Read`, so pulling one 512-byte block
        // decodes only the first LZMA2 chunk. A decode error before any tar
        // header (unsupported check type, corrupt stream, ...) still claims
        // the format, so extraction reports the real error; answering `None`
        // would make build.rs stage the .xz as a single-file source and the
        // build would fail far away with "./configure: No such file".
        let mut dec = lzma_rust2::XzReader::new(std::io::BufReader::new(f), true);
        return match read_up_to(&mut dec, &mut head) {
            Some(n) => (n >= 512 && is_tar_header(&head)).then_some(SourceFormat::TarXz),
            None => Some(SourceFormat::TarXz),
        };
    }
    // the pull-based decoders only consume as much input as needed to fill
    // the 512-byte header request — no artificial input cap is required
    // (bzip2 in particular *errors* when its input ends mid-stream, so a
    // Take()-bounded prefix would never work here)
    if magic.starts_with(&[0x1f, 0x8b]) {
        looks_like_tar(&mut flate2::read::GzDecoder::new(f))
            .then_some(SourceFormat::TarGz)
    } else if magic.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
        match zstd::stream::read::Decoder::new(f) {
            Ok(mut dec) => looks_like_tar(&mut dec).then_some(SourceFormat::TarZst),
            Err(_) => None,
        }
    } else if magic.starts_with(b"BZh") && matches!(magic.get(3), Some(c) if c.is_ascii_digit()) {
        looks_like_tar(&mut bzip2::read::BzDecoder::new(f))
            .then_some(SourceFormat::TarBz2)
    } else {
        None
    }
}

fn read_up_to(f: &mut dyn Read, buf: &mut [u8]) -> Option<usize> {
    let mut got = 0;
    while got < buf.len() {
        match f.read(&mut buf[got..]) {
            Ok(0) => break,
            Ok(n) => got += n,
            Err(_) => return None,
        }
    }
    Some(got)
}

/// Does this (decompressed) stream begin with a tar header?
fn looks_like_tar(reader: &mut dyn Read) -> bool {
    let mut block = [0u8; 512];
    let mut got = 0;
    while got < 512 {
        match reader.read(&mut block[got..]) {
            Ok(0) => break,
            Ok(k) => got += k,
            Err(_) => return false,
        }
    }
    got >= 512 && is_tar_header(&block)
}

/// ustar/GNU magic at offset 257; pre-POSIX v7 headers have no magic, so
/// fall back to validating the header checksum (checksum field read as
/// spaces, like every tar implementation does).
fn is_tar_header(b: &[u8; 512]) -> bool {
    if &b[257..262] == b"ustar" {
        return true;
    }
    let stored = match octal_field(&b[148..156]) {
        Some(v) => v,
        None => return false,
    };
    let sum: u64 = b
        .iter()
        .enumerate()
        .filter(|(i, _)| !(148..156).contains(i))
        .map(|(_, v)| *v as u64)
        .sum::<u64>()
        + 8 * b' ' as u64;
    stored == sum
}

fn octal_field(field: &[u8]) -> Option<u64> {
    let s: String = field
        .iter()
        .filter(|c| **c != 0)
        .map(|c| *c as char)
        .collect();
    u64::from_str_radix(s.trim().trim_matches(' '), 8).ok()
}

/// Extract a source tarball (tar / tar.gz / tar.bz2 / tar.xz / tar.zst) into
/// `dest`, replacing the external `tar --no-same-owner -xf` call.
///
/// Behaviour parity with GNU tar for the properties that matter to builds:
///   * ownership is never restored (files belong to the invoking user —
///     that is what `--no-same-owner` bought us) and suid bits are dropped;
///   * mtimes are preserved;
///   * absolute entry paths are rooted at `dest` and `..` components are
///     refused, so a tampered tarball cannot escape the staging directory;
///   * hard links are created even when their target appears later in the
///     archive (GNU tar is order independent here), degrading to a copy.
pub fn extract_source_archive(tarball: &Path, dest: &Path) -> Result<()> {
    let fmt = detect_source_format(tarball)
        .with_context(|| format!("{} is not a recognized tar archive", tarball.display()))?;
    std::fs::create_dir_all(dest).with_context(|| format!("create {}", dest.display()))?;
    unpack_tar_stream(decompressed_reader(tarball, fmt)?, dest)
}

/// Shared decompression switch of [`extract_source_archive`] /
/// [`extract_source_archive_stripped`]: open `tarball`, wrap it in the
/// streaming decoder `fmt` was detected with.
fn decompressed_reader(tarball: &Path, fmt: SourceFormat) -> Result<Box<dyn Read>> {
    let f = std::fs::File::open(tarball).with_context(|| format!("open {}", tarball.display()))?;
    Ok(match fmt {
        SourceFormat::Tar => Box::new(f),
        SourceFormat::TarGz => Box::new(flate2::read::GzDecoder::new(f)),
        SourceFormat::TarBz2 => Box::new(bzip2::read::BzDecoder::new(f)),
        SourceFormat::TarXz => {
            Box::new(lzma_rust2::XzReader::new(std::io::BufReader::new(f), true))
        }
        SourceFormat::TarZst => Box::new(zstd::stream::read::Decoder::new(f)?),
    })
}

/// Extract a source archive into `dest` with `strip_components` leading path
/// components removed first (fetch-phase.md §4.2).
///
/// * `strip_components = 0` is exactly [`extract_source_archive`].
/// * Otherwise the (decompressed) archive is re-emitted into a plain tar
///   sibling with the original headers — types, modes, ownership fields and
///   mtimes are copied — so the hardened [`unpack_tar_stream`] path runs on
///   it afterwards and every sanitisation, symlink and deferred-hard-link
///   behaviour is reused unchanged.
/// * Strip accounting follows GNU tar's extraction semantics: `.` segments
///   count as a component and are consumed by the strip count; entries with
///   fewer components are skipped; hard-link *targets* are stripped too and
///   a link whose target strip comes out empty is skipped (GNU errors there;
///   nothing sensible can be materialised); symlink targets pass through.
/// * Unlike the legacy listing path this is strict: an entry refusing to be
///   sanitised (`..`, absolute) aborts the staging instead of being skipped
///   with a warning — §16 expects traversal attempts rejected for
///   new-schema sources.
pub fn extract_source_archive_stripped(
    tarball: &Path,
    dest: &Path,
    strip_components: u32,
) -> Result<()> {
    if strip_components == 0 {
        return extract_source_archive(tarball, dest);
    }
    let fmt = detect_source_format(tarball)
        .with_context(|| format!("{} is not a recognized tar archive", tarball.display()))?;
    std::fs::create_dir_all(dest).with_context(|| format!("create {}", dest.display()))?;
    // plain tar written next to the destination (same `work` tree, so same
    // filesystem and no cross-device dance); it is transient
    let tmp = dest.with_file_name(format!(
        "{}.rewrite.tar",
        dest.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
    ));
    let rewrite = rewrite_tar_stripped(decompressed_reader(tarball, fmt)?, &tmp, strip_components);
    let extracted = rewrite.and_then(|()| {
        let f = std::fs::File::open(&tmp)
            .with_context(|| format!("open rewritten tar {}", tmp.display()))?;
        unpack_tar_stream(f, dest)
    });
    let _ = std::fs::remove_file(&tmp);
    extracted
}

/// Rewrite a (decompressed) tar stream into a plain tar at `tmp`, removing
/// `strip_components` leading components from every entry name and from
/// hard-link targets. Returns when the archive is complete.
///
/// `..` components (entry *names*; GNU/tar-rs would skip those entries on
/// extraction) abort with a hard error for new-schema sources (§16:
/// "traversal paths … are rejected"). Symlink targets are kept verbatim —
/// GNU tar does not translate them either, and extraction stores them
/// exactly as written. Skipped entries are deliberately left unread: the
/// `entries()` iterator seeks to the next header itself.
fn rewrite_tar_stripped<R: Read>(reader: R, tmp: &Path, strip: u32) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let mut ar = tar::Archive::new(reader);
    let file = std::fs::File::create(tmp)
        .with_context(|| format!("create rewritten tar {}", tmp.display()))?;
    let mut out = tar::Builder::new(BufWriter::with_capacity(256 * 1024, file));
    for entry in ar.entries().context("read tar entries")? {
        let mut entry = entry.context("read tar entry")?;
        let t = entry.header().entry_type();
        if t.is_gnu_longname() || t.is_gnu_longlink() {
            // extension headers are composed into the next entry's path by
            // tar-rs; they must not be re-emitted as visible entries
            continue;
        }
        // component split over raw bytes (tar paths need not be UTF-8);
        // empty segments from `//` or a trailing `/` do not count as
        // components, matching GNU's notion
        let path = entry.path()?;
        let comps: Vec<&std::ffi::OsStr> = path
            .as_os_str()
            .as_bytes()
            .split(|b| *b == b'/')
            .filter(|c| !c.is_empty())
            .map(std::ffi::OsStr::from_bytes)
            .collect();
        // strictness first: for new-schema sources a traversal attempt
        // aborts the staging instead of the legacy skip-with-warning
        if comps.iter().any(|c| *c == "..") {
            bail!("unsafe path in source archive: `{}`", path.display());
        }
        if comps.len() < strip as usize {
            // GNU tar semantics: not enough components — the entry is not
            // extracted at all
            continue;
        }
        let stripped: PathBuf = comps[strip as usize..]
            .iter()
            .fold(PathBuf::new(), |mut p, c| {
                p.push(c);
                p
            });
        if stripped.as_os_str().is_empty() {
            continue;
        }
        let mut header = entry.header().clone();
        // A ustar header can split a long path across `prefix` + `name`.
        // `set_path` (inside `append_data`) only rewrites `name` when the
        // stripped path fits in 100 bytes and leaves the stale `prefix`
        // behind, so readers would see `<prefix>/<stripped path>` and the
        // file would land in the wrong place. Drop the old prefix first.
        if let Some(ustar) = header.as_ustar_mut() {
            ustar.prefix = [0u8; 155];
        }
        if t.is_hard_link() {
            let target = entry
                .link_name()
                .context("read hard-link entry")?
                .context("hard link entry without a target")?;
            let tcomps: Vec<&std::ffi::OsStr> = target
                .as_os_str()
                .as_bytes()
                .split(|b| *b == b'/')
                .filter(|c| !c.is_empty())
                .map(std::ffi::OsStr::from_bytes)
                .collect();
            if tcomps.iter().any(|c| *c == "..") {
                bail!(
                    "unsafe hard-link target in source archive: `{}`",
                    target.display()
                );
            }
            if tcomps.len() < strip as usize {
                // nothing left to link to any more; GNU reports an error for
                // the empty target — skipping is the fail-closed variant
                continue;
            }
            let ttarget: PathBuf = tcomps[strip as usize..]
                .iter()
                .fold(PathBuf::new(), |mut p, c| {
                    p.push(c);
                    p
                });
            if ttarget.as_os_str().is_empty() {
                // exactly at the strip boundary: GNU errors on the emptied
                // target; skipping keeps staging deterministic (fail-closed)
                continue;
            }
            out.append_link(&mut header, &stripped, &ttarget)
                .with_context(|| {
                    format!("re-emit hard link {} -> {}", stripped.display(), ttarget.display())
                })?;
        } else if t.is_dir() || t.is_symlink() {
            // directories and symlinks carry no body in the extraction
            // model; some writers leave junk in the size field, so normalise
            header.set_size(0);
            out.append_data(&mut header, &stripped, std::io::empty())
                .with_context(|| format!("re-emit {}", stripped.display()))?;
        } else {
            // regular/contiguous/special: data is appended as-is, exactly
            // what the plain extraction path would read for such entries
            out.append_data(&mut header, &stripped, &mut entry)
                .with_context(|| format!("re-emit {}", stripped.display()))?;
        }
    }
    out.finish().context("finish rewritten tar")?;
    Ok(())
}

fn unpack_tar_stream<R: Read>(reader: R, dest: &Path) -> Result<()> {
    let mut ar = tar::Archive::new(reader);
    ar.set_overwrite(true);
    // deferred hard links: (link path, target path) — created after the
    // stream, in case the target shows up later in the archive
    let mut links: Vec<(PathBuf, PathBuf)> = Vec::new();
    for entry in ar.entries().context("read tar entries")? {
        let mut entry = entry.context("read tar entry")?;
        let t = entry.header().entry_type();
        let p = entry.path()?.to_path_buf();
        if t.is_hard_link() {
            let target = entry
                .link_name()?
                .context("hard link entry without a target")?
                .to_path_buf();
            links.push((sanitize_entry_path(&p)?, sanitize_entry_path(&target)?));
            continue;
        }
        // unpack_in roots absolute paths at `dest` and skips `..` entries
        // (GNU/bsdtar semantics, hardened by symlink validation); a `false`
        // return means the entry was skipped for exactly that reason
        if !entry
            .unpack_in(dest)
            .with_context(|| format!("unpack {}", p.display()))?
        {
            crate::nawa_warn!("skipped unsafe path in source archive: {}", p.display());
        }
    }
    for (link, target) in links {
        if link.as_os_str().is_empty() || target.as_os_str().is_empty() {
            bail!("unsafe hard link path in source archive");
        }
        let from = dest.join(&target);
        let to = dest.join(&link);
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
        }
        // order-independent hard links; a missing target or an exotic
        // filesystem degrades to a plain copy, like bsdtar -H does not but
        // every build system expects to survive
        if std::fs::hard_link(&from, &to).is_err() {
            std::fs::copy(&from, &to)
                .with_context(|| format!("materialize hard link {} -> {}", from.display(), to.display()))?;
        }
    }
    Ok(())
}

/// GNU-tar-style path sanitisation for entries handled outside unpack_in:
/// strip leading `/`, refuse any `..` component.
fn sanitize_entry_path(p: &Path) -> Result<PathBuf> {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::Prefix(_) | Component::RootDir | Component::CurDir => {}
            Component::ParentDir => bail!("unsafe path in source archive: {}", p.display()),
            Component::Normal(n) => out.push(n),
        }
    }
    Ok(out)
}

