//! Binary upgrades from a repository: compare the local database against
//! `index.toml`, verify sha256 + signature, then install newer packages in
//! dependency order. Every replaced version is recorded in the undo log, so
//! each step is reversible with `nawa rollback`.
//!
//! The repository can be a local directory or an http(s) URL. Remote repos
//! are materialized into a per-URL cache (`<root>/var/cache/nawa/remote/`):
//! the index is re-fetched on every run (falling back to the cached copy when
//! the mirror is unreachable) and archives are downloaded on demand, cached,
//! and sha256-verified before install.

use anyhow::Result;
#[cfg(test)]
use std::collections::BTreeMap;
use std::collections::{HashMap, HashSet};
use std::path::Path;

use crate::db::Db;
use crate::fetch::RepoRef;
use crate::install;
use crate::recipe;
use crate::repo;
#[cfg(test)]
use crate::sign;
use crate::version::vercmp;

mod source;

pub(crate) use source::{archive_download_url, fetch_entry, open_repo, ArchiveAuth, OpenRepo};

pub(crate) struct UpgradeOpts {
    pub dry_run: bool,
    pub force: bool,
    pub allow_unsigned: bool,
    /// skip the interactive confirmation (non-interactive callers must
    /// pass it — a piped stdin otherwise refuses, never silently proceeds)
    pub yes: bool,
}

pub(crate) fn upgrade(
    root: &Path,
    repo_ref: &RepoRef,
    recipes: &Path,
    opts: &UpgradeOpts,
) -> Result<()> {
    let open = open_repo(root, repo_ref, opts.allow_unsigned, opts.force)?;
    let Some(plan) = build_plan(root, &open.index, recipes)? else {
        return Ok(());
    };
    print_plan(&plan);

    if opts.dry_run {
        crate::nawa_info!("dry run — no changes made");
        return Ok(());
    }

    // the plan is on the table; a real run needs an explicit go-ahead
    // (scripts pass --yes; a piped stdin without it is refused, not guessed)
    crate::confirm::confirm("nawa: proceed with the upgrade?", opts.yes)?;
    apply_plan(root, &open, &plan, opts)
}

struct PlannedUpgrade<'a> {
    entry: &'a repo::RepoEntry,
    new_version: String,
}

struct UpgradePlan<'a> {
    installed: HashMap<String, crate::db::Entry>,
    upgrades: Vec<PlannedUpgrade<'a>>,
}

/// Compare the local database against the repository index and order the
/// newer packages for installation. Returns `None` when everything is current.
fn build_plan<'a>(
    root: &Path,
    index: &'a repo::Index,
    recipes: &Path,
) -> Result<Option<UpgradePlan<'a>>> {
    let db = Db::new(root);
    let installed_map: HashMap<String, crate::db::Entry> =
        db.installed()?.into_iter().map(|e| (e.name.clone(), e)).collect();
    let mut candidates: Vec<(&repo::RepoEntry, String)> = Vec::new();
    let mut entries: Vec<&crate::db::Entry> = installed_map.values().collect();
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    for e in entries {
        let re = match index.find(&e.name) {
            Some(r) => r,
            None => continue, // not in this repo -> untouched
        };
        let new = format!("{}-{}-{}", re.name, re.version, re.rel);
        if vercmp(&format!("{}-{}", re.version, re.rel), &format!("{}-{}", e.version, e.rel))
            == std::cmp::Ordering::Greater
        {
            candidates.push((re, new));
        }
    }

    if candidates.is_empty() {
        crate::nawa_info!("everything up to date ({} installed)", installed_map.len());
        return Ok(None);
    }

    // dependency order when the recipe tree is available; packages that have
    // no recipe (third-party repo content) are appended alphabetically so a
    // partial tree never aborts the upgrade
    let names: Vec<String> = candidates.iter().map(|(r, _)| r.name.clone()).collect();
    let ordered: Vec<String> = match recipe::load_all(recipes) {
        Ok(tree) if !tree.is_empty() => {
            let installed: HashSet<String> = installed_map.keys().cloned().collect();
            order_with_fallback(&names, &installed, &tree)
        }
        _ => {
            crate::nawa_note_stderr!("no recipe tree at {} — applying in alphabetical order", recipes.display());
            let mut n = names.clone();
            n.sort();
            n
        }
    };

    let upgrades = ordered
        .iter()
        .map(|name| {
            let (entry, new_version) = candidates
                .iter()
                .find(|(entry, _)| &entry.name == name)
                .expect("ordered name came from the plan");
            PlannedUpgrade { entry, new_version: new_version.clone() }
        })
        .collect();
    Ok(Some(UpgradePlan { installed: installed_map, upgrades }))
}

