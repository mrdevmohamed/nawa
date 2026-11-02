//! End-to-end test: recipe -> build -> install -> query -> remove
//! Uses a meta recipe (no downloads) against a temp root.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn nawa_bin() -> PathBuf {
    // cargo builds the binary for integration tests and tells us where it is
    PathBuf::from(env!("CARGO_BIN_EXE_nawa"))
}

/// A nawa invocation isolated from the ambient configuration: no NAWA_* env
/// and a cwd outside the repository, so the repo's own `nawa.toml` (and the
/// profile it points at) is never discovered.
fn nawa() -> Command {
    let mut c = Command::new(nawa_bin());
    for k in [
        "NAWA_CONFIG", "NAWA_ROOT", "NAWA_RECIPES", "NAWA_BUILDDIR",
        "NAWA_PROFILES", "NAWA_JOBS", "NAWA_REPO", "NAWA_INSTALL_MODE",
    ] {
        c.env_remove(k);
    }
    // never read the host's /etc/nawa/nawa.toml, and keep installs by name
    // deterministic (no network): these tests exercise the build path
    c.env("NAWA_SYSCONFDIR", std::env::temp_dir().join("nawa-test-no-sysconf"));
    c.env("NAWA_INSTALL_MODE", "build");
    c.current_dir(std::env::temp_dir());
    c
}

