//! Debug tool: create a small fake `.nawa.tar.zst` archive for testing the
//! CLI end-to-end without building a real package.
//!
//! Usage:
//!   mkfake <name> <version> <rel> <out.nawa.tar.zst> [path=content ...]
//!
//! Example:
//!   mkfake demo 1.0 1 demo-1.0-1.nawa.tar.zst usr/bin/demo=v1 'etc/demo.conf=config v1'

use anyhow::{bail, Result};

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 4 {
        bail!("usage: mkfake <name> <version> <rel> <out.nawa.tar.zst> [path=content ...]");
    }
    let (name, version, rel, out) = (&args[0], &args[1], &args[2], &args[3]);
    let pkginfo = format!(
        "pkgname = {name}\npkgver = {version}\npkgrel = {rel}\narch = x86_64\nrundeps = \nsize = 1\n"
    );

    let f = std::fs::File::create(out)?;
    let enc = zstd::Encoder::new(f, 3)?;
    let mut tar = tar::Builder::new(enc);

    let mut hdr = tar::Header::new_gnu();
    hdr.set_size(pkginfo.len() as u64);
    hdr.set_mode(0o644);
    hdr.set_cksum();
    tar.append_data(&mut hdr, ".PKGINFO", pkginfo.as_bytes())?;

    for spec in &args[4..] {
        let (path, content) = match spec.split_once('=') {
            Some(kv) => kv,
            None => bail!("spec `{spec}` must be path=content"),
        };
        let data = content.as_bytes();
        let mut hdr = tar::Header::new_gnu();
        hdr.set_size(data.len() as u64);
        hdr.set_mode(0o755);
        hdr.set_cksum();
        tar.append_data(&mut hdr, format!("payload/{path}"), data)?;
    }

    let enc = tar.into_inner()?;
    enc.finish()?;
    eprintln!("mkfake: wrote {out}");
    Ok(())
}
