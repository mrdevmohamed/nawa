//! Integration tests for the Cargo fetch backend (fetch-phase.md §8, §16
//! "Cargo").
//!
//! Hermetic: the external source arrives through a route-based HTTP server
//! and every `cargo fetch` resolves against a git dependency clonable from
//! a `file://` path inside the scratch — the only real fetch flow that
//! stays on the loop host. Registry *redirects* live behind the §8
//! source-tree rules (they are refused in v1), so live-network registry
//! checks belong in the separate, non-blocking CI job (§16). What Is
//! exercised here is the real machinery: `cargo fetch --locked`, the
//! CARGO_HOME cache entry, the offline build from the per-build view, and
//! the §8 config rules.
//!
//! Tests run at `--isolation env`: the fetch runs as a plain
//! cleared-environment command (§12). The view is a copy, never
//! hard-linked (§8: Cargo may write `.package-cache` even offline).

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
        "NAWA_GOPROXY", "NAWA_FETCH_PATH", "NAWA_FETCH_TIMEOUT",
        "CARGO_HOME",
        // the root CI job points CARGO_TARGET_DIR at /var/tmp for the *outer*
        // `cargo test`; inherited by the recipe's own `cargo build` it would
        // move target/release/ away from where the recipes copy it from
        "CARGO_TARGET_DIR", "CARGO_BUILD_TARGET_DIR",
    ] {
        c.env_remove(k);
    }
    c.env("NAWA_SYSCONFDIR", std::env::temp_dir().join("nawa-test-no-sysconf"));
    c.env("NAWA_INSTALL_MODE", "build");
    c.current_dir(std::env::temp_dir());
    c
}