#[test]
fn build_install_query_remove_roundtrip() {
    let scratch = std::env::temp_dir().join(format!("nawa-e2e-{}", std::process::id()));
    let _ = fs::remove_dir_all(&scratch);

    let recipes = scratch.join("recipes/hello");
    fs::create_dir_all(&recipes).unwrap();
    fs::write(
        recipes.join("recipe.toml"),
        r#"
[package]
name = "hello"
version = "1.0"
rel = 1
arch = ["x86_64"]
summary = "hello meta package"

[build]
install = """
mkdir -p %{pkgdir}/usr/bin
printf '#!/bin/sh\necho hello-nawa\n' > %{pkgdir}/usr/bin/hello
chmod +x %{pkgdir}/usr/bin/hello
"""
"#,
    )
    .unwrap();

    let root = scratch.join("rootfs");
    fs::create_dir_all(&root).unwrap();

    // build
    let out = nawa()
        .args([
            "--recipes", recipes.parent().unwrap().to_str().unwrap(),
            "--builddir", scratch.join("build").to_str().unwrap(),
            "build", "hello",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "build failed:\n{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let archive = scratch.join("build/out/hello-1.0-1.nawa.tar.zst");
    assert!(archive.exists(), "archive missing: {}", archive.display());

    // install
    let out = nawa()
        .args([
            "--root", root.to_str().unwrap(),
            "install", archive.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "install failed:\n{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(root.join("usr/bin/hello").exists());

    // query
    let out = nawa()
        .args(["--root", root.to_str().unwrap(), "query"])
        .output()
        .unwrap();
    let q = String::from_utf8_lossy(&out.stdout);
    assert!(q.contains("hello"), "query output missing hello: {q}");

    // owner
    let out = nawa()
        .args([
            "--root", root.to_str().unwrap(),
            "owner", "/usr/bin/hello",
        ])
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).contains("hello"));

    // remove (confirmation is required for non-interactive runs: --yes)
    let out = nawa()
        .args(["--root", root.to_str().unwrap(), "remove", "--yes", "hello"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "remove failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!root.join("usr/bin/hello").exists());

    let _ = fs::remove_dir_all(&scratch);
}

/// Non-interactive `remove` without `--yes` is refused (never guessed).
#[test]
fn remove_refuses_non_interactive_without_yes() {
    let scratch = std::env::temp_dir().join(format!("nawa-e2e-confirm-{}", std::process::id()));
    let _ = fs::remove_dir_all(&scratch);
    let archive = build_hello_meta(&scratch);
    let root = scratch.join("rootfs");
    fs::create_dir_all(&root).unwrap();
    run(["--root", root.to_str().unwrap(), "install", archive.to_str().unwrap()]);

    // the refusal names --yes and nothing was removed
    let out = run_raw(["--root", root.to_str().unwrap(), "remove", "hello"]);
    assert!(!out.status.success(), "must refuse without --yes");
    assert!(String::from_utf8_lossy(&out.stderr).contains("--yes"));
    assert!(root.join("usr/bin/hello").exists(), "nothing may be removed");

    // --yes performs the removal
    run(["--root", root.to_str().unwrap(), "remove", "--yes", "hello"]);
    assert!(!root.join("usr/bin/hello").exists());
    let _ = fs::remove_dir_all(&scratch);
}

/// The v0.7 UX surface: global flags after the subcommand, search, info,
/// why, list-with-pattern, and build --install in one step.
#[test]
fn ux_search_info_why_and_flags_anywhere() {
    let scratch = std::env::temp_dir().join(format!("nawa-e2e-ux-{}", std::process::id()));
    let _ = fs::remove_dir_all(&scratch);
    build_hello_meta(&scratch);
    let root = scratch.join("rootfs");
    fs::create_dir_all(&root).unwrap();
    let recipes = scratch.join("recipes");
    let r = recipes.to_str().unwrap().to_string();
    let b = scratch.join("build").to_str().unwrap().to_string();
    let rt = root.to_str().unwrap().to_string();

    // global flag AFTER the subcommand (new in v0.7)
    let out = run_raw(["search", "--recipes", &r, "hello"]);
    assert!(out.status.success(), "search failed: {}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stdout).contains("hello meta package"));

    // search --glob matches the name only
    let out = run_raw(["search", "--recipes", &r, "--glob", "hel*"]);
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("hello"));

    // info before install: "not installed"
    let out = run_raw(["info", "--recipes", &r, "hello"]);
    assert!(out.status.success(), "info failed: {}", String::from_utf8_lossy(&out.stderr));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("not installed"));
    assert!(text.contains("hello meta package"));
    assert!(text.contains("recipe.toml"));

    // why on something unknown errors and suggests nothing here (no deps)
    let out = run_raw(["why", "--recipes", &r, "hello"]);
    assert!(String::from_utf8_lossy(&out.stdout).contains("not installed"));

    // build --install: one step instead of three
    let out = run_raw(["--recipes", &r, "--builddir", &b, "build", "--install", "hello", "--root", &rt]);
    assert!(
        out.status.success(),
        "build --install failed:\n{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(root.join("usr/bin/hello").exists(), "archive was not installed by build --install");

    // list (query alias) with a pattern filter
    let out = run_raw(["list", "--root", &rt, "hel"]);
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("hello"));
    // pattern that matches nothing fails loudly
    let out = run_raw(["list", "--root", &rt, "zzz-nope"]);
    assert!(!out.status.success());

    // __complete_pkgs lists the recipe name (used by shell completions)
    let out = run_raw(["__complete_pkgs", "--recipes", &r]);
    assert!(String::from_utf8_lossy(&out.stdout).contains("hello"));

    // completions scripts render
    for shell in ["bash", "zsh", "fish"] {
        let out = run_raw(["completions", shell]);
        assert!(out.status.success(), "completions {shell} failed");
        assert!(!out.stdout.is_empty(), "{shell} completion script is empty");
    }

    let _ = fs::remove_dir_all(&scratch);
}

// ---- shared helpers --------------------------------------------------------

/// A tiny meta recipe (no downloads): builds+installs /usr/bin/hello.
/// Returns the built archive path.
fn build_hello_meta(scratch: &std::path::Path) -> std::path::PathBuf {
    let recipes = scratch.join("recipes/hello");
    fs::create_dir_all(&recipes).unwrap();
    fs::write(
        recipes.join("recipe.toml"),
        r#"
[package]
name = "hello"
version = "1.0"
rel = 1
arch = ["x86_64"]
summary = "hello meta package"

[build]
install = """
mkdir -p %{pkgdir}/usr/bin
printf '#!/bin/sh\necho hello-nawa\n' > %{pkgdir}/usr/bin/hello
chmod +x %{pkgdir}/usr/bin/hello
"""
"#,
    )
    .unwrap();

    run([
        "--recipes", scratch.join("recipes").to_str().unwrap(),
        "--builddir", scratch.join("build").to_str().unwrap(),
        "build", "hello",
    ]);
    let archive = scratch.join("build/out/hello-1.0-1.nawa.tar.zst");
    assert!(archive.exists(), "archive missing: {}", archive.display());
    archive
}

/// Run nawa, panic with stdout+stderr when it fails.
fn run<'a>(args: impl IntoIterator<Item = &'a str>) -> std::process::Output {
    let out = run_raw(args);
    assert!(
        out.status.success(),
        "nawa failed:\n{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

fn run_raw<'a>(args: impl IntoIterator<Item = &'a str>) -> std::process::Output {
    nawa()
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn deps_all_skips_installed_unless_include_installed() {
    // regression: `deps-all` feeds the installed set into the resolver, which
    // treats "installed" as "done" — with everything installed the order came
    // back empty (stage3 STAGE3_WORLD=1 silently rebuilt nothing). The
    // --include-installed flag ignores the db and lists every recipe.
    let scratch = std::env::temp_dir().join(format!("nawa-e2e-depsall-{}", std::process::id()));
    let _ = fs::remove_dir_all(&scratch);

    let recipes = scratch.join("recipes");
    for (name, deps) in [("hello", ""), ("zzz", "")] {
        let dir = recipes.join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("recipe.toml"),
            format!(
                "[package]\nname = \"{name}\"\nversion = \"1.0\"\n\n[depends]\nbuild = [{deps}]\n\n[build]\ninstall = \"\"\"\nmkdir -p %{{pkgdir}}/usr/bin\nprintf 'x' > %{{pkgdir}}/usr/bin/{name}\n\"\"\"\n"
            ),
        )
        .unwrap();
    }

    let root = scratch.join("rootfs");
    fs::create_dir_all(&root).unwrap();

    // build + install "hello" so the db records it
    let out = nawa()
        .args([
            "--recipes", recipes.to_str().unwrap(),
            "--builddir", scratch.join("build").to_str().unwrap(),
            "build", "hello",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "build failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let archive = scratch.join("build/out/hello-1.0-1.nawa.tar.zst");
    let out = nawa()
        .args(["--root", root.to_str().unwrap(), "install", archive.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(out.status.success());

    let deps_all = |extra: &str| {
        let out = nawa()
            .args(["--root", root.to_str().unwrap(), "--recipes", recipes.to_str().unwrap()])
            .args(["deps-all"])
            .args(if extra.is_empty() { vec![] } else { vec![extra] })
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };

    // default: the installed "hello" is skipped
    assert_eq!(deps_all(""), "zzz", "installed packages must be skipped by default");
    // --include-installed: every recipe is listed
    assert_eq!(deps_all("--include-installed"), "hello\nzzz");
    let _ = fs::remove_dir_all(&scratch);
}

/// Regression: `build --install` must rebuild even when an archive with the
/// same name-version-rel is already in out/ (recipe edited, version not bumped).
#[test]
fn build_install_rebuilds_instead_of_reusing_a_stale_archive() {
    let scratch = std::env::temp_dir().join(format!("nawa-e2e-stale-{}", std::process::id()));
    let _ = fs::remove_dir_all(&scratch);
    let recipes = scratch.join("recipes/demo");
    fs::create_dir_all(&recipes).unwrap();
    let root = scratch.join("rootfs");
    fs::create_dir_all(&root).unwrap();
    let write_recipe = |marker: &str| {
        fs::write(
            recipes.join("recipe.toml"),
            format!(
                "[package]\nname = \"demo\"\nversion = \"1.0\"\nrel = 1\narch = [\"x86_64\"]\n\
                 summary = \"demo\"\n\n[build]\ninstall = \"\"\"\n\
                 mkdir -p %{{pkgdir}}/usr/bin\necho {marker} > %{{pkgdir}}/usr/bin/demo\n\"\"\"\n"
            ),
        )
        .unwrap();
    };
    let go = || {
        let o = nawa()
            .args([
                "build", "--install", "demo", "--force",
                "--recipes", scratch.join("recipes").to_str().unwrap(),
                "--builddir", scratch.join("build").to_str().unwrap(),
                "--root", root.to_str().unwrap(),
            ])
            .output()
            .unwrap();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    };
    write_recipe("first");
    go();
    assert_eq!(fs::read_to_string(root.join("usr/bin/demo")).unwrap().trim(), "first");
    write_recipe("second");
    go();
    assert_eq!(
        fs::read_to_string(root.join("usr/bin/demo")).unwrap().trim(),
        "second",
        "build --install installed a stale archive"
    );
    let _ = fs::remove_dir_all(&scratch);
}

/// Regression: the near-miss hint for a missing archive must work both for a
/// path with a directory and for a bare file name in the CWD.
#[test]
fn install_missing_archive_suggests_the_near_miss() {
    let scratch = std::env::temp_dir().join(format!("nawa-e2e-hint-{}", std::process::id()));
    let _ = fs::remove_dir_all(&scratch);
    fs::create_dir_all(scratch.join("out")).unwrap();
    fs::write(scratch.join("out/demo-1.0-1.nawa.tar.zst"), b"x").unwrap();
    let root = scratch.join("rootfs");
    let o = nawa()
        .args(["install", "--root", root.to_str().unwrap(), "out/demo-1.0-2.nawa.tar.zst"])
        .current_dir(&scratch)
        .output()
        .unwrap();
    assert!(!o.status.success());
    assert!(String::from_utf8_lossy(&o.stderr).contains("did you mean `demo-1.0-1.nawa.tar.zst`"));
    let o = nawa()
        .args(["install", "--root", root.to_str().unwrap(), "demo-1.0-2.nawa.tar.zst"])
        .current_dir(scratch.join("out"))
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&o.stderr).contains("did you mean `demo-1.0-1.nawa.tar.zst`"));
    let _ = fs::remove_dir_all(&scratch);
}

/// Regression: `config show` must attribute a value passed as a flag to "flag".
#[test]
fn config_show_reports_flag_as_the_source() {
    let o = nawa()
        .args(["config", "show", "--root", "/xyz"])
        .env_remove("NAWA_ROOT")
        .env_remove("NAWA_CONFIG")
        .output()
        .unwrap();
    let out = String::from_utf8_lossy(&o.stdout);
    let line = out.lines().find(|l| l.contains("/xyz")).expect("root line");
    assert!(line.contains("(flag)"), "got: {line}");
}


/// Repository-backed update must use the binary archive, and strict repo
/// mode must refuse a stale repository version without changing the system.
#[test]
fn update_from_repo_uses_binary_archive_and_rejects_stale_repo_versions() {
    let scratch = std::env::temp_dir().join(format!("nawa-e2e-update-repo-{}", std::process::id()));
    let _ = fs::remove_dir_all(&scratch);

    let recipes = scratch.join("recipes");
    let recipe_dir = recipes.join("demo");
    let builddir = scratch.join("build");
    let root = scratch.join("rootfs");
    fs::create_dir_all(&recipe_dir).unwrap();
    fs::create_dir_all(&root).unwrap();

    let write_recipe = |version: &str, marker: &str| {
        fs::write(
            recipe_dir.join("recipe.toml"),
            format!(
                "[package]\nname = \"demo\"\nversion = \"{version}\"\nrel = 1\n\
                 arch = [\"x86_64\"]\nsummary = \"demo update test\"\n\n\
                 [build]\ninstall = \"\"\"\n\
                 mkdir -p %{{pkgdir}}/usr/share\necho {marker} > %{{pkgdir}}/usr/share/demo-version\n\
                 \"\"\"\n"
            ),
        )
        .unwrap();
    };

    let build_demo = || {
        let out = nawa()
            .args([
                "--recipes", recipes.to_str().unwrap(),
                "--builddir", builddir.to_str().unwrap(),
                "build", "demo",
            ])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "build failed:\n{}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    };

    // Install 1.0 first, then make the local recipe request 2.0.
    write_recipe("1.0", "v1");
    build_demo();
    let v1_archive = builddir.join("out/demo-1.0-1.nawa.tar.zst");
    assert!(v1_archive.is_file());
    let installed = nawa()
        .args([
            "--root", root.to_str().unwrap(),
            "install", v1_archive.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        installed.status.success(),
        "initial install failed:\n{}",
        String::from_utf8_lossy(&installed.stderr)
    );

    write_recipe("2.0", "v2");
    build_demo();
    let v2_archive = builddir.join("out/demo-2.0-1.nawa.tar.zst");
    assert!(v2_archive.is_file());

    // A stale repo must be rejected before any package is touched.
    let stale_repo = scratch.join("stale-repo");
    fs::create_dir_all(&stale_repo).unwrap();
    fs::copy(&v1_archive, stale_repo.join(v1_archive.file_name().unwrap())).unwrap();
    let indexed = nawa()
        .args(["repo-create", stale_repo.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(indexed.status.success(), "repo-create failed: {}", String::from_utf8_lossy(&indexed.stderr));

    let stale = nawa()
        .args([
            "--root", root.to_str().unwrap(),
            "--recipes", recipes.to_str().unwrap(),
            "update", "--from", "repo",
            "--repo", stale_repo.to_str().unwrap(),
            "--yes",
        ])
        .output()
        .unwrap();
    assert!(!stale.status.success(), "strict repo mode must reject the stale package");
    assert!(
        String::from_utf8_lossy(&stale.stderr).contains("recipe requires"),
        "unexpected error: {}",
        String::from_utf8_lossy(&stale.stderr)
    );
    assert_eq!(
        fs::read_to_string(root.join("usr/share/demo-version")).unwrap().trim(),
        "v1",
        "a rejected plan must leave the installed package untouched"
    );

    // A repository with the requested version must supply the update; no
    // source build is needed for this invocation.
    let current_repo = scratch.join("current-repo");
    fs::create_dir_all(&current_repo).unwrap();
    fs::copy(&v2_archive, current_repo.join(v2_archive.file_name().unwrap())).unwrap();
    let indexed = nawa()
        .args(["repo-create", current_repo.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(indexed.status.success(), "repo-create failed: {}", String::from_utf8_lossy(&indexed.stderr));

    let updated = nawa()
        .args([
            "--root", root.to_str().unwrap(),
            "--recipes", recipes.to_str().unwrap(),
            "update", "--from", "repo",
            "--repo", current_repo.to_str().unwrap(),
            "--yes",
        ])
        .output()
        .unwrap();
    assert!(
        updated.status.success(),
        "repo update failed:\n{}\n{}",
        String::from_utf8_lossy(&updated.stdout),
        String::from_utf8_lossy(&updated.stderr)
    );
    let stdout = String::from_utf8_lossy(&updated.stdout);
    assert!(stdout.contains("fetching demo-2.0-1 from the repository"), "{stdout}");
    assert!(!stdout.contains("building demo"), "repo mode unexpectedly built from source: {stdout}");
    assert_eq!(
        fs::read_to_string(root.join("usr/share/demo-version")).unwrap().trim(),
        "v2"
    );

    let _ = fs::remove_dir_all(&scratch);
}
