//! Phase-0 offline gate baseline (fetch-phase.md §16, revision-3 test 7):
//!
//! 1. `NAWA_OFFLINE=1` with a cold cache must fail *without opening a
//!    single socket connection* — the guard lives in `fetch::download*`,
//!    which is the only place nawa touches the network.
//! 2. the same recipe with a *seeded, verified* source-cache entry builds
//!    successfully offline (a cache hit returns before any connect).
//!
//! Both run in a child `nawa` process, so they never touch the test
//! process's global flags; the local listener counts every accepted
//! connection and must end at zero for the offline tests.

use std::fs;
use std::io::Write as _;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

fn nawa_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_nawa"))
}

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
    let d = std::env::temp_dir().join(format!("nawa-offline-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

fn sha256(bytes: &[u8]) -> String {
    use sha2::Digest;
    hex::encode(sha2::Sha256::digest(bytes))
}

fn fixture_tar_gz(top: &str) -> Vec<u8> {
    let mut plain = Vec::new();
    {
        let mut b = tar::Builder::new(&mut plain);
        let dir = |b: &mut tar::Builder<&mut Vec<u8>>, name: &str| {
            let mut h = tar::Header::new_gnu();
            h.set_entry_type(tar::EntryType::Directory);
            h.set_size(0);
            h.set_mode(0o755);
            h.set_cksum();
            b.append_data(&mut h, name, std::io::empty()).unwrap();
        };
        let file = |b: &mut tar::Builder<&mut Vec<u8>>, name: &str, data: &[u8]| {
            let mut h = tar::Header::new_gnu();
            h.set_size(data.len() as u64);
            h.set_mode(0o644);
            h.set_cksum();
            b.append_data(&mut h, name, data).unwrap();
        };
        dir(&mut b, top);
        file(&mut b, &format!("{top}/hello.sh"), b"#!/bin/sh\necho offline-ok\n");
        b.finish().unwrap();
    }
    let mut gz = Vec::new();
    flate2::write::GzEncoder::new(&mut gz, flate2::Compression::default())
        .write_all(&plain)
        .unwrap();
    gz
}

/// A listener that accepts forever and counts. Whether the child process is
/// allowed to connect at all is exactly what the two tests assert.
fn counting_listener() -> (std::net::SocketAddr, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let count = Arc::new(AtomicUsize::new(0));
    let l2 = listener.try_clone().unwrap();
    let counter = count.clone();
    std::thread::spawn(move || {
        // serve a real payload too: if the guard is ever broken, the build
        // would *succeed* over the network instead of failing loudly
        let body = fixture_tar_gz("example-1.0");
        loop {
            let Ok((mut sock, _)) = l2.accept() else { break };
            counter.fetch_add(1, Ordering::SeqCst);
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .into_bytes();
            let _ = sock.write_all(&resp);
            let _ = sock.write_all(&body);
        }
    });
    (addr, count)
}

/// recipe + harness pieces shared by both tests.
struct Setup {
    recipes: PathBuf,
    builddir: PathBuf,
    /// URL of the (local, counting) source
    url: String,
    sha: String,
    cache: PathBuf,
}

fn setup(tag: &str) -> (Setup, Arc<AtomicUsize>) {
    let s = scratch(tag);
    let (addr, count) = counting_listener();
    let body = fixture_tar_gz("example-1.0");
    let recipes = s.join("recipes");
    let r = recipes.join("reboff");
    fs::create_dir_all(&r).unwrap();
    let sha = sha256(&body);
    fs::write(
        r.join("recipe.toml"),
        format!(
            r#"
[package]
name = "reboff"
version = "1.0"
rel = 1
arch = ["x86_64"]
source = "http://{addr}/example-1.0.tar.gz"
sha256 = "{sha}"
source_dir = "example-1.0"
source_date_epoch = 1234567

[build]
install = """
mkdir -p %{{pkgdir}}/usr/bin
cp %{{srcdir}}/hello.sh %{{pkgdir}}/usr/bin/hello
chmod +x %{{pkgdir}}/usr/bin/hello
"""
"#
        ),
    )
    .unwrap();
    (
        Setup {
            recipes,
            builddir: s.join("build"),
            url: format!("http://{addr}/example-1.0.tar.gz"),
            sha,
            cache: s.join("distfiles"),
        },
        count,
    )
}

fn seed_cache(sha: &str, cache: &Path) {
    fs::create_dir_all(cache).unwrap();
    let body = fixture_tar_gz("example-1.0");
    let name = format!("{}-example-1.0.tar.gz", &sha[..16]);
    fs::write(cache.join(name), body).unwrap();
}

#[test]
fn offline_cold_cache_fails_without_opening_a_socket() {
    let (st, count) = setup("cold");
    let out = nawa()
        .env("NAWA_OFFLINE", "1")
        .env("NAWA_SRCCACHE", &st.cache)
        .args([
            "--recipes",
            st.recipes.to_str().unwrap(),
            "--builddir",
            st.builddir.to_str().unwrap(),
            "build",
            "reboff",
        ])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "an unpopulated cache must not build offline:\n{stderr}"
    );
    assert!(
        stderr.contains("offline mode") && stderr.contains(&st.url),
        "the error must name offline mode and the missing input: {stderr}"
    );
    assert_eq!(
        count.load(Ordering::SeqCst),
        0,
        "offline mode must fail before any connection is attempted"
    );
    let _ = fs::remove_dir_all(st.builddir.parent().unwrap());
}

#[test]
fn offline_seeded_cache_builds_without_opening_a_socket() {
    let (st, count) = setup("warm");
    seed_cache(&st.sha, &st.cache);
    let out = nawa()
        .env("NAWA_OFFLINE", "1")
        .env("NAWA_SRCCACHE", &st.cache)
        .args([
            "--recipes",
            st.recipes.to_str().unwrap(),
            "--builddir",
            st.builddir.to_str().unwrap(),
            "build",
            "reboff",
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "a verified cache entry must build offline:\n{stdout}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        stdout.contains("source cached"),
        "the build must report its cache hit: {stdout}"
    );
    assert_eq!(
        count.load(Ordering::SeqCst),
        0,
        "a cache hit must not touch the network, offline or not"
    );
    let _ = fs::remove_dir_all(st.builddir.parent().unwrap());
}
