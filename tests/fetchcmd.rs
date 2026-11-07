//! Integration tests for the `nawa fetch` command and the §15 offline
//! gate, §14B `nawa recipe check`, and the §11 `clean sources` rework
//! (fetch-phase.md §16 item 5 / item 3, rev3 items 7–8).
//!
//! Same hermetic shape as tests/fetchgo.rs: external sources arrive through
//! a route-based local HTTP server, the Go ecosystem fetch resolves through
//! `NAWA_GOPROXY=file://…`, and the toolchain location travels through
//! `NAWA_FETCH_PATH` (its documented test-only purpose in
//! `src/fetchrun.rs`). A test process itself holds one source-cache lock to
//! prove `clean sources` never sweeps a lock a live worker holds.

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
    let d = std::env::temp_dir().join(format!("nawa-fetchcmd-{tag}-{}", std::process::id()));
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
// file:// module-proxy fixture (shared with tests/fetchgo.rs)
// ---------------------------------------------------------------------------

fn fixture_proxy_zip(module: &str, version: &str, file: &(&str, &[u8])) -> Vec<u8> {
    let name = format!("{module}@{version}/{}", file.0);
    stored_zip(&name, file.1)
}

/// A tiny ZIP writer: one entry, STORE method (no compression), zero
/// timestamps — deterministic bytes; the tree has no `zip` dependency.
fn stored_zip(name: &str, data: &[u8]) -> Vec<u8> {
    let crc = crc32(data);
    let mut z = Vec::new();
    let mut lfh: Vec<u8> = Vec::new();
    put32(&mut lfh, 0x04034b50);
    put16(&mut lfh, 20);
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
    let cd_off = z.len() as u32;
    let mut cd: Vec<u8> = Vec::new();
    put32(&mut cd, 0x02014b50);
    put16(&mut cd, 20);
    put16(&mut cd, 20);
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
    put16(&mut cd, 0);
    put16(&mut cd, 0);
    put32(&mut cd, 0);
    put32(&mut cd, 0);
    cd.extend(name.as_bytes());
    let eocd: Vec<u8> = {
        let mut e = Vec::new();
        put32(&mut e, 0x06054b50);
        put16(&mut e, 0);
        put16(&mut e, 0);
        put16(&mut e, 1);
        put16(&mut e, 1);
        put32(&mut e, cd.len() as u32);
        put32(&mut e, cd_off);
        put16(&mut e, 0);
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

/// One proxy layout for the `example.org/hello v1.0.0` fixture module.
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

/// The pinned h1 lines of the fixture module — computed once with the real
/// `go` toolchain against this exact proxy zip, then frozen.
const HELLO_GO_SUM_PINS: &[u8] = b"example.org/hello v1.0.0 h1:w02iB5Oh3gIejn36rCHwy7uXSkYM8f/2+DFnxphQV8o=\nexample.org/hello v1.0.0/go.mod h1:Q3qg57GZNcwpC031JBnOm3N7J89qJIgXV+N5zkiIyWw=\n";

fn go_mod() -> Vec<u8> {
    b"module main.fix\n\ngo 1.16\n\nrequire example.org/hello v1.0.0\n".to_vec()
}

/// The main-module files: `go.mod`, `go.sum`, `main.go`.
fn main_fixture() -> Vec<(String, Vec<u8>)> {
    vec![
        ("go.mod".to_string(), go_mod()),
        ("go.sum".to_string(), HELLO_GO_SUM_PINS.to_vec()),
        (
            "main.go".to_string(),
            b"package main\n\nimport (\n\t\"fmt\"\n\t\"example.org/hello\"\n)\n\nfunc main() { fmt.Println(hello.Hi()) }\n".to_vec(),
        ),
    ]
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

// ---------------------------------------------------------------------------
// the two fixture recipes: a new-schema one with [fetch.go], a legacy one
// ---------------------------------------------------------------------------

/// The `[fetch.go]` recipe whose source carries the main module.
fn go_recipe(source_sha: &str, gobin: &str, addr: &str) -> String {
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
url = "{addr}/src.tar.gz"
sha256 = "{source_sha}"
into = "m"
strip_components = 1

[build]
install = """{gobin}/go build -o %{{pkgdir}}/bin/m ./... \
 && mkdir -p %{{pkgdir}}/usr/share/m \
 && %{{pkgdir}}/bin/m > %{{pkgdir}}/usr/share/m/out.txt \
 && cat %{{pkgdir}}/usr/share/m/out.txt"""

[fetch.go]
dir = "m"
"#
    )
}

/// A plain legacy recipe: one remote tarball, pinned sha256.
fn legacy_recipe(addr: &str, tar_sha: &str) -> String {
    format!(
        r#"
[package]
name = "hello"
version = "1.0"
rel = 1
arch = ["x86_64"]
source = "{addr}/hello.tar.gz"
sha256 = "{tar_sha}"

[build]
install = "true"
"#
    )
}

/// The `go` binary location on this host: an explicit `NAWA_FETCH_PATH`
/// wins; otherwise `go` resolved from the test process's own PATH.
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

/// A `nawa fetch` invocation for this tree with the toolchain and proxy envs
/// a fetch may need (NAWA_GOPROXY/NAWA_FETCH_PATH are read by the command
/// itself and injected into the cleared-environment fetch child, §12).
fn run_fetch(s: &Path, args: &[&str], extra_env: &[(&str, &str)]) -> (String, String, bool) {
    let out = nawa()
        .args([
            "--recipes",
            s.join("recipes").to_str().unwrap(),
            "--builddir",
            s.join("build").to_str().unwrap(),
            "fetch",
        ])
        .args(args)
        .env("NAWA_SRCCACHE", s.join("cache"))
        .env("NAWA_FETCH_PATH", toolchain_dir())
        .envs(extra_env.iter().map(|(k, v)| (k.to_string(), v.to_string())))
        .output()
        .unwrap();
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.success(),
    )
}

/// A `nawa build` invocation with the same env plumbing.
fn run_build(s: &Path, args: &[&str], extra_env: &[(&str, &str)]) -> (String, String, bool) {
    let out = nawa()
        .args([
            "--recipes",
            s.join("recipes").to_str().unwrap(),
            "--builddir",
            s.join("build").to_str().unwrap(),
            "build",
        ])
        .args(args)
        .env("NAWA_SRCCACHE", s.join("cache"))
        .env("NAWA_FETCH_PATH", toolchain_dir())
        .envs(extra_env.iter().map(|(k, v)| (k.to_string(), v.to_string())))
        .output()
        .unwrap();
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.success(),
    )
}

