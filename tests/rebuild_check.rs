//! Phase-0 baseline: the legacy `--rebuild-check` must pass unchanged before
//! the fetch-phase work (fetch-phase.md §16 Phase 0) touches anything, so
//! every later phase can prove "no legacy regression" against it.
//!
//! Two recipes, both hermetic (no upstream network — the tarball case is
//! served from a local listener):
//!
//! * a meta recipe (empty source) — the cheapest full pipeline run,
//! * a tarball recipe with a *pinned* `source_date_epoch` — exercises the
//!   download cache, extraction, and the normalized-archive path twice at
//!   different wall-clock times, in two different build directories.
//!
//! Note on the pinned epoch: `buildenv::source_date_epoch` walks the working
//! directory *including directories*, and tar entries that arrive
//! dirs-first (the common upstream layout) get their directory mtimes
//! bumped to the extraction time when children are written into them
//! (fetch-phase.md §1.2 records this; there is no deferred dir-mtime pass).
//! An *unpinned* dirs-first tarball is therefore NOT stable across
//! re-extraction at two different times. The new-schema rule (§6) excludes
//! directories from the walk for exactly this reason; legacy recipes that
//! must survive `--rebuild-check` today pin `source_date_epoch`.

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

fn nawa_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_nawa"))
}

/// A nawa invocation isolated from the ambient configuration (same harness
/// as e2e.rs): no NAWA_* env, cwd outside the repository so the repo's own
/// nawa.toml/profile is never discovered, no /etc config.
fn nawa() -> Command {
    let mut c = Command::new(nawa_bin());
    for k in [
        "NAWA_CONFIG", "NAWA_ROOT", "NAWA_RECIPES", "NAWA_BUILDDIR",
        "NAWA_PROFILES", "NAWA_JOBS", "NAWA_REPO", "NAWA_INSTALL_MODE",
        "NAWA_OFFLINE", "NAWA_SRCCACHE",
    ] {
        c.env_remove(k);
    }
    c.env("NAWA_SYSCONFDIR", std::env::temp_dir().join("nawa-test-no-sysconf"));
    c.env("NAWA_INSTALL_MODE", "build");
    c.current_dir(std::env::temp_dir());
    c
}