fn print_plan(plan: &UpgradePlan<'_>) {
    let pal = crate::color::Palette::auto(crate::color::Stream::Stdout);
    crate::nawa_info!("{} package(s) to upgrade:", plan.upgrades.len());
    for upgrade in &plan.upgrades {
        let name = &upgrade.entry.name;
        let cur = plan.installed.get(name).map(|e| e.version_full()).unwrap_or_default();
        println!(
            "  {} {} {} {}",
            pal.name(&format!("{name:<24}")),
            pal.ver(&cur),
            pal.dim("->"),
            pal.ok(&upgrade.new_version),
        );
    }
}

fn apply_plan(
    root: &Path,
    open: &OpenRepo,
    plan: &UpgradePlan<'_>,
    opts: &UpgradeOpts,
) -> Result<()> {
    let ordered: Vec<String> = plan.upgrades.iter().map(|u| u.entry.name.clone()).collect();
    let repo_ids: Vec<String> = open.base_url.clone().into_iter().collect();
    let mut op = crate::transaction::begin_operation(
        root,
        crate::transaction::OperationCommand::Upgrade,
        &ordered,
        &repo_ids,
    )?;
    let db_here = Db::new(root);
    let mut done = 0usize;
    let mut first_err: Option<anyhow::Error> = None;
    for upgrade in &plan.upgrades {
        let name = &upgrade.entry.name;
        let re = upgrade.entry;
        // a Ctrl+C that arrived during the previous package was deferred
        // until it was complete; stop before starting the next one
        if let Err(error) = crate::interrupt::check() {
            first_err = Some(error.into());
            break;
        }
        let before = db_here.entry(name)?;
        let target = crate::transaction::history::PackageState {
            version: Some(re.version.clone()), rel: Some(re.rel), arch: re.arch.clone(),
            reason: before.as_ref().map(|entry| entry.reason.clone()).unwrap_or_else(|| "explicit".to_string()),
            artifact_sha256: re.sha256.clone(),
        };
        let mut recorder = op.recorder();
        let step_no = recorder.step_intent_state(
            name, crate::transaction::history::StepAction::Upgrade,
            before.as_ref(), Some(&target), &re.sha256,
        )?;
        let outcome = fetch_entry(root, open, re, opts.allow_unsigned).and_then(|src| {
            if open.archive_auth == ArchiveAuth::SignedIndexSha256 {
                install::install_archive_from_index_proof(
                    &src,
                    root,
                    opts.force,
                    &re.sha256,
                    crate::db::InstallReason::Keep,
                    Some(install::IndexProof {
                        index_bytes: Some(&open.index_bytes),
                        index_signature: open.index_signature.as_deref(),
                        signer_public: open.signer_public.as_deref(),
                        repo_id: None,
                    }),
                )
            } else {
                install::install_archive_pinned(
                    &src,
                    root,
                    install::InstallOpts { force: opts.force, allow_unsigned: opts.allow_unsigned, key: None, reason: crate::db::InstallReason::Keep, pinned_sha: Some(&re.sha256) },
                )
            }
        });
        install::record_install_outcome(
            &mut recorder,
            root,
            step_no,
            name,
            crate::transaction::history::StepAction::Upgrade,
            before.as_ref(),
            &target,
            "repository",
            &outcome,
        )?;
        match &outcome {
            Ok(_) => done += 1,
            Err(error) => {
                first_err = Some(anyhow::anyhow!("{error:#}"));
                break;
            }
        }
    }
    op.finish(first_err.as_ref())?;
    if let Some(e) = first_err {
        return Err(e.context(format!("upgraded {done} of {} package(s) before the failure", ordered.len())));
    }
    crate::nawa_info!("upgraded {done} package(s)");
    Ok(())
}