fn eco_entries(cache: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(cache.join("fetch/go"))
        .map(|rd| {
            rd.flatten()
                .map(|e| e.path())
                .filter(|p| p.is_dir())
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

// ---------------------------------------------------------------------------
// §16 item 3: `nawa fetch --all` pre-seeds; `build --offline` succeeds from
// the caches; a missing ecosystem entry fails clearly
// ---------------------------------------------------------------------------

#[test]
fn fetch_all_preseeds_and_offline_build_succeeds_or_fails_clearly() {
    let s = scratch("gatewayway");
    let recipes = s.join("recipes");
    fs::create_dir_all(recipes.join("go-b")).unwrap();
    fs::create_dir_all(recipes.join("hello")).unwrap();
    let proxy = s.join("proxy");
    write_proxy_fixture(&proxy);

    let go_tar = fixture_tar_gz("main-1.0", &main_fixture())
       ;
    let hello_tar = fixture_tar_gz("hello-1.0",  &[("hello.txt".to_string(), b"hello world\n".to_vec())]);
    let (addr, count) = route_server(vec![
        ("/src.tar.gz", Some(go_tar.clone())),
        ("/hello.tar.gz", Some(hello_tar.clone())),
    ]);

    fs::write(
        recipes.join("go-b/recipe.toml"),
        go_recipe(&sha256(&go_tar), &toolchain_dir(), &addr),
    )
    .unwrap();
    fs::write(
        recipes.join("hello/recipe.toml"),
        legacy_recipe(&addr, &sha256(&hello_tar)),
    )
    .unwrap();

    // — `nawa fetch --all`: the legacy recipe is included (§15)
    let (out, err, ok) =
        run_fetch(&s, &["--all", "--isolation", "env"], &[("NAWA_GOPROXY", &format!("file://{}", proxy.display()))]);
    assert!(ok, "fetch --all failed\nstdout:\n{out}\nstderr:\n{err}");
    assert!(out.contains("2 recipe(s)"), "both recipes fetched:\n{out}");
    assert!(out.contains("hello"), "the legacy recipe is in the --all set:\n{out}");

    // the caches: legacy pinned-tarball entry + content-addressed source
    // entries + exactly one published Go entry with marker/lock/no-write
    let cache = s.join("cache");
    let legacy = cache.join(format!(
        "{}-hello.tar.gz",
        &sha256(&hello_tar)[..16]
    ));
    assert!(legacy.is_file(), "the legacy entry must be cached: {out}");
    assert!(cache.join("sha256").is_dir(), "content-addressed cache exists");
    let entries = eco_entries(&cache);
    assert_eq!(entries.len(), 1, "exactly one ecosystem entry after fetch");
    let entry = &entries[0];
    let key = entry.file_name().unwrap().to_string_lossy().to_string();
    assert_eq!(key.len(), 64, "the key is a full sha256: {key}");
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = entry.symlink_metadata().unwrap().permissions().mode();
        assert_eq!(mode & 0o222, 0, "the published entry must have no write bits");
    }
    assert!(
        entry.join("nawa-complete").is_file(),
        "the completion marker must exist in the entry"
    );
    assert!(cache.join("fetch/go").join(format!("{key}.lock")).is_file());

    let hits = count.load(Ordering::SeqCst);
    assert!(hits >= 2, "both sources came through the route server");

    // — `nawa build --offline go-b` succeeds from the caches, connecting to
    // nothing (§16 item 3)
    let work = s.join("build/go-b-1.0-1");
    let _ = fs::remove_dir_all(&work);
    let (out, err, ok) =
        run_build(&s, &["go-b", "--isolation", "env", "--offline"], &[("NAWA_GOPROXY", &format!("file://{}", proxy.display()))]);
    assert!(ok, "offline build failed\nstdout:\n{out}\nstderr:\n{err}");
    assert!(
        out.contains("hi-from-dep"),
        "the offline build resolved the Go dependency out of the entry:\n{out}"
    );
    assert_eq!(
        count.load(Ordering::SeqCst),
        hits,
        "the --offline build must not re-download the source"
    );
    assert!(
        !work.join("fetch-go.log").exists(),
        "no populate may run offline"
    );

    // — removing the ecosystem entry makes the offline build fail clearly,
    // naming the entry it needs
    fs::rename(cache.join("fetch/go"), s.join("go-away")).unwrap();
    let (out, err, ok) =
        run_build(&s, &["go-b", "--isolation", "env", "--offline"], &[("NAWA_GOPROXY", &format!("file://{}", proxy.display()))]);
    assert!(!ok, "the offline build must fail without the ecosystem entry:\n{out}");
    assert!(
        err.contains("the Go module cache entry"),
        "the failure must name the missing entry:\nstdout:\n{out}\nstderr:\n{err}"
    );
    assert!(
        err.contains("fetch/go"),
        "the failure must name the expected path:\n{err}"
    );

    // recovery: back online (no --offline), the same build repopulates the
    // entry and succeeds — the clear failure never leaves damage behind
    let (out, err, ok) =
        run_build(&s, &["go-b", "--isolation", "env"], &[("NAWA_GOPROXY", &format!("file://{}", proxy.display()))]);
    assert!(ok, "the online recovery build failed\nstdout:\n{out}\nstderr:\n{err}");
    assert_eq!(eco_entries(&cache).len(), 1, "the entry was republished");
}

// ---------------------------------------------------------------------------
// §15: `fetch --offline` is verify-only — cold failure names the artifact,
// warm success connects to nothing; `--sources-only` skips the ecosystems
// ---------------------------------------------------------------------------

#[test]
fn fetch_offline_is_verify_only() {
    let s = scratch("verifyonly");
    let recipes = s.join("recipes");
    fs::create_dir_all(recipes.join("go-b")).unwrap();
    fs::create_dir_all(recipes.join("hello")).unwrap();
    let proxy = s.join("proxy");
    write_proxy_fixture(&proxy);

    let go_tar = fixture_tar_gz("main-1.0", &main_fixture());
    let hello_tar = fixture_tar_gz("hello-1.0",  &[("hello.txt".to_string(), b"hello world\n".to_vec())]);
    let (addr, count) = route_server(vec![
        ("/src.tar.gz", Some(go_tar.clone())),
        ("/hello.tar.gz", Some(hello_tar.clone())),
    ]);

    fs::write(
        recipes.join("go-b/recipe.toml"),
        go_recipe(&sha256(&go_tar), &toolchain_dir(), &addr),
    )
    .unwrap();
    fs::write(
        recipes.join("hello/recipe.toml"),
        legacy_recipe(&addr, &sha256(&hello_tar)),
    )
    .unwrap();

    let prox = format!("file://{}", proxy.display());
    // (a) cold + --offline: a hard error naming the artifact, no connection
    let (out, err, ok) = run_fetch(&s, &["--all", "--isolation", "env", "--offline"], &[]);
    assert!(!ok, "the cold --offline fetch must fail:\n{out}");
    assert!(
        err.contains("offline mode:"),
        "the cold failure is the offline gate:\n{err}"
    );
    assert!(
        err.contains("is not in the pre-fetch cache"),
        "the message names the artifact: {err}"
    );
    assert_eq!(
        count.load(Ordering::SeqCst),
        0,
        "the verify-only run must not connect to anything"
    );

    // (b) --sources-only: sources are downloaded and verified, the
    // ecosystem step is skipped entirely (needs no toolchain)
    let (out, err, ok) = run_fetch(&s, &["--all", "--isolation", "env", "--sources-only"], &[("NAWA_GOPROXY", &prox)]);
    assert!(ok, "sources-only fetch failed\nstdout:\n{out}\nstderr:\n{err}");
    assert!(out.contains("(sources only)"), "the summary states the mode:\n{out}");
    let cache = s.join("cache");
    assert!(
        cache.join("sha256").is_dir(),
        "the content-addressed source entries exist"
    );
    assert!(
        cache.join(format!("{}-hello.tar.gz", &sha256(&hello_tar)[..16])).is_file(),
        "the legacy entry exists"
    );
    assert!(
        !cache.join("fetch").exists(),
        "--sources-only must not touch the ecosystem cache"
    );
    let hits = count.load(Ordering::SeqCst);
    assert!(hits >= 2, "both sources came through the route server");

    // (c) warm sources, cold ecosystem entries: the offline gate fails
    // naming the Go entry (not the source URL this time)
    let (out, err, ok) = run_fetch(&s, &["--all", "--isolation", "env", "--offline"], &[("NAWA_GOPROXY", &prox)]);
    assert!(!ok, "the offline fetch must fail with cold ecosystem entries:\n{out}");
    assert!(
        err.contains("the Go module cache entry is not populated"),
        "the failure names the Go entry:\n{err}"
    );
    assert_eq!(
        count.load(Ordering::SeqCst),
        hits,
        "the cold-entries failure connects to nothing either"
    );

    // (d) the online fetch populates the ecosystem cache
    let (out, err, ok) = run_fetch(&s, &["--all", "--isolation", "env"], &[("NAWA_GOPROXY", &prox)]);
    assert!(ok, "the online fetch failed\nstdout:\n{out}\nstderr:\n{err}");
    assert_eq!(eco_entries(&cache).len(), 1, "one ecosystem entry");
    let hits = count.load(Ordering::SeqCst);

    // (e) warm + --offline: pure verification, zero connections, exit 0
    let (out, err, ok) = run_fetch(&s, &["--all", "--isolation", "env", "--offline"], &[]);
    assert!(ok, "the warm --offline fetch failed\nstdout:\n{out}\nstderr:\n{err}");
    assert_eq!(
        count.load(Ordering::SeqCst),
        hits,
        "verify-only must connect to nothing"
    );
}

// ---------------------------------------------------------------------------
// rev3 item 7: a legacy recipe's cold-cache --offline build fails with zero
// listener connections
// ---------------------------------------------------------------------------

#[test]
fn legacy_offline_build_fails_with_zero_connections() {
    let s = scratch("legacycold");
    let recipes = s.join("recipes");
    fs::create_dir_all(recipes.join("hello")).unwrap();
    let hello_tar = fixture_tar_gz("hello-1.0",  &[("hello.txt".to_string(), b"hello world\n".to_vec())]);
    let (addr, count) = route_server(vec![("/hello.tar.gz", Some(hello_tar.clone()))]);
    fs::write(
        recipes.join("hello/recipe.toml"),
        legacy_recipe(&addr, &sha256(&hello_tar)),
    )
    .unwrap();

    let (out, err, ok) = run_build(&s, &["hello", "--isolation", "none", "--offline"], &[]);
    assert!(!ok, "the cold --offline build must fail:\n{out}");
    assert!(
        err.contains("offline mode:") && err.contains("hello.tar.gz"),
        "the failure names the not-cached URL:\nstdout:\n{out}\nstderr:\n{err}"
    );
    assert_eq!(
        count.load(Ordering::SeqCst),
        0,
        "the offline gate must bail before any connection attempt"
    );

    // and the environment form behaves the same as the flag (§15)
    let (out, err, ok) = run_build(&s, &["hello", "--isolation", "none"], &[("NAWA_OFFLINE", "1")]);
    assert!(!ok, "NAWA_OFFLINE=1 must gate the build too:\n{out}");
    assert!(err.contains("offline mode:"), "{err}");
    assert_eq!(count.load(Ordering::SeqCst), 0, "no connection under NAWA_OFFLINE either");
}

// ---------------------------------------------------------------------------
// §14B: `recipe check` warns on every covered condition and exits 0
// ---------------------------------------------------------------------------

#[test]
fn recipe_check_warns_per_condition_with_exit_zero() {
    let s = scratch("fourteenb");
    let recipes = s.join("recipes");

    // the naughty recipe: one fixture per §14B warning class
    let assets = recipes.join("naughty/assets");
    fs::create_dir_all(&assets).unwrap();
    fs::write(assets.join("recipe.toml"), "[package]\nname = \"hidden-one\"\nversion = \"0.0\"\n").unwrap();
    // 33 MiB file — 1 KiB over the §14B size threshold
    fs::write(assets.join("heavy.bin"), vec![0u8; 32 * 1024 * 1024 + 1024]).unwrap();
    std::os::unix::fs::symlink("heavy.bin", assets.join("chain")).unwrap();

    fs::write(
        recipes.join("naughty/recipe.toml"),
        r#"
[package]
name = "naughty"
version = "1.0"
bogus-key = "ignored"
# new-schema tables (assets/fetch) below -> the §19 schema marker must be
# present even for a recipe we only ever *check* (§14B runs local loads)
source = "local:/nawa-schema-2-requires-a-newer-nawa"
source_date_epoch = 1750000000

[nonsense]
x = 1

[build]
compile = """go mod download ./...
echo %{frobnicate}"""
install = "true"

[assets]
include = ["assets", "ghost"]

[fetch.go]
dir = "m"

[sandbox]
network = true
"#,
    )
    .unwrap();

    // the clean recipe: none of the classes fire
    fs::create_dir_all(recipes.join("cleanone")).unwrap();
    fs::write(
        recipes.join("cleanone/recipe.toml"),
        r#"
[package]
name = "cleanone"
version = "1.0"

[build]
install = "true"
"#,
    )
    .unwrap();

    let run_check = |args: &[&str]| {
        let out = nawa()
            .args([
                "--recipes",
                recipes.to_str().unwrap(),
                "--builddir",
                s.join("build").to_str().unwrap(),
                "recipe",
                "check",
            ])
            .args(args)
            .output()
            .unwrap();
        (
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
            out.status.success(),
        )
    };

    let (out, err, ok) = run_check(&["naughty"]);
    assert!(ok, "§14B is a warnings-only command: it must exit 0\nstdout:\n{out}\nstderr:\n{err}");
    for needle in [
        "asset path `ghost` is missing locally",
        "is a symlink",
        "a recipe.toml inside an asset path",
        "asset file",
        "unknown top-level table [nonsense]",
        "unknown key `bogus-key` under [package]",
        "unknown %{frobnicate} in build.compile",
        "go mod download",
        "[fetch.*] together with",
    ] {
        assert!(
            err.contains(needle),
            "missing §14B warning for {needle:?}\nstdout:\n{out}\nstderr:\n{err}"
        );
    }

    // the clean recipe reports nothing
    let (out, err, ok) = run_check(&["cleanone"]);
    assert!(ok, "{err}");
    assert!(
        out.contains("0 warning(s)"),
        "a clean recipe must be silent:\nstdout:\n{out}\nstderr:\n{err}"
    );
    assert!(!err.contains("warning:"), "no stray warnings: {err}");

    // --all covers every recipe.toml in the tree — including the one hidden
    // inside the assets dir (the very hazard §14B warns about), so 3
    let (out, _err, ok) = run_check(&["--all"]);
    assert!(ok, "--all must exit 0 even with findings");
    assert!(out.contains("3 recipe(s)"), "\n{out}");
}

// ---------------------------------------------------------------------------
// rev3 item 8: `clean sources` sweeps only Nawa-owned subsets, never the
// (possibly shared) cache root nor unrelated files; read-only entries get
// removed again only because u+w is restored
// ---------------------------------------------------------------------------

#[test]
fn clean_sources_sweeps_only_nawa_owned_subsets() {
    for dry in [false, true] {
        let s = scratch(if dry { "cleandry" } else { "cleanwet" });
        let cache = s.join("cache");
        let hex16 = &sha256(b"clean-sources-legacy-entry")[..16];
        fs::create_dir_all(cache.join("sha256")).unwrap();
        fs::create_dir_all(cache.join("fetch/go")).unwrap();
        fs::create_dir_all(cache.join("person")).unwrap();

        // the legacy pinned-tarball entry
        fs::write(cache.join(format!("{hex16}-legacy.tar.gz")), b"legacy tarball bytes\n").unwrap();
        // a content-addressed entry
        let digest = sha256(b"content-addressed-bytes");
        fs::write(cache.join("sha256").join(&digest), b"content-addressed-bytes\n").unwrap();
        // a read-only Go ecosystem entry (published with no write bits, §11)
        let key = sha256(b"go-entry-workspace-key");
        let entry = cache.join("fetch/go").join(&key);
        fs::create_dir_all(entry.join("pkg/mod")).unwrap();
        fs::write(entry.join("nawa-complete"), b"").unwrap();
        fs::write(entry.join("pkg/mod/dep.v1"), b"dep\n").unwrap();
        // the entry's own lock — stale when nobody holds it
        fs::write(cache.join("fetch/go").join(format!("{key}.lock")), b"").unwrap();
        // leftovers of interrupted downloads
        fs::write(cache.join(format!("{hex16}-legacy.tar.gz.part")), b"partial\n").unwrap();
        fs::write(cache.join("dead.lock"), b"").unwrap();

        // what must survive
        fs::write(cache.join("keepme.txt"), b"not ours\n").unwrap();
        fs::write(cache.join("person/notes.txt"), b"foreign data\n").unwrap();
        let living = cache.join("live.lock");
        fs::write(&living, b"held by a live worker\n").unwrap();
        // the held lock: taken in THIS process (the child opens its own
        // file description, so the probe cannot steal it)
        let held = std::fs::OpenOptions::new()
            .read(true)
            .open(&living)
            .unwrap();
        rustix::fs::flock(&held, rustix::fs::FlockOperation::NonBlockingLockExclusive).unwrap();

        // read-only bits on the Go entry: only the u+w restore lets the
        // sweep succeed at all
        {
            use std::os::unix::fs::PermissionsExt;
            for p in [
                entry.join("nawa-complete"),
                entry.join("pkg/mod/dep.v1"),
            ] {
                let mut perm = fs::metadata(&p).unwrap().permissions();
                perm.set_mode(0o444);
                fs::set_permissions(&p, perm).unwrap();
            }
            for p in [entry.join("pkg"), entry.join("pkg/mod")].into_iter().chain([entry.clone()]) {
                let mut perm = fs::metadata(&*p).unwrap().permissions();
                perm.set_mode(0o555);
                fs::set_permissions(&p, perm).unwrap();
            }
        }

        let args: &[&str] = if dry { &["clean", "sources", "--dry-run"] } else { &["clean", "sources"] };
        let (out, err, ok) = nawa()
            .args([
                "--recipes",
                s.join("recipes").to_str().unwrap(),
                "--builddir",
                s.join("build").to_str().unwrap(),
            ])
            .args(args)
            .env("NAWA_SRCCACHE", &cache)
            .output()
            .map(|o| (String::from_utf8_lossy(&o.stdout).into_owned(), String::from_utf8_lossy(&o.stderr).into_owned(), o.status.success()))
            .unwrap();
        drop(held); // release the probe lock before any assertion unwinds
        assert!(ok, "clean sources failed\nstdout:\n{out}\nstderr:\n{err}");

        if dry {
            assert!(out.contains("would free"), "\n{out}");
            // the dry run removes nothing
            assert!(cache.join(format!("{hex16}-legacy.tar.gz")).is_file());
            assert!(cache.join("sha256").join(&digest).is_file());
            assert!(entry.join("nawa-complete").is_file());
            assert!(cache.join(format!("{hex16}-legacy.tar.gz.part")).is_file());
            assert!(cache.join("dead.lock").is_file());
            assert!(cache.join("keepme.txt").is_file());
            continue;
        }

        // swept — Nawa-owned subsets only
        assert!(!cache.join(format!("{hex16}-legacy.tar.gz")).exists());
        assert!(!cache.join("sha256").exists(), "the content-addressed dir is swept");
        assert!(!cache.join("fetch").exists(), "the ecosystem dir is swept");
        assert!(!cache.join(format!("{hex16}-legacy.tar.gz.part")).exists());
        assert!(!cache.join("dead.lock").exists(), "a stale lock is a leftover");

        // retained — the cache root itself and everything foreign
        assert!(cache.is_dir(), "the cache root is never removed (§11)");
        assert!(cache.join("keepme.txt").is_file());
        assert!(cache.join("person/notes.txt").is_file());
        assert!(living.is_file(), "a flock-held lock belongs to a live worker");
        assert!(
            out.contains("not Nawa-owned"),
            "the notes name what was left:\n{out}"
        );
        // ... and the read-only entry was removed → the u+w restore worked
        assert!(!entry.exists(), "the read-only entry must be removable");
    }
}
