//! `nawa install <name>` with a binary repository: install from the repository
//! when it has the package, build from the recipe when it does not, and let
//! the user force either (`--build`, `--from-repo`).
//!
//! A local directory repository is used — same code path as the remote
//! default except for the download — so nothing here touches the network.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn nawa_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_nawa"))
}

/// A meta recipe that installs `/usr/share/<name>` containing `<name>-<version>`.
fn recipe(dir: &Path, name: &str, version: &str) {
    let r = dir.join("recipes").join(name);
    fs::create_dir_all(&r).unwrap();
    fs::write(
        r.join("recipe.toml"),
        format!(
            "[package]\nname = \"{name}\"\nversion = \"{version}\"\nrel = 1\narch = [\"x86_64\"]\n\
             [build]\ninstall = \"mkdir -p %{{pkgdir}}/usr/share && echo {name}-{version} > %{{pkgdir}}/usr/share/{name}\"\n"
        ),
    )
    .unwrap();
}

/// A repository archive that installs `/usr/share/<name>` containing `body`.
fn archive(out: &Path, name: &str, ver: &str, body: &str) {
    let pkginfo = format!(
        "pkgname = {name}\npkgver = {ver}\npkgrel = 1\narch = any\nrundeps = \nsize = 1\n"
    );
    let f = fs::File::create(out).unwrap();
    let enc = zstd::stream::write::Encoder::new(f, 3).unwrap();
    let mut tar = tar::Builder::new(enc);
    let mut hdr = tar::Header::new_gnu();
    hdr.set_size(pkginfo.len() as u64);
    hdr.set_mode(0o644);
    hdr.set_cksum();
    tar.append_data(&mut hdr, ".PKGINFO", pkginfo.as_bytes()).unwrap();
    let mut h2 = tar::Header::new_gnu();
    h2.set_size(body.len() as u64);
    h2.set_mode(0o644);
    h2.set_cksum();
    tar.append_data(&mut h2, format!("payload/usr/share/{name}"), body.as_bytes())
        .unwrap();
    tar.into_inner().unwrap().finish().unwrap();
}

fn scratch(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("nawa-repoinst-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(d.join("root")).unwrap();
    d
}

/// Run nawa against the scratch tree; `mode` is NAWA_INSTALL_MODE (None = the
/// built-in default, i.e. auto). The host's /etc/nawa/nawa.toml is never read.
fn nawa(dir: &Path, args: &[&str]) -> Output {
    Command::new(nawa_bin())
        .env_remove("NAWA_CONFIG")
        .env_remove("NAWA_REPO")
        .env_remove("NAWA_INSTALL_MODE")
        .env("NAWA_SYSCONFDIR", dir.join("no-sysconf"))
        .env("NO_COLOR", "1")
        .current_dir(dir)
        .args(["--recipes", dir.join("recipes").to_str().unwrap()])
        .args(["--builddir", dir.join("build").to_str().unwrap()])
        .args(["--profiles", dir.join("profiles").to_str().unwrap()])
        .args(["--root", dir.join("root").to_str().unwrap()])
        .args(args)
        .output()
        .unwrap()
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).to_string()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).to_string()
}

fn ok(o: &Output) -> String {
    assert!(o.status.success(), "failed:\n{}\n{}", stdout(o), stderr(o));
    stdout(o)
}

/// A repository holding `hello` 1 (body "from-repo"), index written.
fn make_repo(d: &Path) -> PathBuf {
    let repo = d.join("repo");
    fs::create_dir_all(&repo).unwrap();
    archive(&repo.join("hello-1-1.nawa.tar.zst"), "hello", "1", "from-repo\n");
    let o = Command::new(nawa_bin())
        .args(["repo-create", repo.to_str().unwrap(), "--out", "index.toml"])
        .env("NAWA_SYSCONFDIR", d.join("no-sysconf"))
        .output()
        .unwrap();
    ok(&o);
    repo
}

fn installed(d: &Path, name: &str) -> Option<String> {
    fs::read_to_string(d.join("root/usr/share").join(name)).ok()
}

#[test]
fn auto_installs_from_the_repo_and_builds_what_it_lacks() {
    let d = scratch("auto");
    recipe(&d, "hello", "1");
    recipe(&d, "other", "1");
    let repo = make_repo(&d);

    let out = ok(&nawa(
        &d,
        &[
            "install", "--yes", "--allow-unsigned",
            "--repo", repo.to_str().unwrap(),
            "hello", "other",
        ],
    ));
    assert_eq!(installed(&d, "hello").as_deref(), Some("from-repo\n"), "{out}");
    assert_eq!(installed(&d, "other").as_deref(), Some("other-1\n"), "built locally:\n{out}");
    assert!(out.contains("1 from the repository, 1 to build"), "{out}");
    let _ = fs::remove_dir_all(&d);
}

#[test]
fn build_flag_ignores_the_repository() {
    let d = scratch("build");
    recipe(&d, "hello", "1");
    let repo = make_repo(&d);

    ok(&nawa(
        &d,
        &["install", "--build", "--yes", "--repo", repo.to_str().unwrap(), "hello"],
    ));
    assert_eq!(installed(&d, "hello").as_deref(), Some("hello-1\n"));
    let _ = fs::remove_dir_all(&d);
}

