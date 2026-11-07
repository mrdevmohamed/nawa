//! Integration tests for recipe-local assets (fetch-phase.md §5, §16
//! "Assets" acceptance items 1–6).
//!
//! Every asset test drives a child `nawa build` at the `env` isolation
//! level — the one whose environment whitelist gains `NAWA_ASSETSDIR` for
//! `[assets]` recipes (§5.3.3). Assets-only recipes carry no sources, so
//! most tests here never touch the network; the ones that do use the same
//! route-based local HTTP server as tests/sources.rs and count accepted
//! connections to prove who called the network.

use std::fs;
use std::io::Write as _;
use std::os::unix::fs::PermissionsExt;
use std::net::TcpListener;
use std::path::Path;
use std::path::PathBuf;
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
        "NAWA_OFFLINE", "NAWA_SRCCACHE", "NAWA_SOURCE_DATE_EPOCH",
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
    let d = std::env::temp_dir().join(format!("nawa-assets-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

fn sha256(bytes: &[u8]) -> String {
    use sha2::Digest;
    hex::encode(sha2::Sha256::digest(bytes))
}

/// A deterministic tar.gz: one top dir, fixed mtimes (files 500_000, dir
/// 1_400_000) — the same shape tests/sources.rs uses, so the epoch here
/// stays pinned at 500000.
fn fixture_tar_gz(top: &str, files: &[(&str, &[u8])]) -> Vec<u8> {
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
    flate2::write::GzEncoder::new(&mut gz, flate2::Compression::default())
        .write_all(&plain)
        .unwrap();
    gz
}

/// Serve a fixed route table: `path → None` answers 404, `path → Some(body)`
/// answers 200 with those bytes; accepted connections are counted.
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

/// Build `name` with the given extra args; returns stdout/stderr/ok.
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

/// A kernel-style assets tree beside one recipe directory (§5.1): two
/// declared directories, one declared single *file* include, plus two
/// undeclared neighbours and one empty directory. Returns the recipe dir.
fn kernel_style_assets(recipes: &Path, name: &str) -> PathBuf {
    let r = recipes.join(name);
    fs::create_dir_all(r.join("files/patches")).unwrap();
    fs::create_dir_all(r.join("files/scripts")).unwrap();
    fs::create_dir_all(r.join("files/config")).unwrap();
    fs::create_dir_all(r.join("files/void")).unwrap();
    fs::create_dir_all(r.join("files/other")).unwrap();
    fs::write(r.join("files/patches/0001-fix.patch"), b"-- the fix\n").unwrap();
    fs::write(r.join("files/config/x86_64.fragment"), b"CONFIG_A=y\n").unwrap();
    fs::write(r.join("files/config/extra.fragment"), b"CONFIG_B=y\n").unwrap();
    fs::write(r.join("files/scripts/configure-kernel.sh"), b"#!/bin/sh\necho assets-ok\n")
        .unwrap();
    // the script carries the owner-exec bit: §5.3.2 round-trips exactly that
    fs::set_permissions(
        r.join("files/scripts/configure-kernel.sh"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    fs::write(r.join("files/other/notdeclared.txt"), b"must stay hidden\n").unwrap();
    r
}

/// Assets-only recipe body with a pinned epoch (§6: assets never feed it).
/// The recipe is deliberately referenced by name in the tests through
/// `.replace("(((name)))", ...)`.
fn assets_only_recipe(name: &str, install: &str) -> String {
    format!(
        r#"
[package]
name = "{name}"
version = "1.0"
rel = 1
arch = ["x86_64"]
source = "local:/nawa-schema-2-requires-a-newer-nawa"
source_date_epoch = 1700000000

[assets]
include = ["files/patches", "files/config/x86_64.fragment", "files/scripts/configure-kernel.sh", "files/void"]

[build]
install = '''
{install}
'''
"#
    )
}

// ---------------------------------------------------------------------------
// §16 Assets 1 + 2 (env level): only declared paths staged, read-only
// ---------------------------------------------------------------------------

#[test]
fn only_declared_assets_are_staged_and_read_only() {
    let s = scratch("staged");
    let recipes = s.join("recipes");
    let r = kernel_style_assets(&recipes, "staged");
    fs::write(
        r.join("recipe.toml"),
        assets_only_recipe(
            "staged",
            r##"set -e
# §5.3.3: the environment name and the placeholder name the same directory
test "$NAWA_ASSETSDIR" = "%{assetsdir}" || { echo "FAIL env != placeholder"; exit 1; }
# §5.3.1: declared trees and single files appear with their recipe-dir-relative layout
test -f "%{assetsdir}/files/patches/0001-fix.patch" || { echo "FAIL patch missing"; exit 1; }
test -f "%{assetsdir}/files/config/x86_64.fragment" || { echo "FAIL fragment missing"; exit 1; }
test -f "%{assetsdir}/files/scripts/configure-kernel.sh" || { echo "FAIL script missing"; exit 1; }
# §16 Assets 1: undeclared siblings are invisible
test ! -e "%{assetsdir}/files/config/extra.fragment" || { echo "FAIL undeclared fragment staged"; exit 1; }
test ! -e "%{assetsdir}/files/other" || { echo "FAIL undeclared dir staged"; exit 1; }
# §5.3.2: empty directories are not preserved
test ! -e "%{assetsdir}/files/void" || { echo "FAIL empty dir staged"; exit 1; }
# §5.3.4 (env level is chmod-only, best effort — but the test may pin it)
test ! -w "%{assetsdir}/files/config/x86_64.fragment" || { echo "FAIL asset file writable"; exit 1; }
test ! -w "%{assetsdir}/files/patches" || { echo "FAIL asset dir writable"; exit 1; }
# §5.3.2: the owner-execute bit is the one mode bit that survives
test -x "%{assetsdir}/files/scripts/configure-kernel.sh" || { echo "FAIL exec bit lost"; exit 1; }
# §14C: no sources — no workspace-root name; asserted via a concatenation
# the raw-recipe scan cannot see
test -z "$(printenv NAWA_SRC"ROOT")" || { echo "FAIL assets-only env gained srcroot"; exit 1; }
# §6: assets-only recipes run on the pinned epoch, nothing else
test "$SOURCE_DATE_EPOCH" = "1700000000" || { echo "FAIL epoch $SOURCE_DATE_EPOCH"; exit 1; }
mkdir -p "%{pkgdir}/usr/share/boot"
cp "%{assetsdir}/files/config/x86_64.fragment" "%{pkgdir}/usr/share/boot/"
"##,
        ),
    )
    .unwrap();

    let (stdout, stderr, ok) = run_build(&recipes, &s.join("build"), &[], "staged");
    assert!(ok, "assets-only build failed:\n{stdout}\n--- stderr ---\n{stderr}");
    let _ = fs::remove_dir_all(&s);
}

// ---------------------------------------------------------------------------
// §16 Assets 3: broken assets fail BEFORE any download
// ---------------------------------------------------------------------------

#[test]
fn missing_or_broken_assets_fail_before_any_download() {
    let s = scratch("baddl");
    let recipes = s.join("recipes");
    kernel_style_assets(&recipes, "badmissing");
    let fake = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    // the route table is empty: nothing may ever be fetched
    let (addr, count) = route_server(vec![]);

    // (a) a missing declared path — the recipe also fetches a source, so
    // the failure must appear before any byte is transferred
    let mut rec = assets_only_recipe(
        "badmissing",
        "cp \"%{assetsdir}/files/config/x86_64.fragment\" \"%{pkgdir}/\"",
    );
    rec = rec.replace(
        "source_date_epoch = 1700000000",
        &format!(
            "source_date_epoch = 1700000000\n\n[[package.sources]]\nname = \"main\"\nurl = \"{addr}/src.tar.gz\"\nsha256 = \"{fake}\""
        ),
    );
    fs::write(recipes.join("badmissing/recipe.toml"), rec).unwrap();
    // remove one declared file: the include table now references a path
    // that does not exist
    fs::remove_file(recipes.join("badmissing/files/scripts/configure-kernel.sh")).unwrap();
    let (stdout, stderr, ok) = run_build(&recipes, &s.join("build"), &[], "badmissing");
    assert!(!ok, "missing asset must fail the build");
    let combined = format!("{stdout}{stderr}");
    assert!(
        combined.contains("badmissing"),
        "the error must name the recipe: {combined}"
    );
    assert_eq!(
        count.load(Ordering::SeqCst),
        0,
        "a broken assets table must fail before any download (§16 Assets 3)"
    );

    // (b) a symlink INSIDE a declared tree — same pre-download refusal
    kernel_style_assets(&recipes, "badsymlink");
    std::os::unix::fs::symlink("0001-fix.patch", recipes.join("badsymlink/files/patches/evil.link"))
        .unwrap();
    fs::write(
        recipes.join("badsymlink/recipe.toml"),
        assets_only_recipe(
            "badsymlink",
            "cp \"%{assetsdir}/files/config/x86_64.fragment\" \"%{pkgdir}/\"",
        ),
    )
    .unwrap();
    let (stdout, stderr, ok) = run_build(&recipes, &s.join("build"), &[], "badsymlink");
    assert!(!ok, "a symlink in assets must fail the build");
    let combined = format!("{stdout}{stderr}");
    assert!(
        combined.contains("symlink"),
        "the error must name the rule (§5.2): {combined}"
    );
    assert_eq!(
        count.load(Ordering::SeqCst),
        0,
        "still no connection — precheck is download-free"
    );
    let _ = fs::remove_dir_all(&s);
}

// ---------------------------------------------------------------------------
// §16 Assets 4: changing an asset changes the build id
// ---------------------------------------------------------------------------

#[test]
fn changing_an_asset_invalidates_the_cached_build_id() {
    let s = scratch("buid");
    let recipes = s.join("recipes");
    let r = kernel_style_assets(&recipes, "buid");
    fs::write(
        r.join("recipe.toml"),
        assets_only_recipe(
            "buid",
            r##"set -e
mkdir -p "%{pkgdir}/usr/share/boot"
cp "%{assetsdir}/files/config/x86_64.fragment" "%{pkgdir}/usr/share/boot/"
"##,
        ),
    )
    .unwrap();
    let (stdout, stderr, ok) = run_build(&recipes, &s.join("build"), &[], "buid");
    assert!(ok, "first build failed:\n{stdout}\n--- stderr ---\n{stderr}");

    // digest converged: the recorded (snapshot) id and the probed
    // (checkout) id agree on unchanged inputs → --use-cache is a hit
    let (hit, hit_err, ok) = run_build(&recipes, &s.join("build"), &["--use-cache"], "buid");
    assert!(ok, "cache probe failed: {hit}{hit_err}");
    assert!(
        hit.contains("cache hit"),
        "unchanged assets must hit the cache: {hit}{hit_err}"
    );

    // mutate the checkout → the probe digest no longer matches the
    // recorded one (§16 Assets 4)
    fs::write(r.join("files/config/x86_64.fragment"), b"CONFIG_A=n\n").unwrap();
    let (rebuild, rebuild_err, ok) =
        run_build(&recipes, &s.join("build"), &["--use-cache"], "buid");
    assert!(ok, "rebuild failed: {rebuild}{rebuild_err}");
    assert!(
        format!("{rebuild}{rebuild_err}").contains("build_id mismatch"),
        "an asset change must rebuild: {rebuild}{rebuild_err}"
    );

    // no further change → the cache converges again (probe == recorded)
    let (hit2, hit2_err, ok2) = run_build(&recipes, &s.join("build"), &["--use-cache"], "buid");
    assert!(ok2, "second cache probe failed: {hit2}{hit2_err}");
    assert!(
        hit2.contains("cache hit"),
        "unchanged assets must hit the cache again: {hit2}{hit2_err}"
    );
    let _ = fs::remove_dir_all(&s);
}

// ---------------------------------------------------------------------------
// §16 Assets 5: a checkout change during the build cannot reach the snapshot
// ---------------------------------------------------------------------------

#[test]
fn snapshot_seals_the_checkout_mid_build() {
    let s = scratch("seal");
    let recipes = s.join("recipes");
    let r = kernel_style_assets(&recipes, "sealed");
    fs::write(
        r.join("recipe.toml"),
        assets_only_recipe(
            "sealed",
            r##"
set -e
# section time: the snapshot is already sealed; wait for the test's go
# signal, then copy what the snapshot holds (the §16 race window)
touch "%{pkgdir}/../.snap-ready"
for i in $(seq 1 200); do [ -f "GO_SIGNAL" ] && break; sleep 0.05; done
mkdir -p "%{pkgdir}/usr/share/boot"
cp "%{assetsdir}/files/config/x86_64.fragment" "%{pkgdir}/usr/share/boot/"
"##,
        )
        .replace("GO_SIGNAL", s.join("go.signal").to_str().unwrap()),
    )
    .unwrap();

    // build in the background; the sentinel only appears *after* the
    // snapshot pass (sections run later than staging)
    let recipes_t = recipes.clone();
    let builddir_t = s.join("build");
    let runner = std::thread::spawn(move || run_build(&recipes_t, &builddir_t, &[], "sealed"));
    let sentinel = s.join("build/sealed-1.0-1/.snap-ready");
    let mut seen = false;
    for _ in 0..600 {
        if sentinel.exists() {
            seen = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(seen, "the build never reached the install section");
    // mutate the checkout while the build is mid-flight: the snapshot was
    // hashed and copied before this write, so the build must not see it
    fs::write(r.join("files/config/x86_64.fragment"), b"CONFIG_A=n\n").unwrap();
    fs::write(s.join("go.signal"), b"go\n").unwrap();

    let (stdout, stderr, ok) = runner.join().unwrap();
    assert!(ok, "mid-build race build failed:\n{stdout}\n--- stderr ---\n{stderr}");
    let product =
        fs::read(s.join("build/sealed-1.0-1/pkg/usr/share/boot/x86_64.fragment")).unwrap();
    assert_eq!(
        product, b"CONFIG_A=y\n",
        "the build must see the sealed snapshot bytes, not the mutated checkout"
    );
    let _ = fs::remove_dir_all(&s);
}

// ---------------------------------------------------------------------------
// §14C per-name: a recipe that does not declare [assets] is refused when
// it references the assets names — and an assets-only recipe is refused
// when it references the srcroot names
// ---------------------------------------------------------------------------

#[test]
fn assets_placeholders_are_refused_per_name() {
    let s = scratch("placeholders");
    let recipes = s.join("recipes");

    // (a) legacy recipe with %{assetsdir}
    let r = recipes.join("pctassets");
    fs::create_dir_all(&r).unwrap();
    fs::write(
        r.join("recipe.toml"),
        r#"
[package]
name = "pctassets"
version = "1.0"
rel = 1
arch = ["x86_64"]

[build]
install = "echo %{assetsdir}"
"#,
    )
    .unwrap();
    let (_stdout, stderr, ok) = run_build(&recipes, &s.join("build"), &[], "pctassets");
    assert!(
        !ok,
        "a recipe without [assets] must not use %{{assetsdir}}"
    );
    assert!(
        stderr.contains("[assets]"),
        "the §14C error must name the fix (declare [assets]): {stderr}"
    );

    // (b) legacy recipe with $NAWA_ASSETSDIR
    let r = recipes.join("envassets");
    fs::create_dir_all(&r).unwrap();
    fs::write(
        r.join("recipe.toml"),
        r#"
[package]
name = "envassets"
version = "1.0"
rel = 1
arch = ["x86_64"]

[build]
install = "echo $NAWA_ASSETSDIR"
"#,
    )
    .unwrap();
    let (_stdout, stderr, ok) = run_build(&recipes, &s.join("build"), &[], "envassets");
    assert!(!ok, "a recipe without [assets] must not use NAWA_ASSETSDIR");
    assert!(
        stderr.contains("[assets]"),
        "the §14C error must name the fix (declare [assets]): {stderr}"
    );

    // (c) assets-only recipe with %{srcroot}: no sources, no workspace root
    let r = recipes.join("assetroot");
    fs::create_dir_all(r.join("files")).unwrap();
    fs::write(r.join("files/frag"), b"x\n").unwrap();
    fs::write(
        r.join("recipe.toml"),
        r#"
[package]
name = "assetroot"
version = "1.0"
rel = 1
arch = ["x86_64"]
source = "local:/nawa-schema-2-requires-a-newer-nawa"
source_date_epoch = 1700000000

[assets]
include = ["files/frag"]

[build]
install = "echo %{srcroot}"
"#,
    )
    .unwrap();
    let (_stdout, stderr, ok) = run_build(&recipes, &s.join("build"), &[], "assetroot");
    assert!(
        !ok,
        "a recipe without [[package.sources]] must not use %{{srcroot}}"
    );
    assert!(
        stderr.contains("%{srcroot}"),
        "the §14C error must name the placeholder: {stderr}"
    );
    let _ = fs::remove_dir_all(&s);
}

// ---------------------------------------------------------------------------
// §16 + §6: assets never feed SOURCE_DATE_EPOCH — even when the asset
// mtimes are far newer than the archive's
// ---------------------------------------------------------------------------

#[test]
fn assets_never_feed_source_date_epoch() {
    use filetime::FileTime;
    let s = scratch("sde");
    let recipes = s.join("recipes");
    let r = kernel_style_assets(&recipes, "sdeknob");
    // push every asset mtime far past the archive's newest regular file
    for p in [
        r.join("files/patches/0001-fix.patch"),
        r.join("files/config/x86_64.fragment"),
        r.join("files/scripts/configure-kernel.sh"),
    ] {
        filetime::set_file_mtime(&p, FileTime::from_unix_time(1_577_836_800, 0)).unwrap();
    }
    let tarball = fixture_tar_gz("main-1.0", &[("hello.sh", b"#!/bin/sh\necho main-ok\n")]);
    let (addr, count) = route_server(vec![("/src.tar.gz", Some(tarball.clone()))]);
    fs::write(
        r.join("recipe.toml"),
        format!(
            r#"
[package]
name = "sdeknob"
version = "1.0"
rel = 1
arch = ["x86_64"]
source = "local:/nawa-schema-2-requires-a-newer-nawa"

[[package.sources]]
name = "main"
url = "{addr}/src.tar.gz"
sha256 = "{}"

[assets]
include = ["files/patches", "files/config/x86_64.fragment", "files/scripts/configure-kernel.sh"]

[build]
install = """
set -e
# §6: the epoch derives from the fetched sources' newest regular-file mtime
# (500000, pinned by the fixture) — not from the much newer assets
test "$SOURCE_DATE_EPOCH" = "500000" || {{ echo "FAIL epoch $SOURCE_DATE_EPOCH"; exit 1; }}
mkdir -p "%{{pkgdir}}/usr/bin"
cp %{{srcroot}}/main-1.0/hello.sh %{{pkgdir}}/usr/bin/hello
"""
"#,
            sha256(&tarball)
        ),
    )
    .unwrap();

    let (stdout, stderr, ok) = run_build(&recipes, &s.join("build"), &[], "sdeknob");
    assert!(ok, "sde build failed:\n{stdout}\n--- stderr ---\n{stderr}");
    // assets never touch the network either — they are recipe-tree files
    // already covered by the repository trust chain (§5.4)
    assert_eq!(
        count.load(Ordering::SeqCst),
        1,
        "only the source archive may be fetched; assets never go over network (§5.4)"
    );
    let _ = fs::remove_dir_all(&s);
}