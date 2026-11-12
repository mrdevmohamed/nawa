//! `nawa install <name>` / `outdated` / `update` / `autoremove` / `list -e|-d`
//! against meta recipes (no downloads) and a throwaway root.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn nawa_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_nawa"))
}

fn recipe(dir: &Path, name: &str, version: &str, build: &[&str], run: &[&str]) {
    let q = |l: &[&str]| l.iter().map(|d| format!("{d:?}")).collect::<Vec<_>>().join(", ");
    let r = dir.join("recipes").join(name);
    fs::create_dir_all(&r).unwrap();
    fs::write(
        r.join("recipe.toml"),
        format!(
            "[package]\nname = \"{name}\"\nversion = \"{version}\"\nrel = 1\narch = [\"x86_64\"]\n\
             [depends]\nbuild = [{}]\nrun = [{}]\n\
             [build]\ninstall = \"mkdir -p %{{pkgdir}}/usr/share && echo {name}-{version} > %{{pkgdir}}/usr/share/{name}\"\n",
            q(build),
            q(run)
        ),
    )
    .unwrap();
}

fn nawa(dir: &Path, args: &[&str]) -> Output {
    Command::new(nawa_bin())
        .env_remove("NAWA_CONFIG")
        .env_remove("NAWA_REPO")
        // never read the host's /etc/nawa/nawa.toml; these tests cover the
        // build path (the repository path lives in tests/repo_install.rs)
        .env("NAWA_SYSCONFDIR", std::env::temp_dir().join("nawa-test-no-sysconf"))
        .env("NAWA_INSTALL_MODE", "build")
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

fn ok(o: &Output) -> String {
    assert!(
        o.status.success(),
        "failed:\n{}\n{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    );
    String::from_utf8_lossy(&o.stdout).to_string()
}

fn scratch(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("nawa-byname-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(d.join("root")).unwrap();
    d
}

#[test]
fn install_by_name_builds_dependencies_first() {
    let d = scratch("deps");
    recipe(&d, "libc0", "1", &[], &[]);
    recipe(&d, "libfoo", "1", &["libc0"], &["libc0"]);
    recipe(&d, "app", "1", &["libfoo"], &["libfoo"]);

    // dry run: plan only, nothing built or installed
    let out = ok(&nawa(&d, &["install", "--dry-run", "app"]));
    let (a, b, c) = (
        out.find("libc0").unwrap(),
        out.find("libfoo").unwrap(),
        out.find("app").unwrap(),
    );
    assert!(a < b && b < c, "dependencies must come first:\n{out}");
    assert!(out.contains("dry run"));
    assert!(!d.join("root/usr/share/app").exists());

    // non-interactive without --yes is refused, like remove/upgrade
    let o = nawa(&d, &["install", "app"]);
    assert!(!o.status.success());
    assert!(String::from_utf8_lossy(&o.stderr).contains("--yes"));
    assert!(!d.join("root/usr/share/app").exists());

    // the real thing
    let out = ok(&nawa(&d, &["install", "--yes", "app"]));
    assert!(out.contains("[3/3] installing app-1-1"), "{out}");
    for f in ["libc0", "libfoo", "app"] {
        assert!(d.join("root/usr/share").join(f).exists(), "{f} not installed");
    }

    // reasons: the named package is explicit, the rest are dependencies
    let explicit = ok(&nawa(&d, &["list", "--explicit"]));
    assert!(explicit.contains("app") && !explicit.contains("libfoo"), "{explicit}");
    let deps = ok(&nawa(&d, &["list", "--deps"]));
    assert!(deps.contains("libfoo") && deps.contains("libc0") && !deps.contains("app"), "{deps}");

    // asking again is a no-op, not a rebuild
    let out = ok(&nawa(&d, &["install", "--yes", "app"]));
    assert!(out.contains("already installed"), "{out}");

    // typo -> suggestion, nothing touched
    let o = nawa(&d, &["install", "--yes", "ap"]);
    assert!(!o.status.success());
    assert!(String::from_utf8_lossy(&o.stderr).contains("app"));
    let _ = fs::remove_dir_all(&d);
}

#[test]
fn install_recovers_a_crashed_transaction_before_opening_its_own() {
    let d = scratch("recover-before-op");
    recipe(&d, "ghost", "1", &[], &[]);
    recipe(&d, "other", "1", &[], &[]);

    // Simulate a process killed after the package db commit but before the
    // StepResult: the db entry exists, the TxWAL still has a pending intent,
    // and the transaction record is still "preparing".
    let history = d.join("root/var/lib/nawa/history");
    let ghost = d.join("root/var/lib/nawa/local/ghost");
    fs::create_dir_all(&ghost).unwrap();
    fs::write(
        ghost.join("desc"),
        "name = \"ghost\"\nversion = \"1\"\nrel = 1\narch = \"x86_64\"\n\
         reason = \"explicit\"\nrun = []\ninstall_date = 1\nsize = 1\n",
    )
    .unwrap();
    fs::create_dir_all(history.join("transactions")).unwrap();
    fs::write(
        history.join("transactions/0000000001.toml"),
        "schema = 1\nid = 1\ncommand = \"install\"\nstatus = \"preparing\"\nstarted_at = 1\n",
    )
    .unwrap();
    let journal = history.join("journal/0000000001");
    fs::create_dir_all(journal.join("steps")).unwrap();
    fs::write(
        journal.join("state.toml"),
        "schema = 1\ntx = 1\ncommand = \"install\"\nstarted_at = 1\n",
    )
    .unwrap();
    fs::write(
        journal.join("steps/intent-0001.toml"),
        "schema = 1\ntx = 1\nstep = 1\npackage = \"ghost\"\naction = \"install\"\n\n\
         [target]\nversion = \"1\"\nrel = 1\narch = \"x86_64\"\nreason = \"explicit\"\n",
    )
    .unwrap();

    // An unrelated per-step install must reconcile that transaction before
    // opening its own. `history` must not be the first command to see it.
    let out = nawa(&d, &["install", "--yes", "other"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "install failed:\n{stderr}");
    assert!(
        stderr.contains("recovered: transaction 1 was interrupted"),
        "install did not report the recovery:\n{stderr}"
    );
    let record = fs::read_to_string(history.join("transactions/0000000001.toml")).unwrap();
    assert!(record.contains("status = \"partial\""), "{record}");
    assert!(record.contains("name = \"ghost\""), "{record}");
    assert!(record.contains("status = \"committed\""), "{record}");
    assert!(!record.contains("preparing"), "{record}");

    let out = nawa(&d, &["history", "info", "1"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "history failed:\n{stderr}");
    assert!(!stderr.contains("recovered:"), "history had to reconcile again:\n{stderr}");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("Status:    partial"), "{text}");
    assert!(text.contains("ghost: install (committed)"), "{text}");
    assert!(!journal.exists(), "the reconciled journal must be removed");
    let _ = fs::remove_dir_all(&d);
}

#[test]
fn outdated_update_and_autoremove() {
    let d = scratch("update");
    recipe(&d, "libfoo", "1", &[], &[]);
    recipe(&d, "app", "1", &[], &["libfoo"]);
    recipe(&d, "gcc0", "1", &[], &[]);
    recipe(&d, "tool", "1", &["gcc0"], &[]);
    ok(&nawa(&d, &["install", "--yes", "app", "tool"]));

    assert!(ok(&nawa(&d, &["outdated"])).contains("up to date"));

    // a new recipe version appears
    recipe(&d, "libfoo", "2", &[], &[]);
    let out = ok(&nawa(&d, &["outdated"]));
    assert!(out.contains("libfoo") && out.contains("1-1") && out.contains("2-1"), "{out}");

    // update only touches what is outdated and keeps the install reason
    let out = ok(&nawa(&d, &["update", "--yes"]));
    assert!(out.contains("1 package(s)") && out.contains("libfoo-2-1"), "{out}");
    assert_eq!(fs::read_to_string(d.join("root/usr/share/libfoo")).unwrap().trim(), "libfoo-2");
    assert!(ok(&nawa(&d, &["list", "--deps"])).contains("libfoo"), "reason must survive an update");
    assert!(ok(&nawa(&d, &["update", "--yes"])).contains("up to date"));

    // nothing is orphaned while `app` needs libfoo and `tool` needs gcc0
    assert!(ok(&nawa(&d, &["autoremove", "--dry-run"])).contains("nothing to remove"));

    // drop the packages that needed them: both become unneeded...
    ok(&nawa(&d, &["remove", "--yes", "app"]));
    ok(&nawa(&d, &["remove", "--yes", "tool"]));
    let out = ok(&nawa(&d, &["autoremove", "--dry-run"]));
    assert!(out.contains("libfoo"), "{out}");
    // ...but a build-only dependency of nothing installed is also unneeded
    // now, and --dry-run must not have removed anything
    assert!(d.join("root/usr/share/libfoo").exists());
    ok(&nawa(&d, &["autoremove", "--yes"]));
    assert!(!d.join("root/usr/share/libfoo").exists());
    assert!(!d.join("root/usr/share/gcc0").exists());
    let _ = fs::remove_dir_all(&d);
}

#[test]
fn autoremove_keeps_build_tools_of_installed_packages() {
    let d = scratch("keepbuild");
    recipe(&d, "gcc0", "1", &[], &[]);
    recipe(&d, "tool", "1", &["gcc0"], &[]);
    ok(&nawa(&d, &["install", "--yes", "tool"]));
    assert!(ok(&nawa(&d, &["autoremove", "--dry-run"])).contains("nothing to remove"));
    let out = ok(&nawa(&d, &["autoremove", "--dry-run", "--build-deps"]));
    assert!(out.contains("gcc0"), "{out}");
    let _ = fs::remove_dir_all(&d);
}

#[test]
fn mixing_names_and_archives_is_rejected() {
    let d = scratch("mix");
    recipe(&d, "a", "1", &[], &[]);
    let o = nawa(&d, &["install", "--yes", "a", "x/y.nawa.tar.zst"]);
    assert!(!o.status.success());
    assert!(String::from_utf8_lossy(&o.stderr).contains("mix"));
    let _ = fs::remove_dir_all(&d);
}
