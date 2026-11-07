//! Integration tests for the Go fetch backend (fetch-phase.md §16
//! "Backward compatibility" + "Go").
//!
//! Highly hermetic: external sources arrive through a route-based local
//! HTTP server and the ecosystem fetch resolves through `NAWA_GOPROXY`
//! pointing at a `file://` module proxy fixture built in the scratch
//! (the design's hermetic-test clause; a `file://` proxy implies
//! `GOSUMDB=off`, §7). The toolchain location travels through
//! `NAWA_FETCH_PATH` (test-only, documented in `src/fetchrun.rs`) because
//! the build whitelist PATH is pinned and must not grow and tests still
//! need the binary in the recipes' install scripts.
//!
//! The tests run at `--isolation env`: the fetch then runs as a plain
//! cleared-environment command (§12: no sandbox at this level). The
//! strict-mode fetch profile is exercised by the sandbox tests elsewhere.

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
        "NAWA_SRCROOT", "NAWA_ASSETSDIR",
        // the fetch knobs must never leak into a run by accident
        "NAWA_GOPROXY", "NAWA_FETCH_PATH", "NAWA_FETCH_TIMEOUT",
    ] {
        c.env_remove(k);
    }
    c.env("NAWA_SYSCONFDIR", std::env::temp_dir().join("nawa-test-no-sysconf"));
    c.env("NAWA_INSTALL_MODE", "build");
    c.current_dir(std::env::temp_dir());
    c
}

