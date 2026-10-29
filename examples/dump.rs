//! Debug helper: raw walk of the tar stream inside a .nawa.tar.zst archive.
//! Usage: cargo run --example dump -- <archive.nawa.tar.zst>

fn main() -> anyhow::Result<()> {
    let path = std::env::args().nth(1).expect("usage: dump <archive>");
    let f = std::fs::File::open(&path)?;
    let tar = zstd::decode_all(f)?;
    println!("decompressed: {} bytes", tar.len());

    let mut off = 0usize;
    let mut n = 0usize;
    while off + 512 <= tar.len() {
        let hdr = &tar[off..off + 512];
        let name_raw = &hdr[0..100];
        let nul = name_raw.iter().position(|&b| b == 0).unwrap_or(100);
        let name = String::from_utf8_lossy(&name_raw[..nul]).to_string();
        let size_s = String::from_utf8_lossy(&hdr[124..136]).to_string();
        let chksum_s = String::from_utf8_lossy(&hdr[148..156]).to_string();
        let magic = String::from_utf8_lossy(&hdr[257..263]).to_string();
        let link_s = String::from_utf8_lossy(&hdr[157..257]).to_string();
        let size = usize::from_str_radix(size_s.trim_end_matches('\0').trim(), 8);
        if size.is_err() || !magic.starts_with("ustar") {
            println!(
                "[{n}] BAD HEADER at off={off}: name={name:?} size={size_s:?} chksum={chksum_s:?} magic={magic:?}"
            );
            println!("  linkname={link_s:?}");
            println!("  raw={:02x?}", &hdr[..80]);
            return Ok(());
        }
        let size = size.unwrap();
        if n < 200 {
            println!("[{n}] off={off} size={size} name={name:?} link={link_s:?}");
        }
        off += 512 + size.div_ceil(512) * 512;
        n += 1;
    }
    println!("clean end: {n} entries, off={off}");
    Ok(())
}
