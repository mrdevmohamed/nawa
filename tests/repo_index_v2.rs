//! Integration tests for signed repository index v2 and legacy v1 behavior.
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn nawa_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_nawa"))
}

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("nawa-index-v2-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("root")).unwrap();
    fs::create_dir_all(dir.join("recipes")).unwrap();
    dir
}

fn run(dir: &Path, args: &[&str]) -> Output {
    Command::new(nawa_bin())
        .env_remove("NAWA_CONFIG")
        .env_remove("NAWA_REPO")
        .env("NAWA_SYSCONFDIR", dir.join("no-sysconf"))
        .env("NO_COLOR", "1")
        .current_dir(dir)
        .args(["--recipes", dir.join("recipes").to_str().unwrap()])
        .args(["--builddir", dir.join("build").to_str().unwrap()])
        .args(["--profiles", dir.join("profiles").to_str().unwrap()])
        .args(["--root", dir.join("root").to_str().unwrap()])
        .args(args)
        .output()
        .unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

fn ok(output: Output) -> String {
    assert!(
        output.status.success(),
        "command failed:\n{}\n{}",
        stdout(&output),
        stderr(&output)
    );
    stdout(&output)
}

fn make_archive(repo: &Path) -> PathBuf {
    make_archive_version(repo, "1", "from-v2-repository\n")
}

fn make_archive_version(repo: &Path, version: &str, body: &str) -> PathBuf {
    fs::create_dir_all(repo).unwrap();
    let path = repo.join(format!("hello-{version}-1.nawa.tar.zst"));
    let pkginfo = format!("pkgname = hello\npkgver = {version}\npkgrel = 1\narch = any\nrundeps = \nsize = 1\n");
    let file = fs::File::create(&path).unwrap();
    let encoder = zstd::stream::write::Encoder::new(file, 3).unwrap();
    let mut tar = tar::Builder::new(encoder);

    let mut info_header = tar::Header::new_gnu();
    info_header.set_size(pkginfo.len() as u64);
    info_header.set_mode(0o644);
    info_header.set_cksum();
    tar.append_data(&mut info_header, ".PKGINFO", pkginfo.as_bytes()).unwrap();

    let mut payload_header = tar::Header::new_gnu();
    payload_header.set_size(body.len() as u64);
    payload_header.set_mode(0o644);
    payload_header.set_cksum();
    tar.append_data(
        &mut payload_header,
        "payload/usr/share/hello",
        body.as_bytes(),
    )
    .unwrap();

    tar.into_inner().unwrap().finish().unwrap();
    path
}

struct SigningKeys {
    secret: PathBuf,
    public: PathBuf,
}

fn make_keys(dir: &Path) -> SigningKeys {
    let key_dir = dir.join("publisher-keys");
    ok(run(dir, &["keygen", "--out", key_dir.to_str().unwrap()]));
    SigningKeys {
        secret: key_dir.join("nawa.sec"),
        public: key_dir.join("nawa.pub"),
    }
}

fn trust_publisher(dir: &Path, keys: &SigningKeys) {
    ok(run(dir, &["trust", keys.public.to_str().unwrap()]));
}

fn create_index(dir: &Path, repo: &Path, keys: &SigningKeys, version: &str) {
    let args = vec![
        "repo-create",
        repo.to_str().unwrap(),
        "--index-version",
        version,
        "--sign",
        "--key",
        keys.secret.to_str().unwrap(),
    ];
    // Keep the command arguments owned for stable borrowing.
    let out = run(dir, &args);
    ok(out);
}

fn install_hello(dir: &Path, repo: &Path, extra: &[&str]) -> Output {
    let mut args = vec!["install", "--yes", "--repo", repo.to_str().unwrap()];
    args.extend_from_slice(extra);
    args.push("hello");
    run(dir, &args)
}

#[test]
fn v2_trusted_index_installs_archive_without_detached_package_signature() {
    let dir = scratch("no-package-sig");
    let repo = dir.join("repo");
    make_archive(&repo);
    let keys = make_keys(&dir);
    trust_publisher(&dir, &keys);
    create_index(&dir, &repo, &keys, "2");

    assert!(repo.join("index.toml.sig").is_file());
    assert!(!repo.join("hello-1-1.nawa.tar.zst.sig").exists());

    let output = install_hello(&dir, &repo, &[]);
    let message = ok(output);
    assert!(message.contains("authenticated by trusted index"), "{message}");
    assert_eq!(
        fs::read_to_string(dir.join("root/usr/share/hello")).unwrap(),
        "from-v2-repository\n"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn v2_archive_tampering_is_rejected_even_with_allow_unsigned() {
    let dir = scratch("tampered");
    let repo = dir.join("repo");
    let archive = make_archive(&repo);
    let keys = make_keys(&dir);
    trust_publisher(&dir, &keys);
    create_index(&dir, &repo, &keys, "2");

    let mut bytes = fs::read(&archive).unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 0x5a;
    fs::write(&archive, bytes).unwrap();

    let output = install_hello(&dir, &repo, &["--allow-unsigned"]);
    assert!(!output.status.success(), "tampered archive unexpectedly installed");
    assert!(
        format!("{}{}", stdout(&output), stderr(&output)).contains("sha256 mismatch"),
        "unexpected error:\n{}\n{}",
        stdout(&output),
        stderr(&output)
    );
    assert!(!dir.join("root/usr/share/hello").exists());
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn v1_trusted_index_still_requires_detached_package_signature() {
    let dir = scratch("legacy");
    let repo = dir.join("repo");
    let archive = make_archive(&repo);
    let keys = make_keys(&dir);
    trust_publisher(&dir, &keys);
    create_index(&dir, &repo, &keys, "1");

    let missing = install_hello(&dir, &repo, &[]);
    assert!(!missing.status.success());
    assert!(
        format!("{}{}", stdout(&missing), stderr(&missing)).contains("is unsigned"),
        "v1 should retain the detached signature requirement"
    );

    ok(run(
        &dir,
        &[
            "sign",
            archive.to_str().unwrap(),
            "--key",
            keys.secret.to_str().unwrap(),
        ],
    ));
    ok(install_hello(&dir, &repo, &[]));
    assert_eq!(
        fs::read_to_string(dir.join("root/usr/share/hello")).unwrap(),
        "from-v2-repository\n"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn v2_repo_create_requires_signing() {
    let dir = scratch("requires-sign");
    let repo = dir.join("repo");
    fs::create_dir_all(&repo).unwrap();
    let output = run(&dir, &["repo-create", repo.to_str().unwrap(), "--index-version", "2"]);
    assert!(!output.status.success());
    assert!(
        format!("{}{}", stdout(&output), stderr(&output)).contains("requires --sign")
    );
    assert!(!repo.join("index.toml").exists());
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn v2_present_but_invalid_index_signature_never_downgrades_to_unsigned() {
    let dir = scratch("bad-index-sig");
    let repo = dir.join("repo");
    make_archive(&repo);
    let keys = make_keys(&dir);
    trust_publisher(&dir, &keys);
    create_index(&dir, &repo, &keys, "2");

    let signature = repo.join("index.toml.sig");
    let raw = fs::read_to_string(&signature).unwrap();
    let mut lines: Vec<String> = raw.lines().map(str::to_string).collect();
    assert!(lines.len() >= 3, "expected a three-line nawa signature");
    let first = lines[1].as_bytes()[0] as char;
    lines[1].replace_range(0..1, if first == '0' { "1" } else { "0" });
    fs::write(signature, lines.join("\n") + "\n").unwrap();

    let output = install_hello(&dir, &repo, &["--allow-unsigned"]);
    assert!(!output.status.success());
    assert!(
        format!("{}{}", stdout(&output), stderr(&output)).contains("signature"),
        "invalid signed index must fail, not downgrade"
    );
    assert!(!dir.join("root/usr/share/hello").exists());
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn v2_trusted_index_ignores_a_stray_package_signature_sidecar() {
    let dir = scratch("stray-sidecar");
    let repo = dir.join("repo");
    make_archive(&repo);
    let keys = make_keys(&dir);
    trust_publisher(&dir, &keys);
    create_index(&dir, &repo, &keys, "2");

    // A stale or malformed package signature is not authoritative in v2
    // repository mode; only the trusted index digest authenticates this file.
    fs::write(repo.join("hello-1-1.nawa.tar.zst.sig"), "not a nawa signature").unwrap();
    ok(install_hello(&dir, &repo, &[]));
    assert_eq!(
        fs::read_to_string(dir.join("root/usr/share/hello")).unwrap(),
        "from-v2-repository\n"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn missing_v2_index_signature_with_explicit_override_uses_legacy_archive_policy() {
    let dir = scratch("v2-index-unsigned-fallback");
    let repo = dir.join("repo");
    let archive = make_archive(&repo);
    let keys = make_keys(&dir);
    trust_publisher(&dir, &keys);
    create_index(&dir, &repo, &keys, "2");

    ok(run(
        &dir,
        &[
            "sign",
            archive.to_str().unwrap(),
            "--key",
            keys.secret.to_str().unwrap(),
        ],
    ));
    fs::remove_file(repo.join("index.toml.sig")).unwrap();

    let denied = install_hello(&dir, &repo, &[]);
    assert!(!denied.status.success(), "unsigned index must not be accepted by default");

    let accepted = install_hello(&dir, &repo, &["--allow-unsigned"]);
    assert!(accepted.status.success(), "{}\n{}", stdout(&accepted), stderr(&accepted));
    let message = format!("{}{}", stdout(&accepted), stderr(&accepted));
    assert!(
        message.contains("legacy per-archive signature policy"),
        "explicit unsigned-index override must not activate v2 archive authentication: {message}"
    );
    assert_eq!(
        fs::read_to_string(dir.join("root/usr/share/hello")).unwrap(),
        "from-v2-repository\n"
    );
    let _ = fs::remove_dir_all(&dir);
}


#[test]
fn upgrade_uses_v2_index_hashes_without_package_sidecars() {
    let dir = scratch("upgrade-v2");
    let keys = make_keys(&dir);
    trust_publisher(&dir, &keys);

    // First install v1 through the unchanged direct-archive signature path.
    let old_repo = dir.join("old");
    let old_archive = make_archive_version(&old_repo, "1", "old-version\n");
    ok(run(
        &dir,
        &[
            "sign",
            old_archive.to_str().unwrap(),
            "--key",
            keys.secret.to_str().unwrap(),
        ],
    ));
    ok(run(&dir, &["install", "--yes", old_archive.to_str().unwrap()]));
    assert_eq!(
        fs::read_to_string(dir.join("root/usr/share/hello")).unwrap(),
        "old-version\n"
    );

    // Publish v2 with only the signed index sidecar, not a package sidecar.
    let repo = dir.join("repo");
    let new_archive = make_archive_version(&repo, "2", "new-version\n");
    create_index(&dir, &repo, &keys, "2");
    assert!(!new_archive.with_file_name("hello-2-1.nawa.tar.zst.sig").exists());

    ok(run(&dir, &["upgrade", "--yes", "--repo", repo.to_str().unwrap()]));
    assert_eq!(
        fs::read_to_string(dir.join("root/usr/share/hello")).unwrap(),
        "new-version\n"
    );
    let _ = fs::remove_dir_all(&dir);
}