fn scratch(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("nawa-rebc-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

fn sha256(bytes: &[u8]) -> String {
    use sha2::Digest;
    hex::encode(sha2::Sha256::digest(bytes))
}

/// Deterministic dirs-first tar.gz with explicit, fixed mtimes (dir 1e6 /
/// 9e5, file 5e5 — like a normal upstream release tarball rotated through
/// `tar`]: directories before their children).
fn fixture_tar_gz(top: &str) -> Vec<u8> {
    let mut plain = Vec::new();
    {
        let mut b = tar::Builder::new(&mut plain);
        let dir = |b: &mut tar::Builder<&mut Vec<u8>>, name: &str, mtime: u64| {
            let mut h = tar::Header::new_gnu();
            h.set_entry_type(tar::EntryType::Directory);
            h.set_size(0);
            h.set_mode(0o755);
            h.set_mtime(mtime);
            h.set_cksum();
            b.append_data(&mut h, name, std::io::empty()).unwrap();
        };
        let file = |b: &mut tar::Builder<&mut Vec<u8>>, name: &str, data: &[u8], mtime: u64| {
            let mut h = tar::Header::new_gnu();
            h.set_size(data.len() as u64);
            h.set_mode(0o644);
            h.set_mtime(mtime);
            h.set_cksum();
            b.append_data(&mut h, name, data).unwrap();
        };
        dir(&mut b, top, 1_000_000);
        dir(&mut b, &format!("{top}/tools"), 900_000);
        file(
            &mut b,
            &format!("{top}/tools/hello.sh"),
            b"#!/bin/sh\necho hello-nawa-rebuild-check\n",
            500_000,
        );
        b.finish().unwrap();
    }
    let mut gz = Vec::new();
    flate2::write::GzEncoder::new(&mut gz, flate2::Compression::default())
        .write_all(&plain)
        .unwrap();
    gz
}

/// Serve `body` for every connection (one byte of setup: the build will
/// connect exactly as often as it needs a download).
fn serve_forever(body: Vec<u8>) -> std::net::TcpListener {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let l2 = listener.try_clone().unwrap();
    std::thread::spawn(move || loop {
        let Ok((mut sock, _)) = l2.accept() else { break };
        let resp = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len())
            .into_bytes();
        use std::io::Write;
        let _ = sock.write_all(&resp);
        let _ = sock.write_all(&body);
    });
    listener
}

fn write_meta_recipe(recipes: &Path) {
    let r = recipes.join("rebmeta");
    fs::create_dir_all(&r).unwrap();
    fs::write(
        r.join("recipe.toml"),
        r#"
[package]
name = "rebmeta"
version = "1.0"
rel = 1
arch = ["x86_64"]

[build]
install = """
mkdir -p %{pkgdir}/usr
printf 'meta-payload\n' > %{pkgdir}/usr/flag
"""
"#,
    )
    .unwrap();
}

#[test]
fn legacy_meta_recipe_rebuild_check_passes() {
    let s = scratch("meta");
    let recipes = s.join("recipes");
    write_meta_recipe(&recipes);
    let builddir = s.join("build");

    let out = nawa()
        .args([
            "--recipes",
            recipes.to_str().unwrap(),
            "--builddir",
            builddir.to_str().unwrap(),
            "build",
            "--rebuild-check",
            "rebmeta",
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "rebuild-check failed:\n{stdout}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.contains("rebuild-check: OK"), "{stdout}");
    let _ = fs::remove_dir_all(&s);
}

#[test]
fn legacy_tarball_recipe_with_pinned_epoch_rebuild_check_passes() {
    let s = scratch("tarball");
    let recipes = s.join("recipes");
    let builddir = s.join("build");

    // one local server for the whole invocation: run A downloads, run B
    // must be served from the *shared* source cache ($NAWA_SRCCACHE is
    // absolute, the default <builddir>/distfiles would move with the
    // rebuild-a/rebuild-b subdirectories)
    let top = "example-1.0";
    let tarball = fixture_tar_gz(top);
    let server = serve_forever(tarball.clone());
    let url = format!("http://127.0.0.1:{}/example-1.0.tar.gz", server.local_addr().unwrap().port());

    let r = recipes.join("rebtar");
    fs::create_dir_all(&r).unwrap();
    fs::write(
        r.join("recipe.toml"),
        format!(
            r#"
[package]
name = "rebtar"
version = "1.0"
rel = 1
arch = ["x86_64"]
source = "{url}"
sha256 = "{}"
source_dir = "example-1.0"
source_date_epoch = 1234567

[build]
install = """
mkdir -p %{{pkgdir}}/usr/bin
cp %{{srcdir}}/tools/hello.sh %{{pkgdir}}/usr/bin/hello
chmod +x %{{pkgdir}}/usr/bin/hello
"""
"#,
            sha256(&tarball)
        ),
    )
    .unwrap();

    let out = nawa()
        .env(
            "NAWA_SRCCACHE",
            s.join("shared-distfiles"), // shared across rebuild-a and rebuild-b
        )
        .args([
            "--recipes",
            recipes.to_str().unwrap(),
            "--builddir",
            builddir.to_str().unwrap(),
            "build",
            "--rebuild-check",
            "rebtar",
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "rebuild-check failed:\n{stdout}\n--- stderr ---\n{stderr}"
    );
    assert!(stdout.contains("rebuild-check: OK"), "{stdout}");
    // run A downloads once ("downloading"), run B is served from the
    // shared cache ("source cached") — both lines land on stdout
    assert!(
        stdout.contains("downloading") && stdout.contains("source cached"),
        "run A must download and run B must hit the shared cache: {stdout}"
    );
    let _ = fs::remove_dir_all(&s);
}
