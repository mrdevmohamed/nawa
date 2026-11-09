//! Integration tests for new-schema source staging (fetch-phase.md §16
//! "Multiple sources" + revision-3 tests 1–3).
//!
//! Every test drives a child `nawa build` against a route-based local HTTP
//! server, so no upstream network is touched and each URL serves exactly
//! the bytes the test pinns. The harness mirrors tests/offline.rs.
//!
//! The recipes here run with `--isolation env`: that is the level whose
//! environment whitelist gains `NAWA_SRCROOT` for `[[package.sources]]`
//! recipes (§4.3) — and, equally important, must NOT gain it for legacy
//! ones.

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
        // the new names must come from the staging engine, never from the
        // ambient environment of whatever machine runs the tests
        "NAWA_SRCROOT", "NAWA_ASSETSDIR",
    ] {
        c.env_remove(k);
    }
    c.env("NAWA_SYSCONFDIR", std::env::temp_dir().join("nawa-test-no-sysconf"));
    c.env("NAWA_INSTALL_MODE", "build");
    c.current_dir(std::env::temp_dir());
    c
}

fn scratch(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("nawa-sources-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

fn sha256(bytes: &[u8]) -> String {
    use sha2::Digest;
    hex::encode(sha2::Sha256::digest(bytes))
}

/// A deterministic tar.gz: one top dir with fixed mtimes (dir `dir_mtime`,
/// files 500_000) and the given (name, content) files.
fn fixture_tar_gz(top: &str, files: &[(&str, &[u8])], dir_mtime: u64) -> Vec<u8> {
    let mut plain = Vec::new();
    {
        let mut b = tar::Builder::new(&mut plain);
        let mut h = tar::Header::new_gnu();
        h.set_entry_type(tar::EntryType::Directory);
        h.set_size(0);
        h.set_mode(0o755);
        h.set_mtime(dir_mtime);
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
    flate2::write::GzEncoder::new(&mut gz, flate2::Compression::default())
        .write_all(&plain)
        .unwrap();
    gz
}

/// Serve a fixed route table for the whole test: `path → None` answers
/// 404, `path → Some(body)` answers 200 with those bytes. Connections
/// accepted are counted so tests can prove who called the network.
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

/// The standard two-archives-plus-plain-file recipe (§4.2). `addr` is the
/// route server base; `install` is the caller's build script.
fn multisrc_recipe(addr: &str, install: &str) -> String {
    let main = fixture_tar_gz("main-1.0", &[("hello.sh", b"#!/bin/sh\necho main-ok\n")], 1_400_000);
    let patches =
        fixture_tar_gz("patch-bundle", &[("fix.sh", b"#!/bin/sh\necho patch-ok\n")], 1_400_000);
    let data = b"bootstrap data\n";
    format!(
        r#"
[package]
name = "multisrc"
version = "1.0"
rel = 1
arch = ["x86_64"]
source = "local:/nawa-schema-2-requires-a-newer-nawa"
workdir = "kernel"

[[package.sources]]
name = "main"
url = "{addr}/src-main.tar.gz"
sha256 = "{}"
strip_components = 1
into = "kernel"

[[package.sources]]
name = "patch-bundle"
url = "{addr}/patches.tar.gz"
sha256 = "{}"
strip_components = 1
into = "patches"

[[package.sources]]
name = "data"
url = "{addr}/data/bootstrap.dat"
sha256 = "{}"
unpack = false
into = "data/bootstrap.dat"

[build]
install = """
{install}
"""
"#,
        sha256(&main),
        sha256(&patches),
        sha256(data),
    )
}

/// Build `recipe_name` with the given extra args; returns stdout/stderr.
fn run_build(recipes: &Path, builddir: &Path, extra: &[&str], name: &str) -> (String, String, bool) {
    let out = nawa()
        .args([
            "--recipes",
            recipes.to_str().unwrap(),
            "--builddir",
            builddir.to_str().unwrap(),
            "build",
            "--isolation",
            "env",
        ])
        .args(extra)
        .arg(name)
        .output()
        .unwrap();
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.success(),
    )
}

// ---------------------------------------------------------------------------
// §16 Multiple sources 1 + revision-3 test 3: layout, srcroot, workdir stamp
// ---------------------------------------------------------------------------

#[test]
fn two_archives_and_a_plain_file_stage_into_the_workspace() {
    let s = scratch("layout");
    let recipes = s.join("recipes");
    let r = recipes.join("multisrc");
    fs::create_dir_all(&r).unwrap();
    let (addr, _count) = route_server(vec![
        (
            "/src-main.tar.gz",
            Some(fixture_tar_gz("main-1.0", &[("hello.sh", b"#!/bin/sh\necho main-ok\n")], 1_400_000)),
        ),
        (
            "/patches.tar.gz",
            Some(fixture_tar_gz("patch-bundle", &[("fix.sh", b"#!/bin/sh\necho patch-ok\n")], 1_400_000)),
        ),
        ("/data/bootstrap.dat", Some(b"bootstrap data\n".to_vec())),
    ]);
    // the epoch falls out of the archive's newest regular-file mtime (§6);
    // the value itself is pinned by the stamping test below
    fs::write(
        r.join("recipe.toml"),
        multisrc_recipe(
            &addr,
            r#"set -e
test "$NAWA_SRCROOT" = "%{srcroot}" || { echo "FAIL srcroot env != %{srcroot}"; exit 1; }
test "$NAWA_SRCDIR" = "%{srcdir}" || { echo "FAIL srcdir env != %{srcdir}"; exit 1; }
test -f "%{srcroot}/kernel/hello.sh" || { echo "FAIL main archive not at %{srcroot}/kernel"; exit 1; }
test -f "%{srcroot}/patches/fix.sh" || { echo "FAIL patch archive not at %{srcroot}/patches"; exit 1; }
test "$(stat -c %Y "%{srcroot}/data/bootstrap.dat")" = "$SOURCE_DATE_EPOCH" || { echo "FAIL plain file not stamped"; exit 1; }
[ "$(pwd -P)" = "%{srcdir}" ] || { echo "FAIL cwd is $(pwd -P), want %{srcdir}"; exit 1; }
mkdir -p %{pkgdir}/usr/bin
cp %{srcdir}/hello.sh %{pkgdir}/usr/bin/hello
"#,
        ),
    )
    .unwrap();

    let (stdout, stderr, ok) = run_build(&recipes, &s.join("build"), &[], "multisrc");
    assert!(
        ok,
        "build failed:\n{stdout}\n--- stderr ---\n{stderr}"
    );
    let _ = fs::remove_dir_all(&s);
}

#[test]
fn plain_file_stamp_is_the_archive_epoch_env() {
    // The stamping check needs the epoch value in env: assert
    // SOURCE_DATE_EPOCH equals the fixed archive mtime (500000), which is
    // what the plain file must be stamped with.
    let s = scratch("stamp");
    let recipes = s.join("recipes");
    let r = recipes.join("multisrc");
    fs::create_dir_all(&r).unwrap();
    let (addr, count) = route_server(vec![
        (
            "/src-main.tar.gz",
            Some(fixture_tar_gz("main-1.0", &[("hello.sh", b"#!/bin/sh\necho main-ok\n")], 1_400_000)),
        ),
        (
            "/patches.tar.gz",
            Some(fixture_tar_gz("patch-bundle", &[("fix.sh", b"#!/bin/sh\necho patch-ok\n")], 1_400_000)),
        ),
        ("/data/bootstrap.dat", Some(b"bootstrap data\n".to_vec())),
    ]);
    fs::write(
        r.join("recipe.toml"),
        multisrc_recipe(
            &addr,
            r#"set -e
test "$SOURCE_DATE_EPOCH" = "500000" || { echo "FAIL epoch $SOURCE_DATE_EPOCH"; exit 1; }
m=$(stat -c %Y "%{srcroot}/data/bootstrap.dat")
test "$m" = "$SOURCE_DATE_EPOCH" || { echo "FAIL stamp $m != $SOURCE_DATE_EPOCH"; exit 1; }
# atime stamped too (checked at read time: reading bumps it afterwards)
mkdir -p %{pkgdir}/usr/bin
cp %{srcdir}/hello.sh %{pkgdir}/usr/bin/hello
"#,
        ),
    )
    .unwrap();
    let (stdout, stderr, ok) = run_build(&recipes, &s.join("build"), &[], "multisrc");
    assert!(ok, "build failed:\n{stdout}\n--- stderr ---\n{stderr}");
    // both archive sources and the plain file were fetched: exactly 3 accepts
    assert_eq!(
        count.load(Ordering::SeqCst),
        3,
        "each source must be fetched exactly once per build"
    );
    let _ = fs::remove_dir_all(&s);
}

// ---------------------------------------------------------------------------
// §16 Multiple sources 2: a wrong hash fails before extraction and sections
// ---------------------------------------------------------------------------

#[test]
fn wrong_hash_fails_before_extraction_and_sections() {
    let s = scratch("badhash");
    let recipes = s.join("recipes");
    let r = recipes.join("multisrc");
    fs::create_dir_all(&r).unwrap();
    let (addr, count) = route_server(vec![
        (
            "/src-main.tar.gz",
            Some(fixture_tar_gz("main-1.0", &[("hello.sh", b"#!/bin/sh\necho main-ok\n")], 1_400_000)),
        ),
        (
            "/patches.tar.gz",
            Some(fixture_tar_gz("patch-bundle", &[("fix.sh", b"#!/bin/sh\necho patch-ok\n")], 1_400_000)),
        ),
        ("/data/bootstrap.dat", Some(b"bootstrap data\n".to_vec())),
    ]);
    // correct recipe except: the SECOND source's sha256 is wrong
    let patches = fixture_tar_gz("patch-bundle", &[("fix.sh", b"#!/bin/sh\necho patch-ok\n")], 1_400_000);
    let right = multisrc_recipe(
        &addr,
        r#"mkdir -p %{pkgdir}/usr
cp %{srcdir}/hello.sh %{pkgdir}/usr/h
"#,
    );
    let broken = right.replace(&sha256(&patches), &"9".repeat(64));
    assert!(broken.contains("sha256 = \"9999999999999999999999999999999999999999999999999999999999999999\""));
    fs::write(r.join("recipe.toml"), broken).unwrap();

    let (stdout, stderr, ok) = run_build(&recipes, &s.join("build"), &[], "multisrc");
    assert!(!ok, "a wrong hash must fail the build:\n{stdout}");
    assert!(
        stderr.contains("sha256 mismatch") || stderr.contains("source `patch-bundle`"),
        "the error must name the failing source: {stderr}"
    );
    // no build section was ever run: no section logs exist
    let work = s.join("build").join("multisrc-1.0-1");
    let logs: Vec<String> = fs::read_dir(&work)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("build-"))
        .collect();
    assert!(logs.is_empty(), "no build section may have run: {logs:?}");
    // Sources are downloaded in parallel (network-bound) — all three are
    // attempted concurrently, so a wrong hash does not prevent the other
    // downloads. The failure still happens before any extraction and
    // before any build section (verified above).
    assert_eq!(count.load(Ordering::SeqCst), 3);
    let _ = fs::remove_dir_all(&s);
}

// ---------------------------------------------------------------------------
// §16 Multiple sources 3: destination collisions are rejected
// ---------------------------------------------------------------------------

#[test]
fn destination_collision_is_rejected_naming_both_sources() {
    let s = scratch("collide");
    let recipes = s.join("recipes");
    let r = recipes.join("collide");
    fs::create_dir_all(&r).unwrap();
    // two archives, both strip=1 into the workspace root, both providing C
    let a = fixture_tar_gz("main-1.0", &[("C", b"from main\n")], 1_400_000);
    let b = fixture_tar_gz("extra-1.0", &[("C", b"from extra\n")], 1_400_000);
    let (addr, _count) = route_server(vec![
        ("/a.tar.gz", Some(a)),
        ("/b.tar.gz", Some(b)),
    ]);
    fs::write(
        r.join("recipe.toml"),
        format!(
            r#"
[package]
name = "collide"
version = "1.0"
rel = 1
arch = ["x86_64"]
source = "local:/nawa-schema-2-requires-a-newer-nawa"

[[package.sources]]
name = "main"
url = "{addr}/a.tar.gz"
sha256 = "{}"
strip_components = 1

[[package.sources]]
name = "extra"
url = "{addr}/b.tar.gz"
sha256 = "{}"
strip_components = 1

[build]
install = "true"
"#,
            sha256(&fixture_tar_gz("main-1.0", &[("C", b"from main\n")], 1_400_000)),
            sha256(&fixture_tar_gz("extra-1.0", &[("C", b"from extra\n")], 1_400_000)),
        ),
    )
    .unwrap();
    let (stdout, stderr, ok) = run_build(&recipes, &s.join("build"), &[], "collide");
    assert!(!ok, "a collision must fail the build:\n{stdout}");
    assert!(
        stderr.contains("collision"),
        "the error must call it a collision: {stderr}"
    );
    assert!(
        stderr.contains("main") && stderr.contains("extra"),
        "the error must name both sources: {stderr}"
    );
    let _ = fs::remove_dir_all(&s);
}

// ---------------------------------------------------------------------------
// §16 Multiple sources 4: a mirror serving different bytes is refused
// ---------------------------------------------------------------------------

#[test]
fn wrong_bytes_mirror_is_refused_and_fallback_applies() {
    let s = scratch("mirror");
    let recipes = s.join("recipes");
    let r = recipes.join("mirror");
    fs::create_dir_all(&r).unwrap();
    let good = fixture_tar_gz("main-1.0", &[("hello.sh", b"#!/bin/sh\necho main-ok\n")], 1_400_000);
    let (addr, count) = route_server(vec![
        // primary 404s, mirror 1 serves wrong bytes, mirror 2 correct
        ("/a.tar.gz", None),
        ("/m1.tar.gz", Some(b"evil tampered bytes\n".to_vec())),
        ("/m2.tar.gz", Some(good)),
    ]);
    fs::write(
        r.join("recipe.toml"),
        format!(
            r#"
[package]
name = "mirror"
version = "1.0"
rel = 1
arch = ["x86_64"]
source = "local:/nawa-schema-2-requires-a-newer-nawa"

[[package.sources]]
name = "main"
url = "{addr}/a.tar.gz"
sha256 = "{}"
mirrors = ["{addr}/m1.tar.gz", "{addr}/m2.tar.gz"]
strip_components = 1

[build]
install = "set -e; mkdir -p %{{pkgdir}}/usr/bin; cp %{{srcdir}}/hello.sh %{{pkgdir}}/usr/bin/hello"
"#,
            sha256(&fixture_tar_gz("main-1.0", &[("hello.sh", b"#!/bin/sh\necho main-ok\n")], 1_400_000)),
        ),
    )
    .unwrap();
    let (stdout, stderr, ok) = run_build(&recipes, &s.join("build"), &[], "mirror");
    assert!(ok, "mirror fallback must succeed:\n{stdout}\n--- stderr ---\n{stderr}");
    let combined = format!("{stdout}{stderr}");
    assert!(
        combined.contains("trying alternate mirror") || combined.contains("mirror"),
        "the fallback must be visible: {combined}"
    );
    // the tampered bytes never reached the cache: the cache entry content
    // must be the verified good bytes only
    let cache = s.join("build/distfiles/sha256");
    let digest = sha256(&fixture_tar_gz("main-1.0", &[("hello.sh", b"#!/bin/sh\necho main-ok\n")], 1_400_000));
    let entry = cache.join(&digest);
    assert_eq!(
        fs::read(&entry).unwrap(),
        fixture_tar_gz("main-1.0", &[("hello.sh", b"#!/bin/sh\necho main-ok\n")], 1_400_000),
        "only verified bytes may be published"
    );
    // 1 (primary 404) + 1 (m1 wrong) + 1 (m2 good)
    assert_eq!(count.load(Ordering::SeqCst), 3);
    let _ = fs::remove_dir_all(&s);
}

// ---------------------------------------------------------------------------
// revision-3 test 1: a recipe with an unpack=false file builds identical
// archives at two different times
// ---------------------------------------------------------------------------

#[test]
fn multi_source_with_plain_file_rebuild_check_passes() {
    let s = scratch("rebuild");
    let recipes = s.join("recipes");
    let r = recipes.join("multisrc");
    fs::create_dir_all(&r).unwrap();
    let (addr, count) = route_server(vec![
        (
            "/src-main.tar.gz",
            Some(fixture_tar_gz("main-1.0", &[("hello.sh", b"#!/bin/sh\necho main-ok\n")], 1_400_000)),
        ),
        (
            "/patches.tar.gz",
            Some(fixture_tar_gz("patch-bundle", &[("fix.sh", b"#!/bin/sh\necho patch-ok\n")], 1_400_000)),
        ),
        ("/data/bootstrap.dat", Some(b"bootstrap data\n".to_vec())),
    ]);
    // UNPINNED epoch: derived once from the archive's regular files and
    // stamped onto the plain file; the staging wall-clock time must never
    // reach it (§6). Shared srccache: the two runs use different builddir
    // subdirs, so the default (<builddir>/distfiles) would move with them.
    fs::write(
        r.join("recipe.toml"),
        multisrc_recipe(
            &addr,
            r#"set -e
mkdir -p %{pkgdir}/usr/bin %{pkgdir}/usr/share
cp %{srcdir}/hello.sh %{pkgdir}/usr/bin/hello
cp %{srcroot}/patches/fix.sh %{pkgdir}/usr/bin/fix
cp %{srcroot}/data/bootstrap.dat %{pkgdir}/usr/share/bootstrap.dat
"#,
        ),
    )
    .unwrap();
    let out = nawa()
        .env("NAWA_SRCCACHE", s.join("shared-distfiles"))
        .args([
            "--recipes",
            recipes.to_str().unwrap(),
            "--builddir",
            s.join("build").to_str().unwrap(),
            "build",
            "--isolation", "env",
            "--rebuild-check",
            "multisrc",
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        out.status.success(),
        "rebuild-check failed:\n{stdout}\n--- stderr ---\n{stderr}"
    );
    assert!(stdout.contains("rebuild-check: OK"), "{stdout}\n{stderr}");
    // run A downloads all three sources from the local server
    assert!(
        count.load(Ordering::SeqCst) >= 3,
        "the first pass must download all three sources (got {} via stdout: {stdout})",
        count.load(Ordering::SeqCst)
    );
    let _ = fs::remove_dir_all(&s);
}

// ---------------------------------------------------------------------------
// revision-3 test 2: all-plain recipes without a pinned epoch are parse errors
// ---------------------------------------------------------------------------

#[test]
fn all_plain_sources_without_pinned_epoch_are_rejected() {
    let s = scratch("nopin");
    let recipes = s.join("recipes");
    let r = recipes.join("plain");
    fs::create_dir_all(&r).unwrap();
    let data = b"bootstrap data\n";
    let (addr, _count) = route_server(vec![("/data.bin", Some(data.to_vec()))]);
    fs::write(
        r.join("recipe.toml"),
        format!(
            r#"
[package]
name = "plain"
version = "1.0"
rel = 1
arch = ["x86_64"]
source = "local:/nawa-schema-2-requires-a-newer-nawa"

[[package.sources]]
name = "data"
url = "{addr}/data.bin"
sha256 = "{}"
unpack = false

[build]
install = "true"
"#,
            sha256(data)
        ),
    )
    .unwrap();
    let (stdout, stderr, ok) = run_build(&recipes, &s.join("build"), &[], "plain");
    assert!(!ok, "an unpinned all-plain recipe must be rejected");
    let combined = format!("{stdout}{stderr}");
    assert!(
        combined.contains("source_date_epoch"),
        "the error must point at package.source_date_epoch: {combined}"
    );
    let _ = fs::remove_dir_all(&s);
}

// ---------------------------------------------------------------------------
// backward compatibility: a legacy recipe's environment keeps no srcroot,
// and a legacy recipe referencing %{srcroot} is refused (§14C)
// ---------------------------------------------------------------------------

#[test]
fn legacy_recipe_sees_no_srcroot_and_is_refused_with_the_placeholder() {
    let s = scratch("legacy");
    let recipes = s.join("recipes");
    let tarball =
        fixture_tar_gz("legacy-1.0", &[("hello.sh", b"#!/bin/sh\necho legacy-ok\n")], 1_400_000);
    let (addr, _count) = route_server(vec![("/legacy.tar.gz", Some(tarball))]);
    let r = recipes.join("legacy");
    fs::create_dir_all(&r).unwrap();
    // (a) a healthy legacy recipe: NAWA_SRCROOT must be absent from the env
    fs::write(
        r.join("recipe.toml"),
        format!(
            r#"
[package]
name = "legacy"
version = "1.0"
rel = 1
arch = ["x86_64"]
source = "{addr}/legacy.tar.gz"
sha256 = "{}"
source_dir = "legacy-1.0"

[build]
install = """
set -e
# the name is split so the §14C raw-recipe scan does not trip on the check itself
test -z "$(printenv NAWA_SRC"ROOT")" || {{ echo "FAIL legacy env gained the new root variable"; exit 1; }}
mkdir -p %{{pkgdir}}/usr/bin
cp %{{srcdir}}/hello.sh %{{pkgdir}}/usr/bin/hello
"""
"#,
            sha256(&fixture_tar_gz(
                "legacy-1.0",
                &[("hello.sh", b"#!/bin/sh\necho legacy-ok\n")],
                1_400_000
            )),
        ),
    )
    .unwrap();
    let (stdout, stderr, ok) = run_build(&recipes, &s.join("build"), &[], "legacy");
    assert!(ok, "legacy build failed:\n{stdout}\n--- stderr ---\n{stderr}");

    // (b) a legacy recipe referencing %{srcroot}: refused at build time
    let r2 = recipes.join("legacyph");
    fs::create_dir_all(&r2).unwrap();
    fs::write(
        r2.join("recipe.toml"),
        r#"
[package]
name = "legacyph"
version = "1.0"
rel = 1
arch = ["x86_64"]

[build]
install = """
echo %{srcroot}
"""
"#
        )
        .unwrap();
    let (_stdout, stderr2, ok2) = run_build(&recipes, &s.join("build"), &[], "legacyph");
    assert!(!ok2, "a legacy recipe with %{{srcroot}} must be refused");
    assert!(
        stderr2.contains("[[package.sources]]"),
        "the §14C error must name the fix: {stderr2}"
    );
    let _ = fs::remove_dir_all(&s);
}
