//! Phase 7 (docs/design/build-isolation.md §10): `nawa build` as a **normal
//! user** — the rootless mode.
//!
//! The full closure dance (base-devel with a real bash, a dependency, a
//! sandboxed build against the assembled root) must work unchanged when nawa
//! is not root: the sandbox self-writes its own uid map (0 → caller) and
//! stages the build under its private /tmp. When the test runner is root
//! (the usual CI/bootstrap case) we drop to `nobody` with setpriv; when the
//! runner is already unprivileged, the test *is* the rootless case.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn nawa_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_nawa"))
}

fn scratch(tag: &str) -> PathBuf {
    let d = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("nawa-rootless-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(d.join("recipes/base-devel")).unwrap();
    fs::create_dir_all(d.join("recipes/deptool")).unwrap();
    fs::create_dir_all(d.join("recipes/probe")).unwrap();
    d
}

fn out_str(o: &Output) -> String {
    format!(
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

fn write_recipes(d: &Path) {
    // base-devel: bash + every library it needs (copied from the build host)
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
printf 'fake lib\n' > %{pkgdir}/usr/lib/libdeptool.so
"""
"#,
    )
    .unwrap();

    fs::write(
        d.join("recipes/probe/recipe.toml"),
        r#"
[package]
name = "probe"
version = "1.0"

[depends]
build = ["deptool"]

[build]
compile = """
echo "bash=$(command -v bash)"
echo "uid-inside=$UID"
echo "deptool-lib=$(ls /usr/lib/libdeptool.so 2>/dev/null || echo MISSING)"
if command -v gcc >/dev/null 2>&1; then
    echo "GCCC-PRESENT-IN-CLOSURE <- must not happen"
    exit 3
fi
"""
install = """
mkdir -p %{pkgdir}/usr/bin
echo x > %{pkgdir}/usr/bin/probe
"""
"#,
    )
    .unwrap();
}

/// Build the two dependencies (env level, no sandbox needed) and then the
/// target with `--isolation closure`, all through `runner`.
fn run_full_closure_dance(d: &Path, runner: &mut dyn FnMut(&mut Command) -> Output) {
    for pkg in ["base-devel", "deptool"] {
        let mut c = Command::new(nawa_path());
        c.env_remove("CFLAGS").env_remove("CXXFLAGS").env_remove("LDFLAGS")
            .args(["--root", d.join("target-root").to_str().unwrap()])
            .args(["--recipes", d.join("recipes").to_str().unwrap()])
            .args(["--builddir", d.join("build").to_str().unwrap()])
            .args(["--profiles", d.join("profiles").to_str().unwrap()])
            .args(["build", pkg, "--isolation", "env"]);
        let o = runner(&mut c);
        assert!(o.status.success(), "{pkg}: {}", out_str(&o));
    }

    let mut c = Command::new(nawa_path());
    c.env_remove("CFLAGS").env_remove("CXXFLAGS").env_remove("LDFLAGS")
        .args(["--root", d.join("target-root").to_str().unwrap()])
        .args(["--recipes", d.join("recipes").to_str().unwrap()])
        .args(["--builddir", d.join("build").to_str().unwrap()])
        .args(["--profiles", d.join("profiles").to_str().unwrap()])
        .args([
            "build", "probe",
            "--isolation", "closure",
            // restricted containers (no fresh /proc) need this; a full host
            // ignores it
            "--allow-degraded",
        ]);
    let o = runner(&mut c);
    if !o.status.success()
        && String::from_utf8_lossy(&o.stderr).contains("cannot create a user namespace")
    {
        eprintln!("SKIP: user namespaces turned off mid-run");
        return;
    }
    assert!(o.status.success(), "{}", out_str(&o));
    let so = String::from_utf8_lossy(&o.stdout);
    assert!(so.contains("bash=/usr/bin/bash"), "{so}");
    assert!(so.contains("uid-inside=0"), "the build runs as (fake) root inside: {so}");
    assert!(so.contains("deptool-lib=/usr/lib/libdeptool.so"), "{so}");
    assert!(
        !so.contains("GCCC-PRESENT-IN-CLOSURE"),
        "the host toolchain leaked into a rootless closure build: {so}"
    );
    assert!(
        d.join("build/out/probe-1.0-1.nawa.tar.zst").exists(),
        "the rootless build must still produce the archive"
    );
}

#[test]
fn closure_build_works_as_a_normal_user() {
    // runner == this process; if it is root, the second test covers rootless
    extern "C" {
        fn geteuid() -> u32;
    }
    if unsafe { geteuid() } == 0 {
        eprintln!("SKIP: test runner is root — the setpriv test covers rootless");
        return;
    }
    let d = scratch("self");
    write_recipes(&d);
    run_full_closure_dance(&d, &mut |c| c.output().unwrap());
    let _ = fs::remove_dir_all(&d);
}

#[test]
fn closure_build_works_as_nobody() {
    // runner is root: drop to nobody for the whole dance (uid 65534)
    extern "C" {
        fn geteuid() -> u32;
    }
    if unsafe { geteuid() } != 0 {
        eprintln!("SKIP: test runner is not root — the self test covers rootless");
        return;
    }
    let d = scratch("nobody");
    write_recipes(&d);
    // everything under the scratch must be owned (and traversable) by nobody
    for p in [d.clone(), d.join("recipes"), d.join("profiles")] {
        let _ = fs::set_permissions(&p, fs::Permissions::from_mode(0o755));
    }
    chown_all(&d, 65534, 65534);

    // a writable HOME: nawa must not need one, but setpriv clears the env
    let mut run_as_nobody = |c: &mut Command| {
        let _ = fs::create_dir_all(d.join("home"));
        let _ = fs::set_permissions(d.join("home"), fs::Permissions::from_mode(0o777));
        Command::new("setpriv")
            .args(["--reuid", "65534", "--regid", "65534", "--clear-groups"])
            .arg("/usr/bin/env")
            .arg(format!("PATH={}", "/usr/bin:/bin:/usr/sbin:/sbin"))
            .arg(format!("HOME={}", d.join("home").display()))
            .arg(c.get_program())
            .args(c.get_args())
            .output()
            .unwrap()
    };
    run_full_closure_dance(&d, &mut run_as_nobody);
    let _ = fs::remove_dir_all(&d);
}

/// chown -R via std (the test runs as root here).
fn chown_all(dir: &Path, uid: u32, gid: u32) {
    use std::os::unix::fs::chown;
    let _ = chown(dir, Some(uid), Some(gid));
    for e in walkdir::WalkDir::new(dir) {
        let e = match e {
            Ok(e) => e,
            Err(_) => continue,
        };
        let _ = chown(e.path(), Some(uid), Some(gid));
    }
}