fn scratch(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("nawa-fetchgo-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

// Strict sandbox tests need a build directory outside the sandbox's private /tmp.
fn scratch_outside_tmp(tag: &str) -> PathBuf {
    let d = PathBuf::from("/var/tmp").join(format!("nawa-fetchgo-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

fn sha256(bytes: &[u8]) -> String {
    use sha2::Digest;
    hex::encode(sha2::Sha256::digest(bytes))
}

/// Serve a fixed route table (path → body); counts accepted connections so
/// tests can prove who talked to the network. Same shape as tests/sources.rs.
fn route_server(
    routes: Vec<(&'static str, Option<Vec<u8>>)>,
) -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let count = Arc::new(AtomicUsize::new(0));
    let l2 = listener.try_clone().unwrap();
    let routes: Vec<(String, Option<Vec<u8>>)> = routes
        .into_iter()
        .map(|(p, b)| (p.to_string(), b))
        .collect();
    let counter = count.clone();
    std::thread::spawn(move || loop {
        let Ok((mut sock, _)) = l2.accept() else { break };
        counter.fetch_add(1, Ordering::SeqCst);
        let mut buf = [0u8; 4096];
        let n = std::io::Read::read(&mut sock, &mut buf).unwrap_or(0);
        let req = String::from_utf8_lossy(&buf[..n]).to_string();
        let path = req.split_whitespace().nth(1).unwrap_or("").to_string();
        let (status, body) = match routes.iter().find(|(p, _)| *p == path) {
            Some((_, Some(b))) => ("HTTP/1.1 200 OK", b.clone()),
            _ => ("HTTP/1.1 404 Not Found", b"absent\n".to_vec()),
        };
        let resp = format!(
            "{status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        let _ = sock.write_all(&resp);
        let _ = sock.write_all(&body);
    });
    (format!("http://127.0.0.1:{port}"), count)
}

// ---------------------------------------------------------------------------
// file:// module-proxy fixture
// ---------------------------------------------------------------------------

/// The dependency module, served at `<proxy>/<module>/@v/…` exactly as a
/// minimal module proxy must (§7: `file://` proxies keep tests hermetic).
/// Sizes and order come from `go mod download` validation (the proxy zip
/// packs every file under `<module-path>@<version>/`).
fn fixture_proxy_zip(module: &str, version: &str, file: &(&str, &[u8])) -> Vec<u8> {
    let name = format!("{module}@{version}/{}", file.0);
    stored_zip(&name, file.1)
}

/// A tiny ZIP writer: one entry, STORE method (no compression), zero
/// timestamps — deterministic bytes. Hand-rolled because the tree has no
/// `zip` dependency and the test needs a byte-deterministic artifact.
fn stored_zip(name: &str, data: &[u8]) -> Vec<u8> {
    let crc = crc32(data);
    let mut z = Vec::new();
    // local file header
    let mut lfh: Vec<u8> = Vec::new();
    put32(&mut lfh, 0x04034b50);
    put16(&mut lfh, 20); // version needed
    put16(&mut lfh, 0); // flags
    put16(&mut lfh, 0); // method: store
    put16(&mut lfh, 0); // mtime
    put16(&mut lfh, 0x0021); // mdate: 1980-01-01
    put32(&mut lfh, crc);
    put32(&mut lfh, data.len() as u32);
    put32(&mut lfh, data.len() as u32);
    put16(&mut lfh, name.len() as u16);
    put16(&mut lfh, 0); // extra len
    lfh.extend(name.as_bytes());
    lfh.extend(data);
    z.extend(&lfh);
    // central directory
    let cd_off = z.len() as u32;
    let mut cd: Vec<u8> = Vec::new();
    put32(&mut cd, 0x02014b50);
    put16(&mut cd, 20); // made by
    put16(&mut cd, 20); // needed
    put16(&mut cd, 0);
    put16(&mut cd, 0);
    put16(&mut cd, 0);
    put16(&mut cd, 0x0021);
    put32(&mut cd, crc);
    put32(&mut cd, data.len() as u32);
    put32(&mut cd, data.len() as u32);
    put16(&mut cd, name.len() as u16);
    put16(&mut cd, 0);
    put16(&mut cd, 0);
    put16(&mut cd, 0); // disk
    put16(&mut cd, 0); // internal attrs
    put32(&mut cd, 0); // external attrs
    put32(&mut cd, 0); // local header offset
    cd.extend(name.as_bytes());
    let eocd: Vec<u8> = {
        let mut e = Vec::new();
        put32(&mut e, 0x06054b50);
        put16(&mut e, 0); // disk
        put16(&mut e, 0); // cd start disk
        put16(&mut e, 1);
        put16(&mut e, 1);
        put32(&mut e, cd.len() as u32);
        put32(&mut e, cd_off);
        put16(&mut e, 0); // comment len
        e
    };
    z.extend(&cd);
    z.extend(&eocd);
    z
}

fn put16(b: &mut Vec<u8>, v: u16) {
    b.extend(v.to_le_bytes());
}

fn put32(b: &mut Vec<u8>, v: u32) {
    b.extend(v.to_le_bytes());
}

/// Standard CRC32 (polynomial 0xEDB88320, reflected), init 0.
fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

/// One proxy layout for the `example.org/hello v1.0.0` fixture module
/// (list + .info + .mod + .zip). The dir is dropped afterwards as scratch.
fn write_proxy_fixture(dir: &Path) {
    let base = dir.join("example.org/hello/@v");
    fs::create_dir_all(&base).unwrap();
    fs::write(base.join("list"), b"v1.0.0\n").unwrap();
    fs::write(
        base.join("v1.0.0.info"),
        b"{\"Version\":\"v1.0.0\",\"Time\":\"2022-01-01T00:00:00Z\"}",
    )
    .unwrap();
    fs::write(base.join("v1.0.0.mod"), b"module example.org/hello\n\ngo 1.16\n").unwrap();
    fs::write(
        base.join("v1.0.0.zip"),
        fixture_proxy_zip("example.org/hello", "v1.0.0", &("hello.go", b"package hello\n\nfunc Hi() string { return \"hi-from-dep\" }\n" as &[u8])),
    )
    .unwrap();
}

// ---------------------------------------------------------------------------
// the main-module fixture
// ---------------------------------------------------------------------------

/// The pinned h1 lines of the fixture module — computed once with the real
/// `go` toolchain against this exact proxy zip, then frozen (they are
/// content digests of the fixture bytes, so they cannot drift).
const HELLO_GO_SUM_PINS: &[u8] = b"example.org/hello v1.0.0 h1:w02iB5Oh3gIejn36rCHwy7uXSkYM8f/2+DFnxphQV8o=\nexample.org/hello v1.0.0/go.mod h1:Q3qg57GZNcwpC031JBnOm3N7J89qJIgXV+N5zkiIyWw=\n";

fn go_mod(comment: &str) -> Vec<u8> {
    format!("module main.fix\n\n{comment}go 1.16\n\nrequire example.org/hello v1.0.0\n")
        .into_bytes()
}

/// The main-module files: `go.mod`, `go.sum`, `main.go` (+ extra files,
/// for vendoring and for rejecting workspaces).
fn main_fixture(extra: &[(String, Vec<u8>)], comment: &str) -> Vec<(String, Vec<u8>)> {
    let mut files = vec![
        ("go.mod".to_string(), go_mod(comment)),
        ("go.sum".to_string(), HELLO_GO_SUM_PINS.to_vec()),
        (
            "main.go".to_string(),
            b"package main\n\nimport (\n\t\"fmt\"\n\t\"example.org/hello\"\n)\n\nfunc main() { fmt.Println(hello.Hi()) }\n".to_vec(),
        ),
    ];
    files.extend(extra.iter().cloned());
    files
}

/// A deterministic tar.gz with one top dir and fixed modes/mtimes.
fn fixture_tar_gz(top: &str, files: &[(String, Vec<u8>)]) -> Vec<u8> {
    let mut plain = Vec::new();
    {
        let mut b = tar::Builder::new(&mut plain);
        let mut h = tar::Header::new_gnu();
        h.set_entry_type(tar::EntryType::Directory);
        h.set_size(0);
        h.set_mode(0o755);
        h.set_mtime(1_400_000);
        h.set_cksum();
        b.append_data(&mut h, top, std::io::empty()).unwrap();
        for (name, data) in files {
            let mut h = tar::Header::new_gnu();
            h.set_size(data.len() as u64);
            h.set_mode(0o644);
            h.set_mtime(500_000);
            h.set_cksum();
            b.append_data(&mut h, format!("{top}/{name}"), &data[..]).unwrap();
        }
        b.finish().unwrap();
    }
    let mut gz = Vec::new();
    let mut w = flate2::write::GzEncoder::new(&mut gz, flate2::Compression::default());
    w.write_all(&plain).unwrap();
    w.finish().unwrap();
    gz
}

/// The `[fetch.go]` recipe whose source carries the main module.
fn recipe(source_sha: &str, install: &str, extra: &str) -> String {
    format!(
        r#"
[package]
name = "go-b"
version = "1.0"
rel = 1
arch = ["x86_64"]
source = "local:/nawa-schema-2-requires-a-newer-nawa"
workdir = "m"

[[package.sources]]
name = "src"
url = "%URL%/src.tar.gz"
sha256 = "{source_sha}"
into = "m"
strip_components = 1

[build]
install = """
{install}
"""
{extra}
"#
    )
}

/// The `go` binary location on this host: an explicit `NAWA_FETCH_PATH`
/// wins; otherwise `go` resolved from the test process's own PATH. The
/// value feeds the fetch run (NAWA_FETCH_PATH — its documented test-only
/// purpose in `src/fetchrun.rs`) and the recipes' install scripts, which
/// need an absolute path because the build whitelist PATH is pinned.
fn toolchain_dir() -> String {
    if let Ok(p) = std::env::var("NAWA_FETCH_PATH") {
        if !p.trim().is_empty() {
            return p.trim().split(':').next().unwrap().to_string();
        }
    }
    let probed = Command::new("sh").args(["-c", "command -v go"]).output().ok();
    if let Some(o) = probed {
        if o.status.success() {
            let path = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if let Some(dir) = Path::new(&path).parent() {
                return dir.to_string_lossy().into_owned();
            }
        }
    }
    "/usr/bin".to_string()
}

/// A build invocation of the `go-b` test recipe with the fetch envs the
/// tests control: `NAWA_GOPROXY=file://<proxy>` (hermetic), and any extra
/// CLI args / environment overrides the specific test needs.
fn run_fetch_build(
    s: &Path,
    proxy: Option<&Path>,
    extra_env: &[(&str, &str)],
    extra_args: &[&str],
) -> (String, String, bool) {
    run_fetch_build_with_isolation(s, "env", proxy, extra_env, extra_args)
}

fn run_fetch_build_with_isolation(
    s: &Path,
    isolation: &str,
    proxy: Option<&Path>,
    extra_env: &[(&str, &str)],
    extra_args: &[&str],
) -> (String, String, bool) {
    let gopath = toolchain_dir();
    let out = nawa()
        .args([
            "--recipes",
            s.join("recipes").to_str().unwrap(),
            "--builddir",
            s.join("build").to_str().unwrap(),
            "build",
            "--isolation",
            isolation,
        ])
        .args(extra_args)
        .arg("go-b")
        .env("NAWA_SRCCACHE", s.join("cache"))
        .env("NAWA_FETCH_PATH", &gopath)
        .env(
            "NAWA_GOPROXY",
            proxy
                .map(|p| format!("file://{}", p.display()))
                .unwrap_or_else(|| "https://proxy.invalid-hermetic-test".to_string()),
        )
        .envs(extra_env.iter().map(|(k, v)| (k.to_string(), v.to_string())))
        .output()
        .unwrap();
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.success(),
    )
}

// ---------------------------------------------------------------------------
// §16 Go 1: fetch → build offline → cache hit; lockfiles unchanged
// ---------------------------------------------------------------------------

#[test]
fn go_backend_fetches_builds_offline_and_hits_the_cache() {
    let s = scratch("backend");
    let recipes = s.join("recipes");
    let pkg = recipes.join("go-b");
    fs::create_dir_all(&pkg).unwrap();
    let proxy = s.join("proxy");
    write_proxy_fixture(&proxy);

    // the fixture module + its dependency, served hermetically
    let tar = fixture_tar_gz("main-1.0", &main_fixture(&[], ""));
    let tar_sha = sha256(&tar);
    let (addr, count) = route_server(vec![("/src.tar.gz", Some(tar))]);

    let gobin = toolchain_dir();
    fs::write(
        pkg.join("recipe.toml"),
        recipe(
            &tar_sha,
            "%{GO}/go build -o %{pkgdir}/bin/m ./... \
             && mkdir -p %{pkgdir}/usr/share/m \
             && %{pkgdir}/bin/m > %{pkgdir}/usr/share/m/out.txt \
             && cat %{pkgdir}/usr/share/m/out.txt",
            "[fetch.go]\ndir = \"m\"\n",
        )
        .replace("%URL%", &addr)
        .replace("%{GO}", &gobin),
    )
    .unwrap();

    // — run 1: online — the fetch populate runs and the binary resolves the
    // dependency out of the module cache view
    let (out, err, ok) = run_fetch_build(&s, Some(&proxy), &[], &[]);
    assert!(ok, "run 1 failed\nstdout:\n{out}\nstderr:\n{err}");
    assert!(out.contains("hi-from-dep"), "the go build produced wrong output:\n{out}");
    let work = s.join("build/go-b-1.0-1");
    assert!(work.join("fetch-go.log").is_file(), "the populate must run once (fetch log)");
    let hits1 = count.load(Ordering::SeqCst);
    assert!(hits1 >= 1, "the source must be fetched over the route server");

    // — the cache entry: <srccache>/fetch/go/<key> with marker, read-only,
    // and the hold-open lock file next to it (§11)
    let cache = s.join("cache/fetch/go");
    let mut entries: Vec<PathBuf> = std::fs::read_dir(&cache)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    entries.sort();
    assert_eq!(entries.len(), 1, "exactly one ecosystem entry after run 1");
    let entry = &entries[0];
    let key = entry.file_name().unwrap().to_string_lossy().to_string();
    assert_eq!(key.len(), 64, "the key is a full sha256: {key}");
    use std::os::unix::fs::PermissionsExt;
    let mode = entry.symlink_metadata().unwrap().permissions().mode();
    assert_eq!(mode & 0o222, 0, "the published entry must have no write bits");
    assert!(
        entry.join("nawa-complete").is_file(),
        "the completion marker must exist in the entry"
    );
    assert!(
        cache.join(format!("{key}.lock")).is_file(),
        "the entry lock file must exist"
    );

    // — the per-build view: a copy of the module cache without the marker
    let view = work.join("fetch/go");
    assert!(
        view.join("example.org/hello@v1.0.0").is_dir(),
        "the view must carry the downloaded module"
    );
    assert!(
        !view.join("nawa-complete").exists(),
        "the marker is cache metadata, never a view member"
    );

    // — run 2: --offline with the work dir wiped — the published entry
    // serves the build; the fetch never runs again, the source is not
    // re-downloaded (route server count unchanged)
    fs::remove_dir_all(&work).unwrap();
    let (out2, err2, ok2) =
        run_fetch_build(&s, Some(&proxy), &[("NAWA_OFFLINE", "1")], &[]);
    assert!(ok2, "run 2 (--offline) failed\nstdout:\n{out2}\nstderr:\n{err2}");
    assert!(out2.contains("hi-from-dep"), "the offline build resolved the dep:\n{out2}");
    let work2 = work.clone();
    assert!(
        !work2.join("fetch-go.log").exists(),
        "no populate may run on the second build"
    );
    assert_eq!(
        count.load(Ordering::SeqCst),
        hits1,
        "the --offline run must not re-download the source"
    );
    assert_eq!(entries, {
        let mut e2: Vec<PathBuf> = std::fs::read_dir(&cache)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        e2.sort();
        e2
    }, "run 2 must not add an entry");

    // §16 Go: go.mod/go.sum unchanged — staged bytes equal fixture bytes
    let mod_file = work.join("src/m/go.mod");
    let sum_file = work.join("src/m/go.sum");
    assert_eq!(
        fs::read(&mod_file).unwrap(),
        go_mod(""),
        "go.mod must come out of the fetch cycle byte-identical"
    );
    assert_eq!(
        fs::read(&sum_file).unwrap(),
        HELLO_GO_SUM_PINS.to_vec(),
        "go.sum must come out of the fetch cycle byte-identical"
    );

    // — run 3: `--use-cache` — build id with the fetch-go extra matches and
    // the whole build (fetch included) is skipped
    let captcount = count.load(Ordering::SeqCst);
    let (out3, err3, ok3) = run_fetch_build(&s, Some(&proxy), &[], &["--use-cache"]);
    assert!(ok3, "use-cache run failed\nstdout:\n{out3}\nstderr:\n{err3}");
    assert!(
        out3.contains("build cache hit"),
        "the sidecar build id (with fetch-go=1) must match:\n{out3}\n{err3}"
    );
    assert_eq!(
        count.load(Ordering::SeqCst),
        captcount,
        "a cache hit must not touch the network at all"
    );
}

// ---------------------------------------------------------------------------
// §16 (implied) 2: the ecosystem key follows the lockfile bytes
// ---------------------------------------------------------------------------

#[test]
fn the_ecosystem_key_follows_the_lockfile_bytes() {
    let s = scratch("key");
    let recipes = s.join("recipes");
    let pkg = recipes.join("go-b");
    fs::create_dir_all(&pkg).unwrap();
    let proxy = s.join("proxy");
    write_proxy_fixture(&proxy);
    let gobin = toolchain_dir();
    let cache = s.join("cache/fetch/go");
    let dirs = || -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = std::fs::read_dir(&cache)
            .unwrap_or_else(|e| panic!("read {}: {e}", cache.display()))
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        v.sort();
        v
    };

    // run 1: the original module
    let tar1 = fixture_tar_gz("main-1.0", &main_fixture(&[], ""));
    let sha1 = sha256(&tar1);
    let (addr1, _c1) = route_server(vec![("/src.tar.gz", Some(tar1))]);
    fs::write(
        pkg.join("recipe.toml"),
        recipe(
            &sha1,
            &format!("{gobin}/go build -o %{{pkgdir}}/usr/bin/m ./... && %{{pkgdir}}/usr/bin/m"),
            "[fetch.go]\ndir = \"m\"\n",
        )
        .replace("%URL%", &addr1)
        .replace("%{GO}", &gobin),
    )
    .unwrap();
    let (o1, e1, ok1) = run_fetch_build(&s, Some(&proxy), &[], &[]);
    assert!(ok1, "run 1 failed\n{o1}\n{e1}");
    assert_eq!(dirs().len(), 1, "one entry for the first lockfile set");

    // run 2: go.mod gains a comment — valid for Go, different bytes → new key
    let tar2 = fixture_tar_gz("main-1.0", &main_fixture(&[], "// nawa-key-bump\n"));
    let sha2 = sha256(&tar2);
    let (addr2, _c2) = route_server(vec![("/src.tar.gz", Some(tar2))]);
    fs::write(
        pkg.join("recipe.toml"),
        recipe(
            &sha2,
            &format!("{gobin}/go build -o %{{pkgdir}}/usr/bin/m ./... && %{{pkgdir}}/usr/bin/m"),
            "[fetch.go]\ndir = \"m\"\n",
        )
        .replace("%URL%", &addr2)
        .replace("%{GO}", &gobin),
    )
    .unwrap();
    let (o2, e2, ok2) = run_fetch_build(&s, Some(&proxy), &[], &[]);
    assert!(ok2, "run 2 failed\n{o2}\n{e2}");
    let ds = dirs();
    assert_eq!(ds.len(), 2, "the new lockfile bytes must get their own entry");
    for d in &ds {
        assert!(d.join("nawa-complete").is_file(), "every published entry carries the marker");
    }
}

// ---------------------------------------------------------------------------
// §16 Go + §12: an offline miss names the entry; setup proves the split
// between the source cache (pre-seeded) and the ecosystem entry.
// ---------------------------------------------------------------------------

#[test]
fn an_offline_ecosystem_miss_names_the_expected_entry() {
    let s = scratch("offline-miss");
    let recipes = s.join("recipes");
    let pkg = recipes.join("go-b");
    fs::create_dir_all(&pkg).unwrap();
    let proxy = s.join("proxy");
    write_proxy_fixture(&proxy);

    let tar = fixture_tar_gz("main-1.0", &main_fixture(&[], ""));
    let tar_sha = sha256(&tar);
    let (addr, _) = route_server(vec![("/src.tar.gz", Some(tar))]);
    let gobin = toolchain_dir();
    fs::write(
        pkg.join("recipe.toml"),
        recipe(
            &tar_sha,
            &format!("{gobin}/go build -o %{{pkgdir}}/usr/bin/m ./... && %{{pkgdir}}/usr/bin/m"),
            "[fetch.go]\ndir = \"m\"\n",
        )
        .replace("%URL%", &addr)
        .replace("%{GO}", &gobin),
    )
    .unwrap();

    // populate everything once (source cache + ecosystem entry)
    let (o, e, ok) = run_fetch_build(&s, Some(&proxy), &[], &[]);
    assert!(ok, "setup run failed\n{o}\n{e}");

    // wipe only the ecosystem entries: the source cache stays usable.
    // §11: published entries are read-only, so removal restores u+w like
    // `nawa clean sources` will (§11 retention rule).
    force_rm(&s.join("cache/fetch"));

    // §12: the offline run must fail with the entry it is missing
    let (o2, e2, ok2) = run_fetch_build(&s, None, &[("NAWA_OFFLINE", "1")], &[]);
    assert!(!ok2, "the offline run must fail, got:\n{o2}\n{e2}");
    assert!(
        e2.contains("offline") && e2.contains("/fetch/go/"),
        "the error must name the missing fetch/go entry:\n{e2}"
    );
    // …and nothing half-state must appear in the cache
    assert!(
        !s.join("cache/fetch").exists(),
        "an offline miss must not populate a partial entry"
    );

    // the online run recovers (populate → publish → copy → build)
    let (o3, e3, ok3) = run_fetch_build(&s, Some(&proxy), &[], &[]);
    assert!(ok3, "recovery run failed\n{o3}\n{e3}");
    assert!(
        o3.contains("hi-from-dep"),
        "the recovered build resolves the dependency:\n{o3}"
    );
}

// ---------------------------------------------------------------------------
// §16 (implied) vendored: `vendor/modules.txt` skips the fetch entirely and
// builds with -mod=vendor; no proxy reachable, and still green.
// ---------------------------------------------------------------------------

#[test]
fn vendored_go_skips_the_fetch_and_builds_from_the_tree() {
    let s = scratch("vendored");
    let recipes = s.join("recipes");
    let pkg = recipes.join("go-b");
    fs::create_dir_all(&pkg).unwrap();

    let vendored_hello = b"package hello\n\nfunc Hi() string { return \"hi-from-dep\" }\n" as &[u8];
    let files = vec![
        (
            "vendor/modules.txt".to_string(),
            b"# example.org/hello v1.0.0\n## explicit\nexample.org/hello\n".to_vec(),
        ),
        ("vendor/example.org/hello/hello.go".to_string(), vendored_hello.to_vec()),
    ];
    let tar = fixture_tar_gz("main-1.0", &main_fixture(&files, ""));
    let tar_sha = sha256(&tar);
    let (addr, count) = route_server(vec![("/src.tar.gz", Some(tar))]);
    let gobin = toolchain_dir();
    fs::write(
        pkg.join("recipe.toml"),
        recipe(
            &tar_sha,
            &format!("{gobin}/go build -o %{{pkgdir}}/usr/bin/m ./... && %{{pkgdir}}/usr/bin/m"),
            "[fetch.go]\ndir = \"m\"\n",
        )
        .replace("%URL%", &addr)
        .replace("%{GO}", &gobin),
    )
    .unwrap();

    // no NAWA_GOPROXY at all: the default proxy would not resolve in hermes;
    // if anything tried the ecosystem fetch, the build would fail
    let (o, e, ok) = run_fetch_build(&s, None, &[], &[]);
    assert!(ok, "vendored build must succeed with no proxy:\n{o}\n{e}");
    assert!(
        o.contains("hi-from-dep"),
        "the build resolved the vendor tree:\n{o}"
    );
    let work = s.join("build/go-b-1.0-1");
    assert!(
        !work.join("fetch-go.log").exists(),
        "vendored projects must not run the populate"
    );
    // the view exists (the sandbox binder needs it) but no entry copy came in
    let view = work.join("fetch/go");
    assert!(view.is_dir(), "the view dir must exist for the build env");
    assert_eq!(
        fs::read_dir(&view).unwrap().count(),
        0,
        "the vendored view stays empty"
    );
    assert_eq!(
        count.load(Ordering::SeqCst),
        1,
        "only the source tarball was fetched"
    );
}

// ---------------------------------------------------------------------------
// §14C: go.work anywhere from the fetch dir to the workspace root
// ---------------------------------------------------------------------------

/// Remove a tree that may contain read-only published entries (§11): walk
/// first, restoring write bits (what `nawa clean sources` does, Phase 5),
/// then remove.
fn force_rm(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    for e in walkdir::WalkDir::new(dir).into_iter().flatten() {
        let mode = if e.file_type().is_dir() { 0o755 } else { 0o644 };
        let _ = fs::set_permissions(e.path(), fs::Permissions::from_mode(mode));
    }
    fs::remove_dir_all(dir).unwrap();
}
#[test]
fn a_go_workspace_file_is_a_hard_error() {
    let s = scratch("gowork");
    let recipes = s.join("recipes");
    let pkg = recipes.join("go-b");
    fs::create_dir_all(&pkg).unwrap();
    let proxy = s.join("proxy");
    write_proxy_fixture(&proxy);

    let files = vec![("go.work".to_string(), b"go 1.16\n\nuse .\n".to_vec())];
    let tar = fixture_tar_gz("main-1.0", &main_fixture(&files, ""));
    let tar_sha = sha256(&tar);
    let (addr, _) = route_server(vec![("/src.tar.gz", Some(tar))]);
    let gobin = toolchain_dir();
    fs::write(
        pkg.join("recipe.toml"),
        recipe(
            &tar_sha,
            &format!("{gobin}/go build ./..."),
            "[fetch.go]\ndir = \"m\"\n",
        )
        .replace("%URL%", &addr)
        .replace("%{GO}", &gobin),
    )
    .unwrap();

    let (_, e, ok) = run_fetch_build(&s, Some(&proxy), &[], &[]);
    assert!(!ok, "a go.work file must fail the fetch phase");
    assert!(
        e.contains("go.work"),
        "the error must name the offending file:\n{e}"
    );
}

// ---------------------------------------------------------------------------
// §14C: a missing go.sum is a fetch-time hard error
// ---------------------------------------------------------------------------

#[test]
fn a_missing_go_sum_is_a_hard_error() {
    let s = scratch("nosum");
    let recipes = s.join("recipes");
    let pkg = recipes.join("go-b");
    fs::create_dir_all(&pkg).unwrap();
    let proxy = s.join("proxy");
    write_proxy_fixture(&proxy);

    // only go.mod + main.go — no go.sum
    let files = b"package main\n\nfunc main()\n".to_vec();
    let bare = vec![("go.mod".to_string(), go_mod("")), ("main.go".to_string(), files)];
    let tar = fixture_tar_gz("main-1.0", &bare);
    let tar_sha = sha256(&tar);
    let (addr, _) = route_server(vec![("/src.tar.gz", Some(tar))]);
    let gobin = toolchain_dir();
    fs::write(
        pkg.join("recipe.toml"),
        recipe(
            &tar_sha,
            &format!("{gobin}/go build ./..."),
            "[fetch.go]\ndir = \"m\"\n",
        )
        .replace("%URL%", &addr)
        .replace("%{GO}", &gobin),
    )
    .unwrap();

    let (_, e, ok) = run_fetch_build(&s, Some(&proxy), &[], &[]);
    assert!(!ok, "a missing go.sum must fail the fetch phase");
    assert!(
        e.contains("go.sum"),
        "the error must name the missing lockfile:\n{e}"
    );
}


// ---------------------------------------------------------------------------
// Fetch integration with strict sandbox isolation
// ---------------------------------------------------------------------------

#[test]
fn go_fetch_and_offline_build_work_under_sandbox() {
    for isolation in ["sandbox"] {
        let s = scratch_outside_tmp(&format!("isolation-{isolation}"));
        let recipes = s.join("recipes");
        let pkg = recipes.join("go-b");
        fs::create_dir_all(&pkg).unwrap();

        let proxy = s.join("proxy");
        write_proxy_fixture(&proxy);
        let source = fixture_tar_gz("main-1.0", &main_fixture(&[], ""));
        let source_sha = sha256(&source);
        let (addr, _) = route_server(vec![("/src.tar.gz", Some(source))]);
        let gobin = toolchain_dir();
        let install = format!(
            "{gobin}/go build -mod=readonly -o %{{pkgdir}}/m ./... \
             && %{{pkgdir}}/m"
        );
        fs::write(
            pkg.join("recipe.toml"),
            recipe(
                &source_sha,
                &install,
                "[fetch.go]\ndir = \"m\"\n",
            )
            .replace("%URL%", &addr)
            .replace("%{GO}", &gobin),
        )
        .unwrap();

        let (out, err, ok) =
            run_fetch_build_with_isolation(&s, isolation, Some(&proxy), &[], &[]);
        assert!(
            ok,
            "Go fetch/build failed at isolation={isolation}\nstdout:\n{out}\nstderr:\n{err}"
        );
        assert!(
            out.contains("hi-from-dep"),
            "offline Go build did not resolve its pinned module at {isolation}:\n{out}\n{err}"
        );
        assert!(
            s.join("build/go-b-1.0-1/fetch-go.log").is_file(),
            "Go fetch phase did not run at isolation={isolation}"
        );
        let _ = fs::remove_dir_all(&s);
    }
}
