//! ELF parsing and lint helpers.

use anyhow::{bail, Context, Result};
use std::io::Read;
use std::path::Path;

/// ELF parsing needs the complete file because goblin follows offsets throughout
/// it. Keep that read bounded: package contents are inputs to the linter and
/// must not be able to force an unbounded allocation.
pub(crate) const MAX_ELF_SCAN_MIB: u64 = 512;
pub(crate) const MAX_ELF_SCAN: u64 = MAX_ELF_SCAN_MIB * 1024 * 1024;
pub(crate) const MAX_TEXT_SCAN: usize = 4 * 1024 * 1024;

pub(crate) enum LintRead {
    Data(Vec<u8>),
    ElfTooLarge { size: u64 },
}

pub(crate) fn read_for_lint(path: &Path) -> std::io::Result<LintRead> {
    let mut f = std::fs::File::open(path)?;
    let mut buf = Vec::with_capacity(4);
    (&mut f).take(4).read_to_end(&mut buf)?;

    if buf.as_slice() == b"\x7fELF" {
        // Reject obviously oversized files before reading their payload. Read
        // at most one byte beyond the limit as a second check in case the file
        // grew after metadata() was queried.
        let size = f.metadata()?.len();
        if size > MAX_ELF_SCAN {
            return Ok(LintRead::ElfTooLarge { size });
        }

        let remaining = MAX_ELF_SCAN
            .saturating_sub(buf.len() as u64)
            .saturating_add(1);
        (&mut f).take(remaining).read_to_end(&mut buf)?;
        if buf.len() as u64 > MAX_ELF_SCAN {
            return Ok(LintRead::ElfTooLarge {
                size: buf.len() as u64,
            });
        }
    } else {
        // Text/configuration leak scans stay deliberately small; large assets
        // are not read into memory just to search for build-path strings.
        let remaining = MAX_TEXT_SCAN.saturating_sub(buf.len());
        (&mut f).take(remaining as u64).read_to_end(&mut buf)?;
    }
    Ok(LintRead::Data(buf))
}

pub(crate) fn looks_texty(data: &[u8]) -> bool {
    // a NUL in the first 512 bytes almost always means binary data; ELF is
    // already handled above
    !data.iter().take(512).any(|&b| b == 0)
}

// ---------------------------------------------------------------------------
// ELF reader (goblin): DT_NEEDED / DT_RUNPATH / DT_RPATH / DT_SONAME
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone)]
pub(crate) struct ElfInfo {
    pub needed: Vec<String>,
    pub rpath: Option<String>,
    pub runpath: Option<String>,
    /// DT_SONAME, when the file carries one. The run-dep policy itself keys
    /// on DT_NEEDED only; the field is kept (like the reader it replaced)
    /// so the info stays complete for consumers.
    #[allow(dead_code)]
    pub soname: Option<String>,
    /// PT_INTERP (the dynamic loader path) when the file has one; `revdep`
    /// checks that it exists in the target root.
    pub interp: Option<String>,
    /// ELFCLASS64 / e_machine of the file: `revdep` only accepts libraries
    /// with the same class and machine, like the loader.
    pub is_64: bool,
    pub machine: u16,
}

/// Parse an ELF file's dynamic section. `Ok(None)` for non-dynamic ELFs
/// (relocatable objects, core files). Endianness and class are taken from
/// the file; both ELF32 and ELF64 are supported.
///
/// goblin is bounds-checked throughout: truncated headers, oversized field
/// values and dangling string offsets are reported as errors instead of
/// panics — this function must stay safe to call on adversarial bytes.
pub(crate) fn parse_elf(data: &[u8]) -> Result<Option<ElfInfo>> {
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
        interp: elf.interpreter.map(str::to_string),
        is_64: elf.is_64,
        machine: elf.header.e_machine,
    };
    Ok(Some(info))
}

pub(crate) fn last_non_empty(paths: &[&str]) -> Option<String> {
    paths.iter().rev().find(|p| !p.is_empty()).map(|p| (*p).to_string())
}

