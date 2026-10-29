//! Build-engine behaviour: profile flags, live output, optional `check`.
//! Uses meta recipes (no downloads) in a scratch dir.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn nawa_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_nawa"))
}

/// A nawa invocation isolated from the ambient configuration: no NAWA_* env
/// and a cwd outside the repository, so the repo's own `nawa.toml` (and the
/// profile it points at) is never discovered.
fn nawa() -> Command {
    let mut c = Command::new(nawa_bin());
    for k in [
        "NAWA_CONFIG", "NAWA_ROOT", "NAWA_RECIPES", "NAWA_BUILDDIR",
        "NAWA_PROFILES", "NAWA_JOBS", "NAWA_REPO",
    ] {
        c.env_remove(k);
    }
    c.current_dir(std::env::temp_dir());
    c
}

fn scratch(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("nawa-be-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

fn write_recipe(dir: &Path) {
    let r = dir.join("recipes/probe");
    fs::create_dir_all(&r).unwrap();
    fs::write(
        r.join("recipe.toml"),
        r#"
[package]
name = "probe"
version = "1.0"
rel = 1
arch = ["x86_64"]

[build]
compile = """
echo out-line-from-compile
echo err-line-from-compile >&2
"""
check = """
echo running-the-check
exit 3
"""
install = """
mkdir -p %{pkgdir}/usr
printf '%s|%s|%s' "$CFLAGS" "$LDFLAGS" "$NAWA_JOBS" > %{pkgdir}/usr/seen
"""
"#,
    )
    .unwrap();
}

fn build(dir: &Path, extra_before: &[&str], extra_after: &[&str]) -> Output {
    let mut c = nawa();
    c.env_remove("CFLAGS")
        .env_remove("CXXFLAGS")
        .env_remove("LDFLAGS")
        .args(["--recipes", dir.join("recipes").to_str().unwrap()])
        .args(["--builddir", dir.join("build").to_str().unwrap()])
        .args(["--profiles", dir.join("profiles").to_str().unwrap()])
        .args(extra_before)
        .args(["build", "probe"])
        .args(extra_after);
    c.output().unwrap()
}

fn seen(dir: &Path) -> String {
    fs::read_to_string(dir.join("build/probe-1.0-1/pkg/usr/seen")).unwrap()
}

#[test]
fn profile_flags_and_jobs_reach_the_build_and_cli_jobs_win() {
    let d = scratch("profile");
    write_recipe(&d);
    fs::create_dir_all(d.join("profiles")).unwrap();
    fs::write(
        d.join("profiles/x86_64.toml"),
        "[profile]\narch = \"x86_64\"\ncflags = \"-O2 -pipe\"\nldflags = \"-Wl,-O1\"\njobs = 3\n",
    )
    .unwrap();

    let out = build(&d, &[], &[]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(seen(&d), "-O2 -pipe|-Wl,-O1|3");

    let out = build(&d, &["--jobs", "7"], &[]);
    assert!(out.status.success());
    assert_eq!(seen(&d), "-O2 -pipe|-Wl,-O1|7");
    let _ = fs::remove_dir_all(&d);
}

#[test]
fn missing_profile_leaves_flags_unset() {
    let d = scratch("noprofile");
    write_recipe(&d);
    let out = build(&d, &["--jobs", "2"], &[]);
    assert!(out.status.success());
    assert_eq!(seen(&d), "||2");
    let _ = fs::remove_dir_all(&d);
}

#[test]
fn output_is_streamed_to_console_and_log() {
    let d = scratch("stream");
    write_recipe(&d);
    let out = build(&d, &[], &[]);
    assert!(out.status.success());
    let so = String::from_utf8_lossy(&out.stdout);
    assert!(so.contains("out-line-from-compile"), "stdout: {so}");
    assert!(so.contains("err-line-from-compile"), "stderr must be merged: {so}");
    let log = fs::read_to_string(d.join("build/probe-1.0-1/build-compile.log")).unwrap();
    assert!(log.contains("out-line-from-compile") && log.contains("err-line-from-compile"));
    let _ = fs::remove_dir_all(&d);
}

#[test]
fn check_is_skipped_by_default_fatal_with_flag_and_warns_with_nonfatal() {
    let d = scratch("check");
    write_recipe(&d);

    // default: the failing check never runs
    let out = build(&d, &[], &[]);
    assert!(out.status.success());
    assert!(!String::from_utf8_lossy(&out.stdout).contains("running-the-check"));

    // --check: runs, failure aborts, tail of output is quoted
    let out = build(&d, &[], &["--check"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("running-the-check"));
    assert!(String::from_utf8_lossy(&out.stderr).contains("check failed"));

    // --check-nonfatal: runs, warns, build still produces the archive
    let out = build(&d, &[], &["--check-nonfatal"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stderr).contains("warning: check failed"));
    assert!(d.join("build/out/probe-1.0-1.nawa.tar.zst").exists());
    let _ = fs::remove_dir_all(&d);
}