pub mod verify;
pub mod order;

pub(crate) use order::order_with_fallback;
#[cfg(test)]
pub(crate) use verify::{check_archive_matches_index, check_index_freshness, require_trust_for_remote, validate_entry_filename};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive;
    use std::path::PathBuf;

    use crate::testutil::tmpdir;

    fn make_pkg(dir: &Path, name: &str, version: &str, rel: u32, body: &[u8]) -> PathBuf {
        let staging = dir.join(format!("staging-{name}-{version}-{rel}"));
        std::fs::create_dir_all(&staging).unwrap();
        std::fs::write(staging.join("bin"), body).unwrap();
        let arch = std::env::consts::ARCH;
        let pkginfo = format!(
            "pkgname = {name}\npkgver = {version}\npkgrel = {rel}\narch = {arch}\nrundeps = \nsize = 1\n"
        );
        let out = dir.join(format!("{name}-{version}-{rel}.nawa.tar.zst"));
        archive::create(&staging, &pkginfo, &out).unwrap();
        out
    }

    fn setup_repo(dir: &Path, pkgs: &[PathBuf]) -> PathBuf {
        let repo = dir.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        for p in pkgs {
            std::fs::copy(p, repo.join(p.file_name().unwrap())).unwrap();
        }
        repo::create_index(&repo, &repo.join("index.toml")).unwrap();
        repo
    }

    #[test]
    fn archive_download_url_preserves_v2_and_selects_v3_locations() {
        let mut index = repo::Index {
            generated: Some(10),
            expires: Some(20),
            repository: Some(repo::RepositoryMeta::v2()),
            recipes: None,
            groups: None,
            packages: Vec::new(),
        };
        assert_eq!(
            archive_download_url(
                "https://example.test/stable", &index,
                "bash-5.2-1.nawa.tar.zst", None
            ).unwrap(),
            "https://example.test/stable/bash-5.2-1.nawa.tar.zst"
        );

        index.repository = Some(repo::RepositoryMeta::v3_per_package_url());
        let url = "https://github.com/example/repo/releases/download/packages-0002/sha256-bash.nawa.tar.zst";
        assert_eq!(
            archive_download_url(
                "https://example.test/stable", &index,
                "bash-5.2-1.nawa.tar.zst", Some(url)
            ).unwrap(),
            url
        );
        assert!(archive_download_url(
            "https://example.test/stable", &index,
            "bash-5.2-1.nawa.tar.zst", None
        ).is_err());
    }

    #[test]
    fn distributed_index_requires_trusted_signer_even_with_allow_unsigned() {
        let base = tmpdir("v3-untrusted");
        let root = base.join("root");
        let repo_dir = base.join("repo");
        std::fs::create_dir_all(&repo_dir).unwrap();
        let locations = base.join("locations.toml");
        std::fs::write(&locations, "schema = 1\n").unwrap();
        let index_path = repo_dir.join("index.toml");
        repo::create_index_with_locations(
            &repo_dir, &index_path, None, repo::INDEX_SCHEMA_V3, Some(&locations)
        ).unwrap();

        let (secret, _public) = crate::sign::keygen(&base.join("publisher")).unwrap();
        crate::sign::sign(&index_path, &secret).unwrap();

        let err = match open_repo(&root, &RepoRef::Local(repo_dir), true, false) {
            Ok(_) => panic!("untrusted distributed index must be rejected"),
            Err(err) => err,
        };
        assert!(format!("{err:#}").contains("trusted signing key"), "{err:#}");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn upgrade_installs_newer_version_from_repo() {
        let base = tmpdir("e2e");
        let root = base.join("root");

        // v1.0 installed directly from an archive outside the repo
        let v1 = make_pkg(&base, "demo", "1.0", 1, b"one");
        install::install_archive(&v1, &root, false, true, None, crate::db::InstallReason::Explicit).unwrap();

        // repo contains 1.0-2 (rel bump)
        let v2 = make_pkg(&base, "demo", "1.0", 2, b"two");
        let repo = setup_repo(&base, std::slice::from_ref(&v2));

        // dry run changes nothing
        upgrade(&root, &RepoRef::Local(repo.clone()), &base.join("no-recipes"), &UpgradeOpts {
            dry_run: true, force: false, allow_unsigned: true, yes: true,
        }).unwrap();
        assert_eq!(std::fs::read(root.join("bin")).unwrap(), b"one");

        // real upgrade
        upgrade(&root, &RepoRef::Local(repo.clone()), &base.join("no-recipes"), &UpgradeOpts {
            dry_run: false, force: false, allow_unsigned: true, yes: true,
        }).unwrap();
        assert_eq!(std::fs::read(root.join("bin")).unwrap(), b"two");
        let db = Db::new(&root);
        assert_eq!(db.entry("demo").unwrap().unwrap().version_full(), "demo-1.0-2");

        // second run: up to date
        upgrade(&root, &RepoRef::Local(repo.clone()), &base.join("no-recipes"), &UpgradeOpts {
            dry_run: false, force: false, allow_unsigned: true, yes: true,
        }).unwrap();

        // rollback to the pre-upgrade version
        crate::install::rollback("demo", &root).unwrap();
        assert_eq!(std::fs::read(root.join("bin")).unwrap(), b"one");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn upgrade_rejects_tampered_archive() {
        let base = tmpdir("tamper");
        let root = base.join("root");
        let v1 = make_pkg(&base, "demo", "1.0", 1, b"one");
        install::install_archive(&v1, &root, false, true, None, crate::db::InstallReason::Explicit).unwrap();
        let v2 = make_pkg(&base, "demo", "2.0", 1, b"two");
        let repo = setup_repo(&base, &[v2]);

        // corrupt the archive AFTER the index recorded its sha256
        let target = repo.join("demo-2.0-1.nawa.tar.zst");
        std::fs::write(&target, b"corrupted").unwrap();

        let err = upgrade(&root, &RepoRef::Local(repo.clone()), &base.join("no-recipes"), &UpgradeOpts {
            dry_run: false, force: false, allow_unsigned: true, yes: true,
        });
        assert!(err.is_err(), "tampered archive must not install");
        // system still intact at 1.0
        assert_eq!(std::fs::read(root.join("bin")).unwrap(), b"one");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn unsafe_index_paths_are_rejected() {
        for bad in ["../evil.zst", "sub/dir.pkg.zst", ".hidden.zst", "", "a\\b.zst"] {
            assert!(validate_entry_filename(bad).is_err(), "`{bad}` must be rejected");
        }
        assert!(validate_entry_filename("demo-1.0-1.nawa.tar.zst").is_ok());
    }

    #[test]
    fn mixed_plan_orders_known_pkgs_and_appends_unknown() {
        let tree: BTreeMap<String, recipe::Recipe> = ["b", "a"]
            .into_iter()
            .map(|n| {
                let deps = if n == "a" { vec!["b".to_string()] } else { vec![] };
                let toml_src = format!(
                    "[package]\nname = \"{n}\"\nversion = \"1.0\"\n[depends]\nbuild = {:?}\n[build]\ninstall = \"true\"\n",
                    deps
                );
                let r: recipe::Recipe = toml::from_str(&toml_src).unwrap();
                (n.to_string(), r)
            })
            .collect();
        let names = vec!["demo".to_string(), "a".to_string()];
        let ordered = order_with_fallback(&names, &HashSet::new(), &tree);
        // `b` is only a build dependency of the planned `a` — it is not part
        // of the plan, so it must NOT leak into the order (the apply loop has
        // no entry for it). Foreign `demo` goes last.
        assert_eq!(ordered, vec!["a", "demo"]);
    }

    #[test]
    fn upgrade_targets_are_ordered_even_though_they_are_installed() {
        // regression: every upgrade target is by definition installed, and
        // the resolver used to treat "installed" as "done" — the plan order
        // came back EMPTY and `nawa upgrade` applied nothing while reporting
        // success
        let tree: BTreeMap<String, recipe::Recipe> = ["b", "a"]
            .into_iter()
            .map(|n| {
                let deps = if n == "a" { vec!["b".to_string()] } else { vec![] };
                let toml_src = format!(
                    "[package]\nname = \"{n}\"\nversion = \"1.0\"\n[depends]\nbuild = {:?}\n[build]\ninstall = \"true\"\n",
                    deps
                );
                let r: recipe::Recipe = toml::from_str(&toml_src).unwrap();
                (n.to_string(), r)
            })
            .collect();
        let names = vec!["a".to_string(), "b".to_string()];
        let installed: HashSet<String> = names.iter().cloned().collect();
        let ordered = order_with_fallback(&names, &installed, &tree);
        assert_eq!(ordered, vec!["b", "a"], "installed targets must still be ordered");
    }

    #[test]
    fn archive_must_match_index_entry() {
        let base = tmpdir("pkginfo-match");
        let st = base.join("st");
        std::fs::create_dir_all(&st).unwrap();
        std::fs::write(st.join("f"), b"x").unwrap();
        let a = base.join("demo-1.0-1.nawa.tar.zst");
        let arch = std::env::consts::ARCH;
        archive::create(
            &st,
            &format!("pkgname = demo\npkgver = 1.0\npkgrel = 1\narch = {arch}\nsize = 1\n"),
            &a,
        )
        .unwrap();
        let ok = repo::RepoEntry {
            name: "demo".into(),
            version: "1.0".into(),
            rel: 1,
            arch: arch.into(),
            file: "demo-1.0-1.nawa.tar.zst".into(),
            sha256: String::new(),
            download_url: None,
            size: 0,
            installed_size: 0,
        };
        assert!(check_archive_matches_index(&a, &ok, false).is_ok());
        let newer = repo::RepoEntry { version: "2.0".into(), ..ok.clone() };
        assert!(check_archive_matches_index(&a, &newer, false).is_err());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn upgrade_orders_installed_targets_through_the_recipe_graph() {
        // end-to-end regression for the empty-plan-order bug: package "a" is
        // installed (that is what makes it an upgrade target) and has a
        // recipe in the tree — the upgrade must actually apply
        let base = tmpdir("order-e2e");
        let root = base.join("root");
        let v1 = make_pkg(&base, "a", "1.0", 1, b"one");
        install::install_archive(&v1, &root, false, true, None, crate::db::InstallReason::Explicit).unwrap();
        let v2 = make_pkg(&base, "a", "2.0", 1, b"two");
        let repo = setup_repo(&base, &[v2]);

        // recipe tree: a build-depends on b (b is not installed, not planned)
        let recipes = base.join("recipes");
        for (n, deps) in [("a", "[\"b\"]"), ("b", "[]")] {
            let dir = recipes.join(n);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("recipe.toml"),
                format!(
                    "[package]\nname = \"{n}\"\nversion = \"1.0\"\n\n[depends]\nbuild = {deps}\n\n[build]\ninstall = \"true\"\n"
                ),
            )
            .unwrap();
        }

        upgrade(&root, &RepoRef::Local(repo), &recipes, &UpgradeOpts {
            dry_run: false, force: false, allow_unsigned: true, yes: true,
        })
        .unwrap();
        assert_eq!(std::fs::read(root.join("bin")).unwrap(), b"two");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn index_older_than_seen_is_rejected() {
        let base = tmpdir("freshness");
        std::fs::write(base.join("index.seen"), "200").unwrap();
        let idx = |g| repo::Index { generated: Some(g), expires: None, repository: None, recipes: None, groups: None, packages: vec![] };
        assert!(check_index_freshness(&base, &idx(100), true, false, true).is_err());
        assert!(check_index_freshness(&base, &idx(300), true, false, true).is_ok());
        assert_eq!(std::fs::read_to_string(base.join("index.seen")).unwrap(), "300");
        // local repos are not tracked
        assert!(check_index_freshness(&base, &idx(1), false, false, true).is_ok());
        // an unauthenticated index must not move the marker
        assert!(check_index_freshness(&base, &idx(999), true, false, false).is_ok());
        assert_eq!(std::fs::read_to_string(base.join("index.seen")).unwrap(), "300");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn remote_repo_requires_trusted_keys_unless_opted_out() {
        let base = tmpdir("remote-trust");
        // empty trust store: refused before any network access
        let err = require_trust_for_remote(&base, "https://mirror.invalid/stable", false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("no trusted keys"), "{err}");
        // explicit opt-out
        assert!(require_trust_for_remote(&base, "https://mirror.invalid/stable", true).is_ok());
        // with a trusted key configured, the repo is allowed (signatures decide)
        let (_, pubk) = sign::keygen(&base.join("keys")).unwrap();
        sign::trust(&pubk, &base).unwrap();
        assert!(require_trust_for_remote(&base, "https://mirror.invalid/stable", false).is_ok());
        // and the full entry point fails fast for an untrusted root
        let empty = tmpdir("remote-trust-root");
        let r = upgrade(
            &empty,
            &RepoRef::Remote("http://127.0.0.1:1/x".into()),
            Path::new("recipes"),
            &UpgradeOpts { dry_run: true, force: false, allow_unsigned: false, yes: true },
        );
        assert!(r.unwrap_err().to_string().contains("no trusted keys"));
        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::remove_dir_all(&empty);
    }

    #[test]
    fn expired_remote_index_is_refused_unless_forced() {
        let base = tmpdir("expiry");
        let idx = repo::Index { generated: Some(1000), expires: Some(2000), repository: None, recipes: None, groups: None, packages: vec![] };
        assert!(check_index_freshness(&base, &idx, true, false, true).is_err());
        assert!(check_index_freshness(&base, &idx, true, true, true).is_ok());
        // local repos only warn
        assert!(check_index_freshness(&base, &idx, false, false, true).is_ok());
        let _ = std::fs::remove_dir_all(&base);
    }
    #[test]
    fn future_index_timestamp_cannot_poison_seen_marker() {
        let base = tmpdir("future-index");
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let ahead = now + 3600;
        let idx = repo::Index {
            generated: Some(ahead),
            expires: Some(ahead + 10_000),
            repository: None,
            recipes: None,
            groups: None,
            packages: vec![],
        };
        // a clock that runs behind the publisher still gets a usable index ...
        assert!(check_index_freshness(&base, &idx, true, false, true).is_ok());
        // ... but the future value is never recorded as the newest seen
        assert!(!base.join("index.seen").exists());

        let current = repo::Index {
            generated: Some(now.saturating_sub(1)),
            expires: Some(now + 10_000),
            repository: None,
            recipes: None,
            groups: None,
            packages: vec![],
        };
        assert!(check_index_freshness(&base, &current, true, false, true).is_ok());
        assert_eq!(
            std::fs::read_to_string(base.join("index.seen")).unwrap(),
            now.saturating_sub(1).to_string()
        );
        let _ = std::fs::remove_dir_all(&base);
    }

}
