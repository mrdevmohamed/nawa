//! Checksums: SHA-256 hashing and verification.

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::Path;

// ---- checksums -------------------------------------------------------------

pub(crate) fn sha256_of_file(path: &Path) -> Result<String> {
    let mut f = std::fs::File::open(path)
        .with_context(|| format!("cannot open {} for hashing", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

pub(crate) fn verify_sha256(path: &Path, expected: &str) -> Result<()> {
    if expected.trim().is_empty() {
        bail!("missing sha256 for {} — refusing to build unverified source", path.display());
    }
    let got = sha256_of_file(path)?;
    if got != expected.trim().to_lowercase() {
        bail!(
            "sha256 mismatch for {}:\n  expected: {}\n  got:      {}",
            path.display(),
            expected,
            got
        );
    }
    Ok(())
}
