//! ed25519 signing & trust management.
//!
//! Signature scheme (nawa-sig-v1):
//! * the signed message is the ASCII string `nawa-v1:sha256:<hex>` where
//!   `<hex>` is the sha256 of the payload file — one verification covers
//!   both integrity (hash) and authenticity (signature)
//! * signatures live in detached files next to the payload:
//!   `foo.nawa.tar.zst` -> `foo.nawa.tar.zst.sig`
//! * key files hold raw hex: `nawa.sec` = 64 hex chars (32-byte seed),
//!   `nawa.pub` = 64 hex chars (compressed verifier point)
//! * trusted public keys live in `<root>/etc/nawa/trusted-keys/*.pub`
//! * the signature file embeds the signer's public key (informational,
//!   and used as a fallback integrity pin when no trust is configured)
//!
//! Verification policy (enforced by `verify_for_install`):
//! 1. signature present + trusted keys configured
//!    -> must match one of the trusted keys, else refuse
//! 2. signature present + no trusted keys
//!    -> verify against the embedded key (integrity pinning) + warn loudly
//! 3. no signature + trusted keys configured
//!    -> refuse unless --allow-unsigned
//! 4. no signature + no trusted keys -> pass with a note (bootstrap mode)

use anyhow::{bail, Context, Result};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use std::io::Read;
use std::path::{Path, PathBuf};

pub const SEC_NAME: &str = "nawa.sec";
pub const PUB_NAME: &str = "nawa.pub";
pub const SIG_SUFFIX: &str = ".sig";
pub const DOMAIN: &str = "nawa-v1:sha256:";

/// Where signing keys live. `$NAWA_KEYS_DIR` wins, else `/etc/nawa/keys`
/// when creatable (root / writable system), else `~/.config/nawa/keys`.
pub fn default_key_dir() -> PathBuf {
    if let Ok(d) = std::env::var("NAWA_KEYS_DIR") {
        if !d.is_empty() {
            return PathBuf::from(d);
        }
    }
    let etc = PathBuf::from("/etc/nawa/keys");
    if std::fs::create_dir_all(&etc).is_ok() {
        return etc;
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home).join(".config").join("nawa").join("keys")
}

/// Resolve the signing key: `--key` > `$NAWA_SIGN_KEY` > `$NAWA_KEYS_DIR` >
/// `/etc/nawa/keys/nawa.sec` > `~/.config/nawa/keys/nawa.sec`.
pub fn resolve_signing_key(explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(p) = explicit {
        if !p.exists() {
            bail!("signing key not found: {}", p.display());
        }
        return Ok(p.to_path_buf());
    }
    if let Ok(p) = std::env::var("NAWA_SIGN_KEY") {
        if !p.is_empty() && PathBuf::from(&p).exists() {
            return Ok(PathBuf::from(p));
        }
    }
    let candidates = [
        default_key_dir().join(SEC_NAME),
        PathBuf::from("/etc/nawa/keys").join(SEC_NAME),
    ];
    for c in &candidates {
        if c.exists() {
            return Ok(c.clone());
        }
    }
    bail!(
        "no signing key found — run `nawa keygen` or pass --key <nawa.sec>"
    )
}

/// Generate a new ed25519 keypair into `dir` (`nawa.sec` 0600 + `nawa.pub` 0644).
pub fn keygen(dir: &Path) -> Result<(PathBuf, PathBuf)> {
    std::fs::create_dir_all(dir)
        .with_context(|| format!("create key dir {}", dir.display()))?;
    let sec_path = dir.join(SEC_NAME);
    let pub_path = dir.join(PUB_NAME);
    if sec_path.exists() {
        bail!(
            "refusing to overwrite existing key {} (move it away first)",
            sec_path.display()
        );
    }
    let seed = random_32()?;
    let sk = SigningKey::from_bytes(&seed);
    // create the secret key with 0600 from the start (no window where it is
    // world-readable) and never clobber an existing file
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&sec_path)
            .with_context(|| format!("create {}", sec_path.display()))?;
        f.write_all((hex::encode(seed) + "\n").as_bytes())?;
    }
    std::fs::write(&pub_path, hex::encode(sk.verifying_key().to_bytes()) + "\n")?;
    Ok((sec_path, pub_path))
}