fn scratch(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("nawa-fetchcargo-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

// Strict sandbox tests need a build directory outside the sandbox's private /tmp.
fn scratch_outside_tmp(tag: &str) -> PathBuf {
    let d = PathBuf::from("/var/tmp").join(format!("nawa-fetchcargo-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

fn sha256(bytes: &[u8]) -> String {
    use sha2::Digest;
    hex::encode(sha2::Sha256::digest(bytes))
}

/// Serve a fixed route table (path → body); counts accepted connections so
/// tests can prove who talked to the network. Same shape as the other
/// suite harnesses.
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
// cargo helpers
// ---------------------------------------------------------------------------

/// Where the host `cargo` lives. `NAWA_FETCH_PATH`'s first element wins
/// (an explicit test override); otherwise the *raw toolchain directory* is
/// preferred over the rustup shim: the fetch run (§7) starts from a cleared
/// environment where the shim can reach neither `RUSTUP_HOME` nor a
/// `~/.rustup` through the scratch `HOME`, so the tests point straight at
/// `<RUSTUP_HOME>/toolchains/<tc>/bin` — mirroring `/usr/local/go/bin` in
/// the Go suite. Production toolchains are real packages and land in
/// `/usr/bin`, where plain `command -v cargo` still finds them.
fn cargo_dir() -> String {
    if let Ok(p) = std::env::var("NAWA_FETCH_PATH") {
        if !p.trim().is_empty() {
            return p.trim().split(':').next().unwrap().to_string();
        }
    }
    if let Some(dir) = rustup_toolchain_dir() {
        return dir;
    }
    let probed = Command::new("sh").args(["-c", "command -v cargo"]).output().ok();
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

/// `<RUSTUP_HOME>/toolchains/<tc>/bin` for some installed toolchain that
/// carries a plain `cargo` (deterministic: sorted, first). `None` when this
/// host does not manage cargo through rustup.
fn rustup_toolchain_dir() -> Option<String> {
    let rh = match std::env::var("RUSTUP_HOME") {
        Ok(v) if !v.trim().is_empty() => v,
        _ => format!("{}/.rustup", std::env::var("HOME").ok()?),
    };
    let mut bins: Vec<PathBuf> = fs::read_dir(Path::new(&rh).join("toolchains"))
        .ok()?
        .flatten()
        .map(|e| e.path().join("bin"))
        .filter(|p| p.join("cargo").is_file())
        .collect();
    bins.sort();
    let first = bins.into_iter().next()?;
    Some(first.to_string_lossy().into_owned())
}

/// One `git -C <dir>` invocation with a pinned identity.
fn git_in(dir: &Path, args: &[&str]) -> (bool, String) {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("HOME", dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_EMAIL", "nawa-test@example.invalid")
        .env("GIT_AUTHOR_NAME", "nawa")
        .env("GIT_COMMITTER_EMAIL", "nawa-test@example.invalid")
        .env("GIT_COMMITTER_NAME", "nawa")
        .output()
        .unwrap();
    (out.status.success(), String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// A git-dependency repo fixture: a crate whose `greeting()` returns
/// `"dep-ok-{marker}"`. Returns the pinned commit.
fn git_dep_repo(dir: &Path, marker: &str) -> String {
    fs::create_dir_all(dir.join("src")).unwrap();
    fs::write(
        dir.join("Cargo.toml"),
        b"[package]\nname = \"testdep\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    fs::write(
        dir.join("src/lib.rs"),
        format!("pub fn greeting() -> &'static str {{ \"dep-ok-{marker}\" }}\n").as_bytes(),
    )
    .unwrap();
    let _ = fs::remove_dir_all(dir.join(".git"));
    let _ = fs::remove_dir_all(dir.join(".git.lock"));
    assert!(git_in(dir, &["init", "-q", "-b", "master", "."]).0);
    assert!(git_in(dir, &["add", "-A"]).0);
    assert!(git_in(dir, &["commit", "-q", "-m", marker]).0);
    let (ok, rev) = git_in(dir, &["rev-parse", "HEAD"]);
    assert!(ok, "rev-parse failed");
    rev
}

/// A throwaway `cargo` invocation for fixture cooking (lock generation),
/// with `CARGO_HOME`/`HOME` pinned to a scratch so the default `~/.cargo`
/// is never touched.
fn cargo_cmd(cg_home: &Path) -> Command {
    let mut c = Command::new(Path::new(&cargo_dir()).join("cargo"));
    c.env("CARGO_HOME", cg_home)
        .env("HOME", cg_home.parent().unwrap_or_else(|| Path::new("/tmp")));
    c
}

/// Generate the lockfile of a fixture crate with the real cargo. The lock
/// embeds the absolute `file://` dependency URL, so it is cooked per
/// scratch rather than pinned as a constant. The cwd is the fixture's own
/// directory: this repo's checkout is itself a *vendored* cargo build (its
/// `.cargo/config.toml` redirects crates-io), and a cargo launched from the
/// test's cwd would load that config on top of the fixture's.
fn generate_lock(manifest_dir: &Path, cg_home: &Path) -> Vec<u8> {
    let out = cargo_cmd(cg_home)
        .current_dir(manifest_dir)
        .args([
            "generate-lockfile",
            "--manifest-path",
            &manifest_dir.join("Cargo.toml").to_string_lossy(),
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "cargo generate-lockfile failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    fs::read(manifest_dir.join("Cargo.lock")).unwrap()
}

/// The main crate of the git-dependency flow.
fn prepare_main(main: &Path, repo: &Path) {
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(
        main.join("Cargo.toml"),
        format!(
            "[package]\nname = \"testmain\"\nversion = \"0.1.0\"\n\n[dependencies]\ntestdep = {{ git = \"file://{}\", branch = \"master\" }}\n",
            repo.display()
        ),
    )
    .unwrap();
    fs::write(
        main.join("src/main.rs"),
        b"fn main() { println!(\"{}\", testdep::greeting()) }\n",
    )
    .unwrap();
}

/// The `[fetch.cargo]` test recipe: sources into `m` (workdir = "m"), and
/// `install` runs cargo by absolute path.
fn recipe(source_sha: &str, addr: &str, install: &str) -> String {
    format!(
        r#"
[package]
name = "cargo-b"
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

[fetch.cargo]
dir = "m"

[build]
install = """
{install}
"""
"#
    )
}

fn write_recipe(pkg: &Path, text: &str) {
    fs::create_dir_all(pkg).unwrap();
    fs::write(pkg.join("recipe.toml"), text).unwrap();
}

/// One build of `cargo-b` at `--isolation env` with the standard env.
fn run_fetch_build(
    s: &Path,
    extra_env: &[(&str, &str)],
    extra_args: &[&str],
) -> (String, String, bool) {
    run_fetch_build_with_isolation(s, "env", extra_env, extra_args)
}

fn run_fetch_build_with_isolation(
    s: &Path,
    isolation: &str,
    extra_env: &[(&str, &str)],
    extra_args: &[&str],
) -> (String, String, bool) {
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
        .arg("cargo-b")
        .env("NAWA_SRCCACHE", s.join("srccache"))
        .env("NAWA_FETCH_PATH", cargo_dir())
        .envs(extra_env.iter().map(|(k, v)| (k.to_string(), v.to_string())))
        .output()
        .unwrap();
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.success(),
    )
}

/// Serve a main crate (`manifest_dir` + lock bytes), run one online build,
/// and expect `needle` in the build output.
fn serve_and_build(
    s: &Path,
    manifest_dir: &Path,
    lock: &[u8],
    cargo: &str,
    needle: &str,
) {
    let files = vec![
        ("Cargo.toml".to_string(), fs::read(manifest_dir.join("Cargo.toml")).unwrap()),
        ("Cargo.lock".to_string(), lock.to_vec()),
        ("src/main.rs".to_string(), fs::read(manifest_dir.join("src/main.rs")).unwrap()),
    ];
    let tar = fixture_tar_gz("main-1.0", &files);
    let tar_sha = sha256(&tar);
    let (addr, _) = route_server(vec![("/src.tar.gz", Some(tar))]);
    let install = format!(
        "export PATH={cargo}:$PATH && cargo build --release --locked --offline \
         && mkdir -p %{{pkgdir}}/usr/bin \
         && cp target/release/testmain %{{pkgdir}}/usr/bin/ \
         && %{{pkgdir}}/usr/bin/testmain"
    );
    write_recipe(&s.join("recipes/cargo-b"), &recipe(&tar_sha, &addr, &install));
    let (out, err, ok) = run_fetch_build(s, &[], &[]);
    assert!(ok, "fetch build failed\nstdout:\n{out}\nstderr:\n{err}");
    assert!(
        out.contains(needle),
        "expected `{needle}` in the build output:\n{out}\n{err}"
    );
}

/// The `<srccache>/fetch/cargo` entry keys, sorted.
fn cargo_keys(s: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = fs::read_dir(s.join("srccache/fetch/cargo"))
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    v.sort();
    v
}

/// (path, mode, sha256) snapshot of a published entry: proves immutability
/// byte-for-byte across builds that write `.package-cache` into the view.
fn entry_snapshot(entry: &Path) -> Vec<(String, u32, String)> {
    use std::os::unix::fs::PermissionsExt;
    let mut v: Vec<(String, u32, String)> = Vec::new();
    for e in walkdir::WalkDir::new(entry).into_iter().filter_map(|e| e.ok()) {
        let mode = e.metadata().unwrap().permissions().mode() & 0o777;
        let sha = if e.file_type().is_dir() {
            String::new()
        } else {
            sha256(&fs::read(e.path()).unwrap())
        };
        v.push((e.path().display().to_string(), mode, sha));
    }
    v.sort();
    v
}

/// Remove a tree with read-only published entries (§11): restore write
/// bits first, like Phase 5's `clean sources` will.
fn force_rm(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    for e in walkdir::WalkDir::new(dir).into_iter().flatten() {
        let mode = if e.file_type().is_dir() { 0o755 } else { 0o644 };
        let _ = fs::set_permissions(e.path(), fs::Permissions::from_mode(mode));
    }
    fs::remove_dir_all(dir).unwrap();
}

// ---------------------------------------------------------------------------
// §16 Cargo 1: fetch → offline build → down-freeance cache hits
// ---------------------------------------------------------------------------

#[test]
fn cargo_backend_fetches_builds_offline_and_hits_the_cache() {
    let s = scratch("backend");
    let repo = s.join("dep");
    let cargo = cargo_dir();
    let rev = git_dep_repo(&repo, "v1");

    let cg_home = s.join("cg-setup");
    fs::create_dir_all(&cg_home).unwrap();
    let main = s.join("main");
    prepare_main(&main, &repo);
    let lock = generate_lock(&main, &cg_home);
    assert!(
        String::from_utf8_lossy(&lock)
            .contains(&format!("git+file://{}?branch=master#{rev}", repo.display())),
        "the generated lock must pin the fixture rev:\n{}",
        String::from_utf8_lossy(&lock)
    );

    let install = format!(
        "export PATH={cargo}:$PATH && cargo build --release --locked --offline \
         && ./target/release/testmain \
         && printf 'CH=%s NET=%s GM=[%s]' \"$CARGO_HOME\" \"$CARGO_NET_OFFLINE\" \"$GOMODCACHE\" \
         && mkdir -p %{{pkgdir}}/usr/bin && cp target/release/testmain %{{pkgdir}}/usr/bin/"
    );
    let files: Vec<(String, Vec<u8>)> = vec![
        ("Cargo.toml".to_string(), fs::read(main.join("Cargo.toml")).unwrap()),
        ("Cargo.lock".to_string(), lock.clone()),
        ("src/main.rs".to_string(), fs::read(main.join("src/main.rs")).unwrap()),
    ];
    let tar = fixture_tar_gz("main-1.0", &files);
    let tar_sha = sha256(&tar);
    let (addr, count) = route_server(vec![("/src.tar.gz", Some(tar))]);
    write_recipe(&s.join("recipes/cargo-b"), &recipe(&tar_sha, &addr, &install));
    let (out, err, ok) = run_fetch_build(&s, &[], &[]);
    assert!(ok, "run 1 failed\nstdout:\n{out}\nstderr:\n{err}");
    assert!(out.contains("dep-ok-v1"), "the offline build resolved the git dep:\n{out}\n{err}");
    assert!(out.contains("NET=true"), "CARGO_NET_OFFLINE must reach the build:\n{out}\n{err}");
    assert!(out.contains("GM=[]"), "Go env names must not ride a cargo recipe:\n{out}\n{err}");
    assert!(
        out.contains("CH=") && out.contains("fetch/cargo"),
        "CARGO_HOME must be the per-build view:\n{out}\n{err}"
    );
    assert!(
        err.contains("git dependencies without registry checksums"),
        "the §8 git-dependency warning must appear at fetch time:\n{err}"
    );

    let work = s.join("build/cargo-b-1.0-1");
    assert!(work.join("fetch-cargo.log").is_file(), "the populate ran once (log)");
    let hits1 = count.load(Ordering::SeqCst);
    assert!(hits1 >= 1, "the source tarball came from the route server");

    // — the published entry (§11): key + marker + flock + no write bits
    let keys = cargo_keys(&s);
    assert_eq!(keys.len(), 1, "exactly one entry after run 1: {keys:?}");
    let entry = keys[0].clone();
    let key = entry.file_name().unwrap().to_string_lossy().to_string();
    assert_eq!(key.len(), 64, "the key is a full sha256: {key}");
    use std::os::unix::fs::PermissionsExt;
    let mode = entry.symlink_metadata().unwrap().permissions().mode();
    assert_eq!(mode & 0o222, 0, "published entries carry no write bits");
    assert!(entry.join("nawa-complete").is_file(), "the completion marker exists");
    assert!(
        s.join("srccache/fetch/cargo").join(format!("{key}.lock")).is_file(),
        "the hold-open flock file exists"
    );
    assert!(entry.join("git").is_dir(), "cargo fetch populated git/ into the entry");
    assert!(
        entry.join("git/db").is_dir(),
        "the Git database must be directly under the published CARGO_HOME root"
    );

    // — the per-build view is a WRITABLE COPY (§8): the marker never crosses
    let view = work.join("fetch/cargo");
    assert!(view.join("git").is_dir(), "the view carries the git tree");
    assert!(
        view.join("git/db").is_dir(),
        "the offline build view must expose Cargo's Git database at CARGO_HOME/git/db"
    );
    assert!(!view.join("nawa-complete").exists(), "the marker stays in the entry");
    let snapshot = entry_snapshot(&entry);

    // — run 2: --offline with the work dir wiped — served from the entry;
    // no populate, no re-download, and cargo's cache mutation lands in the
    // view instead of the entry
    fs::remove_dir_all(&work).unwrap();
    let (out2, err2, ok2) = run_fetch_build(&s, &[("NAWA_OFFLINE", "1")], &[]);
    assert!(ok2, "run 2 (--offline) failed\nstdout:\n{out2}\nstderr:\n{err2}");
    assert!(out2.contains("dep-ok-v1"), "the offline run rebuilt from the entry:\n{out2}\n{err2}");
    assert!(!work.join("fetch-cargo.log").exists(), "no populate on the second build");
    assert_eq!(count.load(Ordering::SeqCst), hits1, "nothing was re-downloaded");
    assert!(
        view.join(".package-cache").exists() || view.join(".package-cache-mutate").exists(),
        "cargo's cache mutation must land in the disposable view"
    );
    assert_eq!(
        snapshot,
        entry_snapshot(&entry),
        "the published entry must stay byte-identical across builds"
    );

    // §16 Cargo 1: Cargo.lock unchanged through the whole cycle
    let staged = work.join("src/m/Cargo.lock");
    assert_eq!(fs::read(&staged).unwrap(), lock, "the lock must come out byte-identical");

    // — run 3: --use-cache — the sidecar build id (fetch-cargo=1) matches
    // and the whole build is skipped
    let hits3 = count.load(Ordering::SeqCst);
    let (out3, err3, ok3) = run_fetch_build(&s, &[], &["--use-cache"]);
    assert!(ok3, "use-cache run failed\nstdout:\n{out3}\nstderr:\n{err3}");
    assert!(
        out3.contains("build cache hit"),
        "the sidecar build id with fetch-cargo=1 must match:\n{out3}\n{err3}"
    );
    assert_eq!(count.load(Ordering::SeqCst), hits3, "a cache hit touches no network");
}

// ---------------------------------------------------------------------------
// §11: the cargo key covers the lockfile bytes AND the Cargo version string
// ---------------------------------------------------------------------------

#[test]
fn the_cargo_key_follows_the_lockfile_bytes() {
    let s = scratch("keylock");
    let cargo = cargo_dir();
    let cg_home = s.join("cg-setup");
    fs::create_dir_all(&cg_home).unwrap();
    let repo = s.join("dep");

    // fixture rev 1
    let rev1 = git_dep_repo(&repo, "v1");
    let main = s.join("main");
    prepare_main(&main, &repo);
    let lock1 = generate_lock(&main, &cg_home);
    assert!(String::from_utf8_lossy(&lock1).contains(&rev1));
    serve_and_build(&s, &main, &lock1, &cargo, "dep-ok-v1");
    assert_eq!(cargo_keys(&s).len(), 1, "one entry for the first lock");

    // rev 2: a new commit with different content
    fs::write(repo.join("src/lib.rs"), REV2.as_bytes()).unwrap();
    assert!(git_in(&repo, &["add", "-A"]).0);
    assert!(git_in(&repo, &["commit", "-q", "-m", "v2"]).0);
    let rev2 = git_in(&repo, &["rev-parse", "HEAD"]).1;
    assert_ne!(rev1, rev2, "the fixture must produce a new rev");

    let main2 = s.join("main2");
    prepare_main(&main2, &repo);
    let lock2 = generate_lock(&main2, &cg_home);
    assert!(
        String::from_utf8_lossy(&lock2).contains(&rev2) && lock2 != lock1,
        "the second lock must pin the new rev"
    );
    serve_and_build(&s, &main2, &lock2, &cargo, "dep-ok-v2");

    let keys = cargo_keys(&s);
    assert_eq!(
        keys.len(),
        2,
        "every lockfile byte set earns its own entry (§11): {keys:?}"
    );
    for k in &keys {
        assert!(k.join("nawa-complete").is_file(), "every entry carries the marker");
    }
}

const REV2: &str = "pub fn greeting() -> &'static str { \"dep-ok-v2\" }\n";

/// The same fixture built twice with forwarders that print DIFFERENT
/// `--version` strings: byte-identical lockfiles, different keys (§11 —
/// the Cargo registry directory names embed a hash that changes between
/// Cargo releases).
#[test]
fn the_cargo_version_is_part_of_the_key() {
    let s = scratch("version");
    let cargo = cargo_dir();
    let cg_home = s.join("cg-setup");
    fs::create_dir_all(&cg_home).unwrap();

    let repo = s.join("dep");
    git_dep_repo(&repo, "v1");
    let main = s.join("main");
    prepare_main(&main, &repo);
    let lock = generate_lock(&main, &cg_home);

    let files = vec![
        ("Cargo.toml".to_string(), fs::read(main.join("Cargo.toml")).unwrap()),
        ("Cargo.lock".to_string(), lock),
        ("src/main.rs".to_string(), fs::read(main.join("src/main.rs")).unwrap()),
    ];
    let tar = fixture_tar_gz("main-1.0", &files);
    let tar_sha = sha256(&tar);
    let (addr, _) = route_server(vec![("/src.tar.gz", Some(tar))]);
    let install = format!(
        "export PATH={cargo}:$PATH && cargo build --release --locked --offline \
         && mkdir -p %{{pkgdir}}/usr/bin \
         && cp target/release/testmain %{{pkgdir}}/usr/bin/ \
         && %{{pkgdir}}/usr/bin/testmain"
    );
    write_recipe(&s.join("recipes/cargo-b"), &recipe(&tar_sha, &addr, &install));

    // A forwarder dir in the rustup-shim model: `cargo` fakes the
    // `--version` line and forwards everything else; `rustc` forwards too,
    // because `cargo fetch` probes `rustc -vV` for the host/platform info
    // before it talks to any source.
    let fake = |s: &Path, tag: &str| {
        let dir = s.join(format!("fakecargo-{tag}"));
        fs::create_dir_all(&dir).unwrap();
        let script = dir.join("cargo");
        let mut f = fs::File::create(&script).unwrap();
        writeln!(
            f,
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then\n  echo \"cargo 9.9.9-nawa-test-{tag}\"\n  exit 0\nfi\nexec \"{}/cargo\" \"$@\"",
            cargo_dir()
        )
        .unwrap();
        f.flush().unwrap();
        drop(f);
        let rustc_fwd = dir.join("rustc");
        fs::write(
            &rustc_fwd,
            format!(
                "#!/bin/sh\nexec \"{}/rustc\" \"$@\"\n",
                cargo_dir()
            ),
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
            fs::set_permissions(&rustc_fwd, fs::Permissions::from_mode(0o755)).unwrap();
        }
        dir
    };

    let run = |s: &Path, tag: &str| {
        let (out, err, ok) = run_fetch_build(
            s,
            &[("NAWA_FETCH_PATH", fake(s, tag).to_str().unwrap())],
            &[],
        );
        assert!(ok, "run {tag} failed\nstdout:\n{out}\nstderr:\n{err}");
        out
    };
    run(&s, "A");
    assert_eq!(cargo_keys(&s).len(), 1, "the first version earns entry one");
    run(&s, "B");
    let keys = cargo_keys(&s);
    assert_eq!(
        keys.len(),
        2,
        "a different cargo --version must earn its own entry (§11): {keys:?}"
    );
}

// ---------------------------------------------------------------------------
// §8 source-tree config rules
// ---------------------------------------------------------------------------

#[test]
fn the_source_tree_config_rules() {
    // (payload inside .cargo/config.toml, expected needle)
    let cases: Vec<(&str, &str)> = vec![
        (
            "[source.crates-io]\nregistry = \"sparse+https://example.invalid/index/\"\n",
            "[source.crates-io]",
        ),
        ("[registries]\nindex = \"https://example.invalid/\"\n", "[registries]"),
        ("[net]\nretry = 3\n", "[net]"),
        ("[http]\nproxy = \"http://example.invalid\"\n", "[http]"),
        (
            "[source.legit]\nregistry = \"sparse+https://example.invalid/index/\"\n",
            "`legit`",
        ),
    ];
    let base_cargo_toml = b"[package]\nname = \"testmain\"\nversion = \"0.1.0\"\n";
    for (idx, (cfg, needle)) in cases.into_iter().enumerate() {
        let tag = format!("cfg{idx}");
        let s = scratch(&tag);
        let main = s.join("main");
        fs::create_dir_all(main.join("src")).unwrap();
        fs::create_dir_all(main.join(".cargo")).unwrap();
        fs::write(main.join("Cargo.toml"), base_cargo_toml).unwrap();
        fs::write(main.join(".cargo/config.toml"), cfg.as_bytes()).unwrap();
        fs::write(main.join("src/main.rs"), b"fn main() {}\n").unwrap();
        fs::write(main.join("Cargo.lock"), b"# no deps\n").unwrap();
        let files = vec![
            ("Cargo.toml".to_string(), fs::read(main.join("Cargo.toml")).unwrap()),
            ("Cargo.lock".to_string(), fs::read(main.join("Cargo.lock")).unwrap()),
            ("src/main.rs".to_string(), fs::read(main.join("src/main.rs")).unwrap()),
            (".cargo/config.toml".to_string(), cfg.as_bytes().to_vec()),
        ];
        let tar = fixture_tar_gz("main-1.0", &files);
        let tar_sha = sha256(&tar);
        let (addr, _) = route_server(vec![("/src.tar.gz", Some(tar))]);
        write_recipe(&s.join("recipes/cargo-b"), &recipe(&tar_sha, &addr, "echo built"));
        let (_, err, ok) = run_fetch_build(&s, &[], &[]);
        assert!(!ok, "case {idx}: the offending config must fail the fetch phase");
        assert!(
            err.contains(needle) && err.contains("§8"),
            "case {idx}: the error must name the offender and §8:\n{err}"
        );
        assert!(
            !s.join("srccache/fetch/cargo").exists(),
            "case {idx}: a config refusal must not populate anything"
        );
    }
}

// ---------------------------------------------------------------------------
// §8 vendored: replace-with → in-tree directory source skips the fetch
// ---------------------------------------------------------------------------

#[test]
fn a_vendored_directory_source_skips_the_fetch() {
    let s = scratch("vendored");
    let main = s.join("main");
    fs::create_dir_all(main.join("src"))
        .and_then(|_| fs::create_dir_all(main.join(".cargo")))
        .and_then(|_| fs::create_dir_all(main.join("vendor/testdep/src")))
        .unwrap();
    fs::write(
        main.join("Cargo.toml"),
        b"[package]\nname = \"testmain\"\nversion = \"0.1.0\"\n\n[dependencies]\ntestdep = \"0.1.0\"\n",
    )
    .unwrap();
    fs::write(main.join("src/main.rs"), MAIN_RS).unwrap();
    fs::write(
        main.join(".cargo/config.toml"),
        b"[source.crates-io]\nreplace-with = \"vendored-sources\"\n\n\
          [source.vendored-sources]\ndirectory = \"vendor\"\n",
    )
    .unwrap();
    fs::write(main.join("vendor/testdep/Cargo.toml"), DEP_CARGO_TOML).unwrap();
    fs::write(
        main.join("vendor/testdep/src/lib.rs"),
        b"pub fn greeting() -> &'static str { \"dep-ok-vendored\" }\n",
    )
    .unwrap();
    fs::write(main.join("vendor/testdep/.cargo-checksum.json"), CHECKSUM_JSON).unwrap();
    let cargo = cargo_dir();
    let cg_home = s.join("cg-setup");
    fs::create_dir_all(&cg_home).unwrap();
    let lock = generate_lock(&main, &cg_home);
    assert!(
        !String::from_utf8_lossy(&lock).contains("checksum"),
        "a directory-source lock has no registry checksums:\n{}",
        String::from_utf8_lossy(&lock)
    );

    let files = vec![
        ("Cargo.toml".to_string(), fs::read(main.join("Cargo.toml")).unwrap()),
        ("Cargo.lock".to_string(), lock),
        ("src/main.rs".to_string(), MAIN_RS.to_vec()),
        (".cargo/config.toml".to_string(), fs::read(main.join(".cargo/config.toml")).unwrap()),
        ("vendor/testdep/Cargo.toml".to_string(), DEP_CARGO_TOML.to_vec()),
        (
            "vendor/testdep/src/lib.rs".to_string(),
            b"pub fn greeting() -> &'static str { \"dep-ok-vendored\" }\n".to_vec(),
        ),
        ("vendor/testdep/.cargo-checksum.json".to_string(), CHECKSUM_JSON.to_vec()),
    ];
    let tar = fixture_tar_gz("main-1.0", &files);
    let tar_sha = sha256(&tar);
    let (addr, count) = route_server(vec![("/src.tar.gz", Some(tar))]);
    let install = format!(
        "export PATH={cargo}:$PATH && cargo build --release --offline \
         && mkdir -p %{{pkgdir}}/usr/bin \
         && cp target/release/testmain %{{pkgdir}}/usr/bin/ \
         && %{{pkgdir}}/usr/bin/testmain && echo vendored-ok"
    );
    write_recipe(&s.join("recipes/cargo-b"), &recipe(&tar_sha, &addr, &install));

    let (out, err, ok) = run_fetch_build(&s, &[], &[]);
    assert!(ok, "the vendored build must succeed with no proxy:\n{out}\n{err}");
    assert!(out.contains("dep-ok-vendored"), "the build resolved the vendor tree:\n{out}\n{err}");
    let work = s.join("build/cargo-b-1.0-1");
    assert!(
        !work.join("fetch-cargo.log").exists(),
        "vendored trees must not run the populate"
    );
    assert!(
        !s.join("srccache/fetch/cargo").exists(),
        "a vendored tree populates no ecosystem entry"
    );
    let view = work.join("fetch/cargo");
    assert!(view.is_dir(), "the view dir must exist for the build env");
    // the build may write its own cache scaffolding (§8 mutation), but the
    // vendored tree fetched nothing: no cloned git db, no registry content
    assert!(
        !view.join("git/db").exists()
            && !view.join("registry/src").exists()
            && !view.join("registry/cache").exists()
            && !view.join("registry/index").exists(),
        "the vendored view must stay free of fetched content: {:?}",
        fs::read_dir(&view)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().into_string().unwrap())
            .collect::<Vec<_>>()
    );
    assert_eq!(
        count.load(Ordering::SeqCst),
        1,
        "only the source tarball was fetched"
    );
}

const MAIN_RS: &[u8] = b"fn main() { println!(\"{}\", testdep::greeting()) }\n";
const DEP_CARGO_TOML: &[u8] = b"[package]\nname = \"testdep\"\nversion = \"0.1.0\"\n\n[dependencies]\n";
const CHECKSUM_JSON: &[u8] = b"{\"files\":{},\"package\":null}\n";

// ---------------------------------------------------------------------------
// §8 preconditions: Cargo.lock mandatory; a broken replacement is an error
// ---------------------------------------------------------------------------

#[test]
fn a_missing_cargo_lock_is_a_hard_error() {
    let s = scratch("nolock");
    let main = s.join("main");
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(
        main.join("Cargo.toml"),
        b"[package]\nname = \"testmain\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    fs::write(main.join("src/main.rs"), b"fn main() {}\n").unwrap();
    let files = vec![
        ("Cargo.toml".to_string(), fs::read(main.join("Cargo.toml")).unwrap()),
        ("src/main.rs".to_string(), fs::read(main.join("src/main.rs")).unwrap()),
    ];
    let tar = fixture_tar_gz("main-1.0", &files);
    let tar_sha = sha256(&tar);
    let (addr, _) = route_server(vec![("/src.tar.gz", Some(tar))]);
    write_recipe(&s.join("recipes/cargo-b"), &recipe(&tar_sha, &addr, "echo built"));
    let (_, err, ok) = run_fetch_build(&s, &[], &[]);
    assert!(!ok, "a missing Cargo.lock must fail the fetch phase");
    assert!(
        err.contains("Cargo.lock") && err.contains("missing") && err.contains("§8"),
        "the error must name the missing lockfile:\n{err}"
    );
}

#[test]
fn a_replacement_to_a_missing_directory_is_not_vendored_silently() {
    let s = scratch("missingdir");
    let main = s.join("main");
    fs::create_dir_all(main.join("src")).unwrap();
    fs::create_dir_all(main.join(".cargo")).unwrap();
    fs::write(
        main.join("Cargo.toml"),
        b"[package]\nname = \"testmain\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    fs::write(main.join("src/main.rs"), b"fn main() {}\n").unwrap();
    fs::write(
        main.join(".cargo/config.toml"),
        b"[source.crates-io]\nreplace-with = \"vendored-sources\"\n\n\
          [source.vendored-sources]\ndirectory = \"vendor\"\n",
    )
    .unwrap();
    fs::write(main.join("Cargo.lock"), b"# no deps\n").unwrap();
    let files = vec![
        ("Cargo.toml".to_string(), fs::read(main.join("Cargo.toml")).unwrap()),
        ("Cargo.lock".to_string(), fs::read(main.join("Cargo.lock")).unwrap()),
        ("src/main.rs".to_string(), fs::read(main.join("src/main.rs")).unwrap()),
        (
            ".cargo/config.toml".to_string(),
            fs::read(main.join(".cargo/config.toml")).unwrap(),
        ),
    ];
    let tar = fixture_tar_gz("main-1.0", &files);
    let tar_sha = sha256(&tar);
    let (addr, _) = route_server(vec![("/src.tar.gz", Some(tar))]);
    write_recipe(&s.join("recipes/cargo-b"), &recipe(&tar_sha, &addr, "echo built"));
    let (_, err, ok) = run_fetch_build(&s, &[], &[]);
    assert!(!ok, "a replacement to a missing directory must fail the fetch phase");
    assert!(
        err.contains("must be an existing directory") && err.contains("§8"),
        "the error must explain the vendored rule:\n{err}"
    );
}

// ---------------------------------------------------------------------------
// §12: an offline miss names the entry; nothing is touched half-way
// ---------------------------------------------------------------------------

#[test]
fn an_offline_cargo_miss_names_the_expected_entry() {
    let s = scratch("miss");
    let repo = s.join("dep");
    let cargo = cargo_dir();
    git_dep_repo(&repo, "v1");
    let cg_home = s.join("cg-setup");
    fs::create_dir_all(&cg_home).unwrap();
    let main = s.join("main");
    prepare_main(&main, &repo);
    let lock = generate_lock(&main, &cg_home);

    // populate everything once (source cache + ecosystem entry)
    serve_and_build(&s, &main, &lock, &cargo, "dep-ok-v1");

    // wipe only the ecosystem entries: the source cache stays usable
    force_rm(&s.join("srccache/fetch"));

    let (o2, e2, ok2) = run_fetch_build(&s, &[("NAWA_OFFLINE", "1")], &[]);
    assert!(!ok2, "the offline run must fail, got:\n{o2}\n{e2}");
    assert!(
        e2.contains("offline") && e2.contains("/fetch/cargo/"),
        "the error must name the missing fetch/cargo entry:\n{e2}"
    );
    assert!(
        !s.join("srccache/fetch").exists(),
        "an offline miss must not populate a partial entry"
    );

    // the online run recovers
    let (o3, e3, ok3) = run_fetch_build(&s, &[], &[]);
    assert!(ok3, "recovery run failed\n{o3}\n{e3}");
    assert!(o3.contains("dep-ok-v1"), "the recovered build resolves the dep:\n{o3}\n{e3}");
}


#[test]
fn inherited_cargo_home_config_is_ignored_by_fetch()
{
    let s = scratch("ambient-config");
    let repo = s.join("dep");
    let cargo = cargo_dir();
    git_dep_repo(&repo, "v1");
    let cg_home = s.join("cg-setup");
    fs::create_dir_all(&cg_home).unwrap();
    let main = s.join("main");
    prepare_main(&main, &repo);
    // Generate the lock before installing hostile config so fixture setup is
    // independent of the configuration being tested.
    let lock = generate_lock(&main, &cg_home);

    // An inherited CARGO_HOME with another invalid config must also be
    // ignored: Nawa supplies a fresh per-fetch CARGO_HOME explicitly.
    let hostile_home = s.join("hostile-cargo-home");
    fs::create_dir_all(hostile_home.join(".cargo")).unwrap();
    fs::write(
        hostile_home.join(".cargo/config.toml"),
        "[net]\nretry = \"still-not-an-integer\"\n",
    )
    .unwrap();

    let files = vec![
        ("Cargo.toml".to_string(), fs::read(main.join("Cargo.toml")).unwrap()),
        ("Cargo.lock".to_string(), lock),
        ("src/main.rs".to_string(), fs::read(main.join("src/main.rs")).unwrap()),
    ];
    let tar = fixture_tar_gz("main-1.0", &files);
    let tar_sha = sha256(&tar);
    let (addr, _) = route_server(vec![("/src.tar.gz", Some(tar))]);
    let install = format!(
        "export PATH={cargo}:$PATH && cargo build --release --locked --offline \\
         && mkdir -p %{{pkgdir}}/usr/bin \\
         && cp target/release/testmain %{{pkgdir}}/usr/bin/ \\
         && %{{pkgdir}}/usr/bin/testmain"
    );
    write_recipe(&s.join("recipes/cargo-b"), &recipe(&tar_sha, &addr, &install));
    let (out, err, ok) = run_fetch_build(
        &s,
        &[("CARGO_HOME", hostile_home.to_str().unwrap())],
        &[],
    );
    assert!(
        ok,
        "ambient Cargo configuration must not affect fetch resolution:\n{out}\n{err}"
    );
    assert!(out.contains("dep-ok-v1"), "the isolated fetch built the dependency:\n{out}\n{err}");
}


#[test]
fn ambient_parent_cargo_config_is_rejected_before_fetch() {
    let s = scratch("ambient-parent-config");
    let repo = s.join("dep");
    let cargo = cargo_dir();
    git_dep_repo(&repo, "v1");
    let cg_home = s.join("cg-setup");
    fs::create_dir_all(&cg_home).unwrap();
    let main = s.join("main");
    prepare_main(&main, &repo);
    let lock = generate_lock(&main, &cg_home);

    // Cargo would discover this file from the recipe's build cwd even though
    // it is outside the downloaded source tree. Nawa must reject it before
    // starting the fetch or the recipe's cargo build command.
    fs::create_dir_all(s.join(".cargo")).unwrap();
    fs::write(
        s.join(".cargo/config.toml"),
        "[net]\nretry = \"not-an-integer\"\n",
    )
    .unwrap();

    let files = vec![
        ("Cargo.toml".to_string(), fs::read(main.join("Cargo.toml")).unwrap()),
        ("Cargo.lock".to_string(), lock),
        ("src/main.rs".to_string(), fs::read(main.join("src/main.rs")).unwrap()),
    ];
    let tar = fixture_tar_gz("main-1.0", &files);
    let tar_sha = sha256(&tar);
    let (addr, _) = route_server(vec![("/src.tar.gz", Some(tar))]);
    let install = format!(
        "export PATH={cargo}:$PATH && cargo build --release --locked --offline \\
         && mkdir -p %{{pkgdir}}/usr/bin \\
         && cp target/release/testmain %{{pkgdir}}/usr/bin/ \\
         && %{{pkgdir}}/usr/bin/testmain"
    );
    write_recipe(&s.join("recipes/cargo-b"), &recipe(&tar_sha, &addr, &install));
    let (out, err, ok) = run_fetch_build(&s, &[], &[]);
    assert!(!ok, "ambient Cargo config must be rejected:\n{out}\n{err}");
    assert!(
        format!("{out}\n{err}").contains("ambient Cargo config"),
        "expected the hermeticity diagnostic:\n{out}\n{err}"
    );
}


// ---------------------------------------------------------------------------
// Real package smoke test: build Nawa from its own source recipe
// ---------------------------------------------------------------------------

/// Package the actual Nawa source tree, including the checked-in Cargo
/// vendor directory. This keeps the test independent of GitHub and crates.io
/// while exercising the same source archive + Cargo fetch path as a distro
/// recipe.
fn nawa_source_tar_gz() -> Vec<u8> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut tar_bytes = Vec::new();
    {
        let mut builder = tar::Builder::new(&mut tar_bytes);
        for file in ["Cargo.toml", "Cargo.lock"] {
            builder
                .append_path_with_name(
                    root.join(file),
                    PathBuf::from("nawa-1.0.0").join(file),
                )
                .unwrap();
        }
        for directory in [".cargo", "src", "vendor"] {
            for entry in walkdir::WalkDir::new(root.join(directory)).sort_by_file_name() {
                let entry = entry.unwrap();
                let relative = entry.path().strip_prefix(root).unwrap();
                let archive_path = PathBuf::from("nawa-1.0.0").join(relative);
                if entry.file_type().is_dir() {
                    builder.append_dir(archive_path, entry.path()).unwrap();
                } else if entry.file_type().is_file() {
                    builder
                        .append_path_with_name(entry.path(), archive_path)
                        .unwrap();
                } else {
                    panic!("unexpected source-tree entry: {}", entry.path().display());
                }
            }
        }
        builder.finish().unwrap();
    }
    let mut compressed = Vec::new();
    let mut encoder =
        flate2::write::GzEncoder::new(&mut compressed, flate2::Compression::default());
    encoder.write_all(&tar_bytes).unwrap();
    encoder.finish().unwrap();
    compressed
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// Expensive end-to-end test, run explicitly by the CI smoke job. It builds
/// the real Nawa source using the real [fetch.cargo] implementation, then
/// installs and executes the resulting package archive.
#[test]
#[ignore = "expensive real-package build; run from the CI smoke job"]
fn builds_nawa_from_its_real_recipe() {
    let s = scratch("real-nawa");
    let source = nawa_source_tar_gz();
    let source_sha = sha256(&source);
    let (addr, _requests) = route_server(vec![("/nawa.tar.gz", Some(source))]);

    let cargo = cargo_dir();
    let target_dir = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("target"));
    let target_dir = if target_dir.is_absolute() {
        target_dir
    } else {
        std::env::current_dir().unwrap().join(target_dir)
    };
    let cargo_q = shell_quote(&cargo);
    let target_q = shell_quote(&target_dir.to_string_lossy());

    let recipe_dir = s.join("recipes/nawa");
    fs::create_dir_all(&recipe_dir).unwrap();
    fs::write(
        recipe_dir.join("recipe.toml"),
        format!(
            r#"
[package]
name = "nawa"
version = "1.0.0"
rel = 1
arch = ["x86_64"]
summary = "Nawa source-based package manager"
source = "local:/nawa-schema-2-requires-a-newer-nawa"
workdir = "nawa"

[[package.sources]]
name = "nawa-source"
url = "{addr}/nawa.tar.gz"
sha256 = "{source_sha}"
strip_components = 1
into = "nawa"

[fetch.cargo]
dir = "nawa"

[build]
compile = """
export PATH={cargo_q}:"$PATH"
export CARGO_TARGET_DIR={target_q}
cargo build --manifest-path Cargo.toml --release --locked --offline
"""
install = """
install -Dm755 {target_q}/release/nawa %{{pkgdir}}/usr/bin/nawa
"""
"#
        ),
    )
    .unwrap();

    let build = nawa()
        .args([
            "--recipes",
            s.join("recipes").to_str().unwrap(),
            "--builddir",
            s.join("build").to_str().unwrap(),
            "build",
            "--isolation",
            "env",
            "nawa",
        ])
        .env("NAWA_SRCCACHE", s.join("srccache"))
        .env("NAWA_FETCH_PATH", &cargo)
        .output()
        .unwrap();
    assert!(
        build.status.success(),
        "real Nawa package build failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&build.stdout),
        String::from_utf8_lossy(&build.stderr)
    );

    let archive = s.join("build/out/nawa-1.0.0-1.nawa.tar.zst");
    assert!(archive.is_file(), "Nawa package archive was not produced");

    let root = s.join("rootfs");
    fs::create_dir_all(&root).unwrap();
    let install = nawa()
        .args([
            "--root",
            root.to_str().unwrap(),
            "install",
            archive.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        install.status.success(),
        "installing the built Nawa package failed:\n{}",
        String::from_utf8_lossy(&install.stderr)
    );

    let installed = root.join("usr/bin/nawa");
    assert!(installed.is_file(), "package did not install /usr/bin/nawa");
    let version = Command::new(&installed).arg("--version").output().unwrap();
    assert!(
        version.status.success(),
        "installed Nawa binary did not execute:\n{}",
        String::from_utf8_lossy(&version.stderr)
    );
    assert!(
        String::from_utf8_lossy(&version.stdout).contains("1.0.0"),
        "installed binary reported an unexpected version: {}",
        String::from_utf8_lossy(&version.stdout)
    );

    let _ = fs::remove_dir_all(&s);
}


// ---------------------------------------------------------------------------
// Fetch integration with strict sandbox isolation
// ---------------------------------------------------------------------------

#[test]
fn cargo_fetch_and_offline_build_work_under_sandbox() {
    let isolation = "sandbox";
    let s = scratch_outside_tmp(&format!("isolation-{isolation}"));
    let repo = s.join("dep");
    let rev = git_dep_repo(&repo, isolation);
    let cg_home = s.join("cg-setup");
    fs::create_dir_all(&cg_home).unwrap();
    let main = s.join("main");
    prepare_main(&main, &repo);
    let lock = generate_lock(&main, &cg_home);
    assert!(
        String::from_utf8_lossy(&lock).contains(&rev),
        "fixture lock must pin the local git dependency"
    );

    let files = vec![
        ("Cargo.toml".to_string(), fs::read(main.join("Cargo.toml")).unwrap()),
        ("Cargo.lock".to_string(), lock),
        ("src/main.rs".to_string(), fs::read(main.join("src/main.rs")).unwrap()),
    ];
    let tar = fixture_tar_gz("main-1.0", &files);
    let tar_sha = sha256(&tar);
    let (addr, _) = route_server(vec![("/src.tar.gz", Some(tar))]);
    let cargo = cargo_dir();
    let install = format!(
        "export PATH={cargo}:$PATH && cargo build --release --locked --offline \
         && ./target/release/testmain"
    );
    write_recipe(&s.join("recipes/cargo-b"), &recipe(&tar_sha, &addr, &install));

    let (out, err, ok) =
        run_fetch_build_with_isolation(&s, isolation, &[], &[]);
    assert!(
        ok,
        "Cargo fetch/build failed at isolation={isolation}\nstdout:\n{out}\nstderr:\n{err}"
    );
    assert!(
        out.contains(&format!("dep-ok-{isolation}")),
        "offline build did not resolve the pinned git dependency at {isolation}:\n{out}\n{err}"
    );
    assert!(
        s.join("build/cargo-b-1.0-1/fetch-cargo.log").is_file(),
        "Cargo fetch phase did not run at isolation={isolation}"
    );
    let _ = fs::remove_dir_all(&s);
}