#[test]
fn from_repo_is_strict_and_fails_before_touching_anything() {
    let d = scratch("strict");
    recipe(&d, "hello", "1");
    recipe(&d, "other", "1");
    let repo = make_repo(&d);

    let o = nawa(
        &d,
        &[
            "install", "--from-repo", "--yes", "--allow-unsigned",
            "--repo", repo.to_str().unwrap(),
            "hello", "other",
        ],
    );
    assert!(!o.status.success(), "{}", stdout(&o));
    assert!(stderr(&o).contains("not in the repository"), "{}", stderr(&o));
    assert_eq!(installed(&d, "hello"), None, "nothing may be installed after a strict failure");
    assert_eq!(installed(&d, "other"), None);
    let _ = fs::remove_dir_all(&d);
}

#[test]
fn a_repository_only_package_needs_no_recipe() {
    let d = scratch("binonly");
    let repo = make_repo(&d); // no recipe for `hello` at all
    fs::create_dir_all(d.join("recipes")).unwrap();

    let out = ok(&nawa(
        &d,
        &["install", "--yes", "--allow-unsigned", "--repo", repo.to_str().unwrap(), "hello"],
    ));
    assert_eq!(installed(&d, "hello").as_deref(), Some("from-repo\n"), "{out}");
    let _ = fs::remove_dir_all(&d);
}

#[test]
fn an_unusable_repository_falls_back_to_building_in_auto_mode() {
    let d = scratch("fallback");
    recipe(&d, "hello", "1");
    let missing = d.join("no-such-repo");

    let out = ok(&nawa(
        &d,
        &["install", "--yes", "--repo", missing.to_str().unwrap(), "hello"],
    ));
    assert_eq!(installed(&d, "hello").as_deref(), Some("hello-1\n"), "{out}");
    assert!(out.contains("not usable"), "one note line explains the fallback:\n{out}");

    // ... but strict repo mode reports the problem instead of building
    let o = nawa(
        &d,
        &["install", "--from-repo", "--yes", "--repo", missing.to_str().unwrap(), "other"],
    );
    assert!(!o.status.success());
    assert!(stderr(&o).contains("not usable"), "{}", stderr(&o));
    let _ = fs::remove_dir_all(&d);
}

#[test]
fn install_mode_comes_from_the_system_config_and_flags_beat_it() {
    let d = scratch("sysconf");
    recipe(&d, "hello", "1");
    let repo = make_repo(&d);

    // /etc/nawa/nawa.toml (here: a temp sysconf dir) says "always build"
    let sys = d.join("sysconf");
    fs::create_dir_all(&sys).unwrap();
    fs::write(
        sys.join("nawa.toml"),
        format!("install_mode = \"build\"\nrepo = \"{}\"\n", repo.display()),
    )
    .unwrap();

    let run = |extra: &[&str]| {
        let mut args = vec!["install", "--yes", "--allow-unsigned"];
        args.extend_from_slice(extra);
        args.push("hello");
        Command::new(nawa_bin())
            .env_remove("NAWA_CONFIG")
            .env_remove("NAWA_REPO")
            .env_remove("NAWA_INSTALL_MODE")
            .env("NAWA_SYSCONFDIR", &sys)
            .env("NO_COLOR", "1")
            .current_dir(&d)
            .args(["--recipes", d.join("recipes").to_str().unwrap()])
            .args(["--builddir", d.join("build").to_str().unwrap()])
            .args(["--profiles", d.join("profiles").to_str().unwrap()])
            .args(["--root", d.join("root").to_str().unwrap()])
            .args(&args)
            .output()
            .unwrap()
    };

    ok(&run(&[]));
    assert_eq!(installed(&d, "hello").as_deref(), Some("hello-1\n"), "system config: build");

    // the flag wins over the system file; the repo still comes from /etc
    let o = run(&["--from", "repo"]);
    ok(&o);
    // already up to date, so nothing changes — but the run must succeed and
    // not complain about a missing repository
    assert!(stdout(&o).contains("up to date"), "{}", stdout(&o));
    let _ = fs::remove_dir_all(&d);
}

#[test]
fn config_show_lists_repo_install_mode_and_system_file() {
    let d = scratch("show");
    let sys = d.join("sysconf");
    fs::create_dir_all(&sys).unwrap();
    fs::write(sys.join("nawa.toml"), "install_mode = \"ask\"\n").unwrap();
    let o = Command::new(nawa_bin())
        .args(["config", "show"])
        .env_remove("NAWA_CONFIG")
        .env_remove("NAWA_REPO")
        .env_remove("NAWA_INSTALL_MODE")
        .env("NAWA_SYSCONFDIR", &sys)
        .env("NO_COLOR", "1")
        .current_dir(&d)
        .output()
        .unwrap();
    let out = ok(&o);
    // built-in default repository, install_mode from the system file
    assert!(out.contains("mrdevmohamed/faseela-repo/releases/download/stable"), "{out}");
    let line = out.lines().find(|l| l.starts_with("install_mode")).expect("install_mode line");
    assert!(line.contains("ask") && line.contains("nawa.toml"), "{line}");
    assert!(out.contains("system"), "{out}");
    let _ = fs::remove_dir_all(&d);
}