pub fn load_signing_key(path: &Path) -> Result<SigningKey> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("read signing key {}", path.display()))?;
    let seed = decode_hex32(raw.lines().next().unwrap_or_default())
        .with_context(|| format!("bad key hex in {}", path.display()))?;
    Ok(SigningKey::from_bytes(&seed))
}

pub fn load_verify_key(path: &Path) -> Result<VerifyingKey> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("read public key {}", path.display()))?;
    parse_pub_hex(&raw)
        .with_context(|| format!("bad public key {}", path.display()))
}

/// Sign `file` (hash + sign) and write `<file>.sig`. Returns the sig path.
pub fn sign(file: &Path, key_path: &Path) -> Result<PathBuf> {
    let sk = load_signing_key(key_path)?;
    let hash = crate::fetch::sha256_of_file(file)?;
    let msg = format!("{DOMAIN}{hash}");
    let sig = sk.sign(msg.as_bytes());
    let sig_path = sig_path_for(file);
    let body = format!(
        "untrusted comment: nawa signature for {}\n{}\n{}\n",
        file.file_name().unwrap_or_default().to_string_lossy(),
        hex::encode(sig.to_bytes()),
        hex::encode(sk.verifying_key().to_bytes()),
    );
    std::fs::write(&sig_path, &body)
        .with_context(|| format!("write {}", sig_path.display()))?;
    Ok(sig_path)
}

pub fn sig_path_for(file: &Path) -> PathBuf {
    let mut s = file.as_os_str().to_os_string();
    s.push(SIG_SUFFIX);
    PathBuf::from(s)
}

pub struct SigFile {
    pub sig: Signature,
    pub embedded: VerifyingKey,
    #[allow(dead_code)]
    pub comment: String,
}

/// Parse a 3-line nawa signature file (comment / sig-hex / pubkey-hex).
pub fn read_sig_file(path: &Path) -> Result<SigFile> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("read signature {}", path.display()))?;
    let mut lines = raw.lines();
    let comment = lines.next().unwrap_or_default().to_string();
    let sig_hex = lines.next().unwrap_or_default().trim();
    let pub_hex = lines.next().unwrap_or_default().trim();
    let sig_bytes = decode_hex64(sig_hex).context("bad signature hex")?;
    let embedded = match VerifyingKey::from_bytes(&decode_hex32(pub_hex).context("bad embedded key hex")?) {
        Ok(vk) => vk,
        Err(_) => bail!("embedded key is not a valid ed25519 point"),
    };
    let sig = match Signature::from_slice(&sig_bytes) {
        Ok(s) => s,
        Err(_) => bail!("truncated signature"),
    };
    Ok(SigFile {
        sig,
        embedded,
        comment,
    })
}

/// Verify `file` against one key. Recomputes sha256 (integrity + authenticity).
pub fn verify(file: &Path, key: &VerifyingKey, sig: &Signature) -> Result<()> {
    let hash = crate::fetch::sha256_of_file(file)?;
    let msg = format!("{DOMAIN}{hash}");
    key.verify_strict(msg.as_bytes(), sig)
        .map_err(|_| anyhow::anyhow!("signature mismatch"))
}

/// Trust-store directory for a target root.
pub fn trust_dir(root: &Path) -> PathBuf {
    root.join("etc/nawa/trusted-keys")
}

/// Copy a public key into the trust store (validated first).
pub fn trust(pubfile: &Path, root: &Path) -> Result<PathBuf> {
    load_verify_key(pubfile)?; // validate
    let dir = trust_dir(root);
    std::fs::create_dir_all(&dir)?;
    let mut name = pubfile
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "key.pub".into());
    if !name.ends_with(".pub") {
        name.push_str(".pub");
    }
    let dest = dir.join(name);
    std::fs::copy(pubfile, &dest)
        .with_context(|| format!("copy into trust store {}", dest.display()))?;
    Ok(dest)
}

/// All trusted public keys for `root` as (key-file-name, key) pairs.
pub fn trusted_keys(root: &Path) -> Result<Vec<(String, VerifyingKey)>> {
    let dir = trust_dir(root);
    let mut out = Vec::new();
    if !dir.is_dir() {
        return Ok(out);
    }
    let mut paths: Vec<_> = std::fs::read_dir(&dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().map(|x| x == "pub").unwrap_or(false))
        .collect();
    paths.sort();
    for p in paths {
        let name = p.file_name().unwrap_or_default().to_string_lossy().to_string();
        match load_verify_key(&p) {
            Ok(vk) => out.push((name, vk)),
            Err(e) => eprintln!("nawa: warning: skipping unparseable key {name}: {e:#}"),
        }
    }
    Ok(out)
}

