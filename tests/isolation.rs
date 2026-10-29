//! Build-isolation integration: `env` level, lint, rebuild-check and the
//! `sandbox`/`closure` levels through the real build engine
//! (docs/design/build-isolation.md §10–11).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn nawa() -> Command {
    Command::new(PathBuf::from(env!("CARGO_BIN_EXE_nawa")))
}

fn scratch(tag: &str) -> PathBuf {
    // NOT under /tmp: the sandbox mounts a fresh tmpfs over /tmp before the
    // staging binds, so host /tmp paths are invisible inside by then
    let d = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("nawa-iso-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(d.join("recipes/probe")).unwrap();
    d
}

fn write_recipe(dir: &Path, body: &str) {
    fs::write(dir.join("recipes/probe/recipe.toml"), body).unwrap();
}

fn build(dir: &Path, before: &[&str], after: &[&str]) -> Output {
    let mut c = nawa();
    c.env_remove("CFLAGS")
        .env_remove("CXXFLAGS")
        .env_remove("LDFLAGS")
        .args(["--root", dir.join("target-root").to_str().unwrap()])
        .args(["--recipes", dir.join("recipes").to_str().unwrap()])
        .args(["--builddir", dir.join("build").to_str().unwrap()])
        .args(["--profiles", dir.join("profiles").to_str().unwrap()])
        .args(before)
        .args(["build", "probe"])
        .args(after);
    c.output().unwrap()
}

fn out_str(o: &Output) -> String {
    format!(
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

fn pkginfo_of(dir: &Path) -> String {
    let out = dir.join("build/out/probe-1.0-1.nawa.tar.zst");
    let f = fs::File::open(out).unwrap();
    let dec = zstd::stream::read::Decoder::new(f).unwrap();
    let mut ar = tar::Archive::new(dec);
    for e in ar.entries().unwrap() {
        let mut e = e.unwrap();
        if e.path().unwrap().to_string_lossy() == ".PKGINFO" {
            let mut s = String::new();
            std::io::Read::read_to_string(&mut e, &mut s).unwrap();
            return s;
        }
    }
    panic!("no .PKGINFO in the built archive")
}

// --------------------------------------------------------------------------
// env level
// --------------------------------------------------------------------------

#[test]
fn env_level_whitelist_umask_and_pkginfo() {
    let d = scratch("env");
    write_recipe(
        &d,
        r#"
[package]
name = "probe"
version = "1.0"

[build]
compile = """
printf 'umask=%s\n' "$(umask)"
printf 'home=%s\n' "$HOME"
printf 'lc=%s\n' "$LC_ALL"
printf 'tz=%s\n' "$TZ"
printf 'sde=%s\n' "${SOURCE_DATE_EPOCH:-unset}"
"""
install = """
mkdir -p %{pkgdir}/usr/bin
printf '%s|%s|%s' "$CFLAGS" "$LC_ALL" "$NAWA_JOBS" > %{pkgdir}/usr/bin/seen
"""
"#,
    );
    let o = build(&d, &["--jobs", "5"], &["--isolation", "env"]);
    assert!(o.status.success(), "{}", out_str(&o));
    let so = String::from_utf8_lossy(&o.stdout);
    assert!(so.contains("umask=0022"), "umask must be 022: {so}");
    assert!(so.contains("home=/build/home"), "{so}");
    assert!(so.contains("lc=C"), "{so}");
    assert!(so.contains("tz=UTC"), "{so}");
    assert!(so.contains("sde=1\n") || so.contains("sde=1"), "meta packages fall back to SDE=1: {so}");

    let seen = fs::read_to_string(d.join("build/probe-1.0-1/pkg/usr/bin/seen")).unwrap();
    assert_eq!(seen, "|C|5");

    let pkginfo = pkginfo_of(&d);
    assert!(!pkginfo.contains("build_date"), "no wall clock at env+: {pkginfo}");
    assert!(pkginfo.contains("source_date_epoch = "), "{pkginfo}");
    assert!(pkginfo.contains("build_id = "), "{pkginfo}");
    let _ = fs::remove_dir_all(&d);
}

#[test]
fn env_level_rebuild_check_is_reproducible() {
    let d = scratch("repro");
    write_recipe(
        &d,
        r#"
[package]
name = "probe"
version = "1.0"

[build]
compile = "echo building"
install = """
mkdir -p %{pkgdir}/usr/bin %{pkgdir}/etc
printf 'hello\n' > %{pkgdir}/usr/bin/probe
touch -d @1700000000 %{pkgdir}/usr/bin/probe %{pkgdir}/etc
"""
"#,
    );
    let o = build(&d, &[], &["--isolation", "env", "--rebuild-check"]);
    assert!(o.status.success(), "{}", out_str(&o));
    assert!(
        String::from_utf8_lossy(&o.stdout).contains("rebuild-check: OK"),
        "{}",
        out_str(&o)
    );
    let _ = fs::remove_dir_all(&d);
}

#[test]
fn lint_rejects_leaked_build_paths_and_layout() {
    let d = scratch("lint");
    write_recipe(
        &d,
        r#"
[package]
name = "probe"
version = "1.0"

[build]
install = """
mkdir -p %{pkgdir}/usr/lib/pkgconfig %{pkgdir}/tmp
printf 'prefix=/usr\nlibdir=/build/src/x\n' > %{pkgdir}/usr/lib/pkgconfig/probe.pc
printf 'x' > %{pkgdir}/tmp/stray
"""
"#,
    );
    let o = build(&d, &[], &["--isolation", "env"]);
    assert!(!o.status.success(), "leaked paths and stray /tmp must fail the build");
    let se = String::from_utf8_lossy(&o.stderr);
    assert!(se.contains("/build/src"), "{se}");
    assert!(se.contains("lint"), "{se}");
    assert!(!d.join("build/out/probe-1.0-1.nawa.tar.zst").exists(), "no archive on lint failure");
    let _ = fs::remove_dir_all(&d);
}

#[test]
fn allow_network_records_impure() {
    let d = scratch("net");
    write_recipe(
        &d,
        r#"
[package]
name = "probe"
version = "1.0"

[sandbox]
network = true

[build]
install = """
mkdir -p %{pkgdir}/usr/bin
echo x > %{pkgdir}/usr/bin/probe
"""
"#,
    );
    let o = build(&d, &[], &["--isolation", "env", "--allow-network"]);
    assert!(o.status.success(), "{}", out_str(&o));
    let pkginfo = pkginfo_of(&d);
    assert!(pkginfo.contains("impure = network"), "{pkginfo}");
    let _ = fs::remove_dir_all(&d);
}

// --------------------------------------------------------------------------
// sandbox / closure levels
// --------------------------------------------------------------------------

fn kernel_probe_ok(op: &str) -> bool {
    let out = nawa().arg("sandbox-probe").arg(op).output().unwrap();
    out.status.success()
}

#[test]
fn sandbox_level_build_runs_and_produces_a_normalized_archive() {
    if !kernel_probe_ok("userns") {
        eprintln!("SKIP: this kernel/container does not allow user namespaces");
        return;
    }
    let d = scratch("sandbox");
    write_recipe(
        &d,
        r#"
[package]
name = "probe"
version = "1.0"

[build]
compile = """
printf "uid=%s home=%s\n" "$(id -u)" "$HOME"
"""
install = """
mkdir -p %{pkgdir}/usr/bin
echo x > %{pkgdir}/usr/bin/probe
"""
"#,
    );
    let o = build(&d, &[], &["--isolation", "sandbox", "--allow-degraded"]);
    if !o.status.success()
        && String::from_utf8_lossy(&o.stderr).contains("cannot create a user namespace")
    {
        eprintln!("SKIP: user namespaces turned off mid-run");
        return;
    }
    assert!(o.status.success(), "{}", out_str(&o));
    let so = String::from_utf8_lossy(&o.stdout);
    assert!(so.contains("uid=0"), "the build must run as root inside: {so}");
    assert!(
        so.contains("home=/build/home") || so.contains("home=/tmp/.nawa-sandbox/home"),
        "HOME must be the sandbox home: {so}"
    );
    let pkginfo = pkginfo_of(&d);
    assert!(pkginfo.contains("source_date_epoch = "), "{pkginfo}");
    let _ = fs::remove_dir_all(&d);
}

#[test]
fn sandbox_level_refuses_without_allow_degraded_on_restricted_hosts() {
    if !kernel_probe_ok("userns") {
        eprintln!("SKIP: this kernel/container does not allow user namespaces");
        return;
    }
    if kernel_probe_ok("proc") {
        // a full host has nothing to degrade: the strict build succeeds
        eprintln!("SKIP: this host allows every isolation feature");
        return;
    }
    let d = scratch("strict");
    write_recipe(
        &d,
        r#"
[package]
name = "probe"
version = "1.0"

[build]
install = """
mkdir -p %{pkgdir}/usr/bin
echo x > %{pkgdir}/usr/bin/probe
"""
"#,
    );
    let o = build(&d, &[], &["--isolation", "sandbox"]);
    assert!(
        !o.status.success(),
        "a restricted host must refuse instead of weakening isolation silently"
    );
    let se = String::from_utf8_lossy(&o.stderr);
    assert!(
        se.contains("fresh /proc") || se.contains("--allow-degraded"),
        "the refusal must say what to do: {se}"
    );
    let _ = fs::remove_dir_all(&d);
}

/// The full closure dance: a `base-devel` whose archive carries a real bash
/// + its libraries (copied from the build host), a dependency package, and a
///   target that builds with `--isolation closure` — inside a sandbox whose
///   root contains ONLY those two archives, never the host.
#[test]
fn closure_level_builds_against_assembled_archives_only() {
    if !kernel_probe_ok("userns") {
        eprintln!("SKIP: this kernel/container does not allow user namespaces");
        return;
    }
    let d = scratch("closure");
    fs::create_dir_all(d.join("recipes/base-devel")).unwrap();
    fs::create_dir_all(d.join("recipes/deptool")).unwrap();

    // a self-contained base-devel for the test: bash plus every .so it
    // needs, so the closure root can actually run the section shell
    fs::write(
        d.join("recipes/base-devel/recipe.toml"),
        r#"
[package]
name = "base-devel"
version = "1"
rel = 1

[build]
install = '''
mkdir -p %{pkgdir}/usr/bin
# a tiny but real toolchain: the shell plus the utilities the recipe below
# uses, with every library ldd reports (loader included)
for tool in bash mkdir ls; do
    cp -L "/usr/bin/$tool" "%{pkgdir}/usr/bin/$tool"
    # ldd prints deps as "lib => /path (addr)" and the loader as "/path (addr)"
    for lib in $(ldd "/usr/bin/$tool" | awk '$1 ~ /^\// {print $1} $3 ~ /^\// {print $3}'); do
        cp -L --parents "$lib" %{pkgdir}/
    done
done
'''
"#,
    )
    .unwrap();

    fs::write(
        d.join("recipes/deptool/recipe.toml"),
        r#"
[package]
name = "deptool"
version = "1"
rel = 1

[build]
install = """
mkdir -p %{pkgdir}/usr/lib
printf 'fake lib for the soname index\n' > %{pkgdir}/usr/lib/libdeptool.so
"""
"#,
    )
    .unwrap();

    // build both dependencies first (env level: fast, no sandbox needed)
    for pkg in ["base-devel", "deptool"] {
        let mut c = nawa();
        c.env_remove("CFLAGS").env_remove("CXXFLAGS").env_remove("LDFLAGS")
            .args(["--root", d.join("target-root").to_str().unwrap()])
            .args(["--recipes", d.join("recipes").to_str().unwrap()])
            .args(["--builddir", d.join("build").to_str().unwrap()])
            .args(["--profiles", d.join("profiles").to_str().unwrap()])
            .args(["build", pkg, "--isolation", "env"]);
        let o = c.output().unwrap();
        assert!(o.status.success(), "{pkg}: {}", out_str(&o));
    }

    write_recipe(
        &d,
        r#"
[package]
name = "probe"
version = "1.0"

[depends]
build = ["deptool"]

[build]
compile = """
# the build root contains ONLY base-devel + deptool: proving the closure
echo "bash=$(command -v bash)"
echo "deptool-lib=$(ls /usr/lib/libdeptool.so 2>/dev/null || echo MISSING)"
if command -v gcc >/dev/null 2>&1; then
    echo "GCCC-PRESENT-IN-CLOSURE  <- must not happen"
    exit 3
fi
"""
install = """
mkdir -p %{pkgdir}/usr/bin
echo x > %{pkgdir}/usr/bin/probe
"""
"#,
    );

    let o = build(&d, &[], &["--isolation", "closure", "--allow-degraded"]);
    if !o.status.success() && String::from_utf8_lossy(&o.stderr).contains("cannot create a user namespace") {
        eprintln!("SKIP: user namespaces turned off mid-run");
        return;
    }
    assert!(o.status.success(), "{}", out_str(&o));
    let so = String::from_utf8_lossy(&o.stdout);
    assert!(so.contains("bash=/usr/bin/bash"), "{so}");
    assert!(so.contains("deptool-lib=/usr/lib/libdeptool.so"), "{so}");
    assert!(
        !so.contains("GCCC-PRESENT-IN-CLOSURE"),
        "the host toolchain leaked into the closure build root: {so}"
    );
    let _ = fs::remove_dir_all(&d);
}

// --------------------------------------------------------------------------
// Phase 5: layer cache (§5.3 v2) + build_id cache hits (§7)
// --------------------------------------------------------------------------

/// The closure build must work through **both** assembly strategies: layered
/// (overlayfs in a user namespace, kernel ≥ 5.11) and extract-per-build
/// (the fallback this restricted container exercises). Either way the
/// package is built and the layer cache gets populated.
#[test]
fn closure_assembly_layer_cache_with_extract_fallback() {
    if !kernel_probe_ok("userns") {
        eprintln!("SKIP: this kernel/container does not allow user namespaces");
        return;
    }
    let d = scratch("layers");
    fs::create_dir_all(d.join("recipes/base-devel")).unwrap();
    fs::write(
        d.join("recipes/base-devel/recipe.toml"),
        r#"
[package]
name = "base-devel"
version = "1"
rel = 1

[build]
install = '''
mkdir -p %{pkgdir}/usr/bin
for tool in bash mkdir ls; do
    cp -L "/usr/bin/$tool" "%{pkgdir}/usr/bin/$tool"
    for lib in $(ldd "/usr/bin/$tool" | awk '$1 ~ /^\// {print $1} $3 ~ /^\// {print $3}'); do
        cp -L --parents "$lib" %{pkgdir}/
    done
done
'''
"#,
    )
    .unwrap();
    write_recipe(
        &d,
        r#"
[package]
name = "probe"
version = "1.0"

[build]
compile = "echo bash=$(command -v bash)"
install = """
mkdir -p %{pkgdir}/usr/bin
echo x > %{pkgdir}/usr/bin/probe
"""
"#,
    );

    // build base-devel first, then the probe against it with the layer
    // cache redirected into the scratch dir (NAWA_LAYER_CACHE)
    let mut c = nawa();
    c.env_remove("CFLAGS").env_remove("CXXFLAGS").env_remove("LDFLAGS")
        .args(["--root", d.join("target-root").to_str().unwrap()])
        .args(["--recipes", d.join("recipes").to_str().unwrap()])
        .args(["--builddir", d.join("build").to_str().unwrap()])
        .args(["--profiles", d.join("profiles").to_str().unwrap()])
        .args(["build", "base-devel", "--isolation", "env"]);
    let o = c.output().unwrap();
    assert!(o.status.success(), "{}", out_str(&o));

    let mut c = nawa();
    c.env_remove("CFLAGS").env_remove("CXXFLAGS").env_remove("LDFLAGS")
        .env("NAWA_LAYER_CACHE", d.join("layers").to_str().unwrap())
        .args(["--root", d.join("target-root").to_str().unwrap()])
        .args(["--recipes", d.join("recipes").to_str().unwrap()])
        .args(["--builddir", d.join("build").to_str().unwrap()])
        .args(["--profiles", d.join("profiles").to_str().unwrap()])
        .args(["build", "probe", "--isolation", "closure", "--allow-degraded"]);
    let o = c.output().unwrap();
    if !o.status.success()
        && String::from_utf8_lossy(&o.stderr).contains("cannot create a user namespace")
    {
        eprintln!("SKIP: user namespaces turned off mid-run");
        return;
    }
    assert!(o.status.success(), "{}", out_str(&o));
    let so = String::from_utf8_lossy(&o.stdout);
    assert!(
        so.contains("assembling the build root"),
        "the assembly strategy must be announced: {so}"
    );

    // count cached layers; the assertions depend on which strategy ran
    let layers = d.join("layers/layers");
    let mut layer_count = 0;
    if let Ok(rd) = fs::read_dir(&layers) {
        for e in rd.filter_map(|e| e.ok()) {
            if e.path().is_dir() {
                layer_count += 1;
            }
        }
    }
    if so.contains("layer cache (overlayfs)") {
        assert!(
            layer_count >= 1,
            "the layered run must populate the layer cache"
        );
        assert!(
            so.contains("layer cache hit") || so.contains("(new layer)"),
            "the layered run must report layer activity: {so}"
        );
        // the second build uses its own builddir, so give it the dependency
        // archive built into the first one
        fs::create_dir_all(d.join("build2/out")).unwrap();
        fs::copy(
            d.join("build/out/base-devel-1-1.nawa.tar.zst"),
            d.join("build2/out/base-devel-1-1.nawa.tar.zst"),
        )
        .unwrap();
        // second build: the layer is already cached
        let o = nawa()
            .env_remove("CFLAGS").env_remove("CXXFLAGS").env_remove("LDFLAGS")
            .env("NAWA_LAYER_CACHE", d.join("layers").to_str().unwrap())
            .args(["--root", d.join("target-root").to_str().unwrap()])
            .args(["--recipes", d.join("recipes").to_str().unwrap()])
            .args(["--builddir", d.join("build2").to_str().unwrap()])
            .args(["--profiles", d.join("profiles").to_str().unwrap()])
            .args(["build", "probe", "--isolation", "closure", "--allow-degraded"])
            .output()
            .unwrap();
        assert!(o.status.success(), "{}", out_str(&o));
        let so2 = String::from_utf8_lossy(&o.stdout);
        assert!(
            so2.contains("layer cache hit"),
            "the second layered build must hit the cache: {so2}"
        );
        assert!(
            so2.contains("[build root: layer cache]"),
            "the built line must tag the layered root: {so2}"
        );
    } else {
        assert!(
            so.contains("extract-per-build"),
            "without overlay support the fallback must be explicit: {so}"
        );
        assert_eq!(
            layer_count, 0,
            "extract-per-build must not touch the layer cache"
        );
    }
    let _ = fs::remove_dir_all(&d);
}

/// `--use-cache`: the second build of identical inputs is a hit (nothing
/// runs); editing the recipe invalidates the build_id and forces a rebuild.
#[test]
fn use_cache_skips_identical_rebuilds_and_rebuilds_on_change() {
    let d = scratch("cache");
    write_recipe(
        &d,
        r#"
[package]
name = "probe"
version = "1.0"

[build]
compile = "echo BUILDING-NOW"
install = """
mkdir -p %{pkgdir}/usr/bin
echo x > %{pkgdir}/usr/bin/probe
"""
"#,
    );
    let build = |d: &Path| {
        nawa()
            .env_remove("CFLAGS").env_remove("CXXFLAGS").env_remove("LDFLAGS")
            .args(["--root", d.join("target-root").to_str().unwrap()])
            .args(["--recipes", d.join("recipes").to_str().unwrap()])
            .args(["--builddir", d.join("build").to_str().unwrap()])
            .args(["--profiles", d.join("profiles").to_str().unwrap()])
            .args(["build", "probe", "--isolation", "env", "--use-cache"])
            .output()
            .unwrap()
    };
    let o1 = build(&d);
    assert!(o1.status.success(), "{}", out_str(&o1));
    let so1 = String::from_utf8_lossy(&o1.stdout);
    assert!(so1.contains("BUILDING-NOW"), "first build really builds: {so1}");
    assert!(!so1.contains("nothing to do"), "no hit on the first build: {so1}");

    let o2 = build(&d);
    assert!(o2.status.success(), "{}", out_str(&o2));
    let so2 = String::from_utf8_lossy(&o2.stdout);
    assert!(so2.contains("build cache hit"), "second build must hit: {so2}");
    assert!(
        !so2.contains("BUILDING-NOW"),
        "a cache hit must not run any build section: {so2}"
    );

    // recipe edit -> build_id mismatch -> real rebuild
    let recipe_path = d.join("recipes/probe/recipe.toml");
    let edited = fs::read_to_string(&recipe_path)
        .unwrap()
        .replace("version = \"1.0\"", "version = \"1.1\"");
    fs::write(&recipe_path, edited).unwrap();
    let o3 = build(&d);
    assert!(o3.status.success(), "{}", out_str(&o3));
    let so3 = String::from_utf8_lossy(&o3.stdout);
    assert!(so3.contains("BUILDING-NOW"), "changed inputs must rebuild: {so3}");
    let _ = fs::remove_dir_all(&d);
}