/// Policy check before installing `archive` into `root`.
/// `key` = explicit `--key` override; `allow_unsigned` = --allow-unsigned.
pub fn verify_for_install(
    archive: &Path,
    root: &Path,
    key: Option<&Path>,
    allow_unsigned: bool,
) -> Result<()> {
    let sig_path = sig_path_for(archive);
    if !sig_path.exists() {
        let trusted = trusted_keys(root)?;
        if !trusted.is_empty() && !allow_unsigned {
            bail!(
                "{} is unsigned but {} trusted key(s) are configured\n(produce a signature with `nawa sign` or pass --allow-unsigned)",
                archive.file_name().unwrap_or_default().to_string_lossy(),
                trusted.len()
            );
        }
        if trusted.is_empty() {
            // bootstrap mode stays permitted, but never silently
            eprintln!(
                "nawa: note: {} is unsigned and no trusted keys are configured — installing unauthenticated (bootstrap mode)",
                archive.file_name().unwrap_or_default().to_string_lossy()
            );
        }
        return Ok(());
    }

    let sf = read_sig_file(&sig_path)?;
    if let Some(kp) = key {
        let vk = load_verify_key(kp)?;
        verify(archive, &vk, &sf.sig)?;
        println!("nawa: signature OK (explicit key {})", kp.display());
        return Ok(());
    }

    let trusted = trusted_keys(root)?;
    if !trusted.is_empty() {
        for (name, vk) in &trusted {
            if verify(archive, vk, &sf.sig).is_ok() {
                println!("nawa: signature OK (trusted key {name})");
                return Ok(());
            }
        }
        bail!(
            "signature on {} does not match any of the {} trusted key(s)",
            archive.file_name().unwrap_or_default().to_string_lossy(),
            trusted.len()
        );
    }

    // no trust configured: pin integrity to the embedded signer key
    verify(archive, &sf.embedded, &sf.sig)?;
    eprintln!(
        "nawa: warning: no trusted keys configured — verified against the key embedded in {} only",
        sig_path.file_name().unwrap_or_default().to_string_lossy()
    );
    Ok(())
}

/// `nawa verify` standalone: explicit key > trust store > embedded key.
pub fn verify_standalone(file: &Path, root: &Path, key: Option<&Path>) -> Result<()> {
    if !file.exists() {
        bail!("file not found: {}", file.display());
    }
    let sig_path = sig_path_for(file);
    if !sig_path.exists() {
        bail!("no signature file {} (run `nawa sign {}`)", sig_path.display(), file.display());
    }
    let sf = read_sig_file(&sig_path)?;
    if let Some(kp) = key {
        let vk = load_verify_key(kp)?;
        verify(file, &vk, &sf.sig)?;
        println!("{}: signature OK (explicit key {})", file.display(), kp.display());
        return Ok(());
    }
    for (name, vk) in trusted_keys(root)? {
        if verify(file, &vk, &sf.sig).is_ok() {
            println!("{}: signature OK (trusted key {})", file.display(), name);
            return Ok(());
        }
    }
    if trusted_keys(root)?.is_empty() {
        verify(file, &sf.embedded, &sf.sig)?;
        eprintln!("nawa: warning: no trusted keys configured — verified against embedded key");
        println!("{}: signature OK (embedded key)", file.display());
        return Ok(());
    }
    bail!("signature does not match any trusted key")
}

fn parse_pub_hex(s: &str) -> Result<VerifyingKey> {
    let line = s
        .lines()
        .map(|l| l.trim())
        .find(|l| !l.is_empty() && !l.starts_with('#'))
        .unwrap_or_default();
    let bytes = decode_hex32(line)?;
    match VerifyingKey::from_bytes(&bytes) {
        Ok(vk) => Ok(vk),
        Err(_) => bail!("invalid ed25519 public key"),
    }
}

fn random_32() -> Result<[u8; 32]> {
    let mut buf = [0u8; 32];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut buf)?;
    Ok(buf)
}

fn decode_hex32(s: &str) -> Result<[u8; 32]> {
    let v = hex::decode(s.trim())?;
    if v.len() != 32 {
        bail!("expected 32 bytes, got {}", v.len());
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&v);
    Ok(out)
}

fn decode_hex64(s: &str) -> Result<[u8; 64]> {
    let v = hex::decode(s.trim())?;
    if v.len() != 64 {
        bail!("expected 64 bytes, got {}", v.len());
    }
    let mut out = [0u8; 64];
    out.copy_from_slice(&v);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("nawa-sign-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn fake_archive(dir: &Path, name: &str, body: &[u8]) -> PathBuf {
        let p = dir.join(format!("{name}.nawa.tar.zst"));
        std::fs::write(&p, body).unwrap();
        p
    }

    #[test]
    fn keygen_sign_verify_roundtrip() {
        let base = tmpdir("rt");
        let keys = base.join("keys");
        let (sec, pub_) = keygen(&keys).unwrap();
        assert!(sec.exists() && pub_.exists());

        let archive = fake_archive(&base, "legit-1.0-1", b"payload-bytes-v1");
        let sigp = sign(&archive, &sec).unwrap();
        assert_eq!(sigp.to_string_lossy(), format!("{}.sig", archive.display()));

        // explicit key
        verify_standalone(&archive, &base, Some(&pub_)).unwrap();

        // embedded key (no trust store)
        verify_standalone(&archive, &base, None).unwrap();
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn tampered_archive_is_rejected() {
        let base = tmpdir("tamper");
        let (sec, _) = keygen(&base.join("keys")).unwrap();
        let archive = fake_archive(&base, "legit-1.0-1", b"payload-bytes-v1");
        sign(&archive, &sec).unwrap();

        // flip a byte after signing
        std::fs::write(&archive, b"payload-bytes-V1").unwrap();
        let sigp = sig_path_for(&archive);
        let sf = read_sig_file(&sigp).unwrap();
        assert!(verify(&archive, &sf.embedded, &sf.sig).is_err());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn wrong_key_is_rejected() {
        let base = tmpdir("wrongkey");
        let (sec_a, _) = keygen(&base.join("ka")).unwrap();
        let (_, pub_b) = keygen(&base.join("kb")).unwrap();
        let archive = fake_archive(&base, "legit-1.0-1", b"payload-bytes-v1");
        sign(&archive, &sec_a).unwrap();
        assert!(verify_standalone(&archive, &base, Some(&pub_b)).is_err());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn trust_store_policy() {
        let base = tmpdir("trust");
        let (sec, pub_) = keygen(&base.join("keys")).unwrap();
        let archive = fake_archive(&base, "legit-1.0-1", b"payload-bytes-v1");

        // no trust, no sig -> pass (bootstrap mode)
        verify_for_install(&archive, &base, None, false).unwrap();

        // trust configured + unsigned -> refuse, --allow-unsigned passes
        let trusted_pub = trust(&pub_, &base).unwrap();
        assert!(verify_for_install(&archive, &base, None, false).is_err());
        verify_for_install(&archive, &base, None, true).unwrap();

        // signed + trusted -> pass
        sign(&archive, &sec).unwrap();
        verify_for_install(&archive, &base, None, false).unwrap();

        // tampered + trusted -> refuse even with --allow-unsigned
        std::fs::write(&archive, b"EVIL-payload-bytes").unwrap();
        assert!(verify_for_install(&archive, &base, None, true).is_err());

        // restored content + fresh signature -> trusted again
        std::fs::write(&archive, b"payload-bytes-v1").unwrap();
        std::fs::remove_file(sig_path_for(&archive)).unwrap();
        sign(&archive, &sec).unwrap();
        verify_for_install(&archive, &base, None, false).unwrap();

        // a rogue signer (not in the trust store) -> refuse
        let (sec3, _) = keygen(&base.join("k3")).unwrap();
        let rogue = fake_archive(&base, "rogue-1.0-1", b"rogue");
        sign(&rogue, &sec3).unwrap();
        assert!(verify_for_install(&rogue, &base, None, false).is_err());
        assert!(verify_for_install(&rogue, &base, None, true).is_err()); // sig exists: unsigned flag irrelevant

        // explicit --key still works
        verify_for_install(&archive, &base, Some(&trusted_pub), false).unwrap();
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn keygen_refuses_overwrite() {
        let base = tmpdir("nooverwrite");
        keygen(&base).unwrap();
        assert!(keygen(&base).is_err());
        let _ = std::fs::remove_dir_all(&base);
    }
}
