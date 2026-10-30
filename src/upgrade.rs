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

use anyhow::{bail, Context, Result};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use crate::db::Db;
use crate::deps;
use crate::fetch::{self, RepoRef};
use crate::install;
use crate::recipe;
use crate::repo;
use crate::sign;
use crate::version::vercmp;

pub struct UpgradeOpts {
    pub dry_run: bool,
    pub force: bool,
    pub allow_unsigned: bool,
    /// skip the interactive confirmation (non-interactive callers must
    /// pass it — a piped stdin otherwise refuses, never silently proceeds)
    pub yes: bool,
}

/// A repository materialized on disk: for a local repo its own directory,
/// for a remote one the per-URL cache holding the freshly fetched index.
pub struct OpenRepo {
    /// directory holding `index.toml` (and, locally, the archives)
    pub dir: PathBuf,
    /// `Some(url)` for a remote repository — archives are fetched on demand
    pub base_url: Option<String>,
    /// the parsed, signature-checked, freshness-checked index
    pub index: repo::Index,
}

/// Fetch (remote) or locate (local) a repository, then parse and
/// authenticate its index: trust policy for remote repos, signature check,
/// rollback/freeze protection. Shared by `nawa upgrade` and `nawa install`.
pub fn open_repo(
    root: &Path,
    repo_ref: &RepoRef,
    allow_unsigned: bool,
    force: bool,
) -> Result<OpenRepo> {
    // ---- materialize the repository (local dir, or fetch over http(s)) -----
    let repo_dir: PathBuf;
    let base_url: Option<String>;
    match repo_ref {
        RepoRef::Local(dir) => {
            if !dir.is_dir() {
                bail!("repository directory not found: {}", dir.display());
            }
            repo_dir = dir.clone();
            base_url = None;
        }
        RepoRef::Remote(url) => {
            require_trust_for_remote(root, url, allow_unsigned)?;
            let cache = fetch::remote_cache_dir(root, url);
            std::fs::create_dir_all(&cache)
                .with_context(|| format!("create cache {}", cache.display()))?;
            let index_path = cache.join("index.toml");
            let index_url = format!("{url}/index.toml");
            if index_path.exists() {
                // refresh in place; on network failure keep the cached index
                match fetch::download_opt(&index_url, &cache.join("index.toml.part")) {
                    Ok(true) => {
                        std::fs::rename(cache.join("index.toml.part"), &index_path)?;
                    }
                    Ok(false) => bail!("remote index not available (HTTP error): {index_url}"),
                    Err(e) => eprintln!(
                        "nawa: warning: cannot reach {url} ({e:#}) — using cached index"
                    ),
                }
            } else {
                println!("nawa: fetching index from {index_url}");
                // land in a .part file first: an interrupted transfer must not
                // leave a truncated index.toml behind as the "cached" copy
                let part = cache.join("index.toml.part");
                fetch::download(&index_url, &part)?;
                std::fs::rename(&part, &index_path)?;
            }
            // the signature sidecar is optional — fetch it when published.
            // It too lands in a .part file first: a truncated cached .sig
            // would make every later upgrade fail signature verification.
            let sig_path = sign::sig_path_for(&index_path);
            let sig_part = cache.join("index.toml.sig.part");
            match fetch::download_opt(&format!("{url}/index.toml.sig"), &sig_part) {
                Ok(true) => {
                    std::fs::rename(&sig_part, &sig_path)?;
                }
                Ok(false) => {
                    let _ = std::fs::remove_file(&sig_part);
                    let _ = std::fs::remove_file(&sig_path);
                    println!("nawa: note: remote repo publishes no index signature");
                }
                Err(e) if sig_path.exists() => {
                    let _ = std::fs::remove_file(&sig_part);
                    eprintln!("nawa: warning: using cached index signature ({e:#})");
                }
                Err(e) => {
                    let _ = std::fs::remove_file(&sig_part);
                    eprintln!("nawa: note: could not fetch index signature ({e:#})");
                }
            }
            repo_dir = cache;
            base_url = Some(url.clone());
        }
    }

    let index_path = repo_dir.join("index.toml");
    let index = repo::load_index(&index_path)?;

    // ---- verify the index signature when one is present --------------------
    let sig_path = sign::sig_path_for(&index_path);
    if sig_path.exists() {
        match sign::verify_standalone(&index_path, root, None) {
            Ok(()) => {}
            Err(e) => bail!("repository index signature is INVALID: {e:#}"),
        }
    } else {
        if !allow_unsigned && !sign::trusted_keys(root)?.is_empty() {
            bail!(
                "repository index {} is unsigned but trusted keys are configured\n(pass --allow-unsigned to override)",
                index_path.display()
            );
        }
        eprintln!("nawa: warning: repository index is unsigned ({})", index_path.display());
    }

    // only an index verified against a configured trusted key may advance the
    // "newest index seen" marker — otherwise one forged index with a huge
    // timestamp would block every later (genuine) upgrade from this repo
    let authenticated = sig_path.exists() && !sign::trusted_keys(root)?.is_empty();
    check_index_freshness(&repo_dir, &index, base_url.is_some(), force, authenticated)?;
    Ok(OpenRepo { dir: repo_dir, base_url, index })
}

/// Make one index entry's archive available locally and fully verified:
/// downloaded into the cache (remote) or located in the repo dir (local),
/// then sha256 against the index, signature policy, and the `.PKGINFO`
/// must match what the index promised.
pub fn fetch_entry(
    root: &Path,
    open: &OpenRepo,
    re: &repo::RepoEntry,
    allow_unsigned: bool,
) -> Result<PathBuf> {
    let repo_dir = &open.dir;
    let base_url = &open.base_url;
    // a malicious or compromised index must not be able to escape the
    // repo/cache directory through crafted archive paths (local indexes
    // are validated exactly like remote ones — repo-create only ever
    // writes plain file names, so nothing legitimate is excluded)
    validate_entry_filename(&re.file)?;
    let src = match base_url {
        None => {
            let p = repo_dir.join(&re.file);
            if !p.exists() {
                bail!("archive {} listed in the index is missing", p.display());
            }
            p
        }
        Some(url) => {
            let dest = repo_dir.join(&re.file);
            fetch::download_cached(&format!("{url}/{}", re.file), &dest, Some(&re.sha256))?;
            // the detached signature travels with the archive; it lands in
            // a .part file first so an interrupted transfer cannot corrupt
            // the cached copy (a truncated .sig would fail every future
            // upgrade until removed by hand)
            let sig_dest = sign::sig_path_for(&dest);
            let sig_part = sig_dest.with_file_name(format!(
                "{}.part",
                sig_dest.file_name().unwrap_or_default().to_string_lossy()
            ));
            match fetch::download_opt(&format!("{url}/{}.sig", re.file), &sig_part) {
                Ok(true) => {
                    std::fs::rename(&sig_part, &sig_dest).with_context(|| {
                        format!("activate {}", sig_dest.display())
                    })?;
                }
                Ok(false) => {
                    let _ = std::fs::remove_file(&sig_part);
                    let _ = std::fs::remove_file(&sig_dest);
                }
                Err(e) => {
                    let _ = std::fs::remove_file(&sig_part);
                    eprintln!(
                        "nawa: warning: could not fetch signature for {} ({e:#})",
                        re.file
                    );
                }
            }
            dest
        }
    };
    // integrity: sha256 recorded in the index
    fetch::verify_sha256(&src, &re.sha256)?;
    // authenticity: signature policy (trust store / embedded / allow-unsigned)
    sign::verify_for_install(&src, root, None, allow_unsigned)?;
    // the archive must be the package/version the index promised
    // (blocks replaying an older signed archive as an "upgrade")
    check_archive_matches_index(&src, re)?;
    Ok(src)
}

pub fn upgrade(
    root: &Path,
    repo_ref: &RepoRef,
    recipes: &Path,
    opts: &UpgradeOpts,
) -> Result<()> {
    let open = open_repo(root, repo_ref, opts.allow_unsigned, opts.force)?;
    let index = &open.index;

    // ---- compute the upgrade plan -------------------------------------------
    let db = Db::new(root);
    let mut plan: Vec<(&repo::RepoEntry, String)> = Vec::new(); // (entry, new full version)
    for e in db.installed()? {
        let re = match index.find(&e.name) {
            Some(r) => r,
            None => continue, // not in this repo -> untouched
        };
        let new = format!("{}-{}-{}", re.name, re.version, re.rel);
        if vercmp(&format!("{}-{}", re.version, re.rel), &format!("{}-{}", e.version, e.rel))
            == std::cmp::Ordering::Greater
        {
            plan.push((re, new));
        }
    }

    if plan.is_empty() {
        println!("nawa: everything up to date ({} installed)", db.installed()?.len());
        return Ok(());
    }

    // dependency order when the recipe tree is available; packages that have
    // no recipe (third-party repo content) are appended alphabetically so a
    // partial tree never aborts the upgrade
    let names: Vec<String> = plan.iter().map(|(r, _)| r.name.clone()).collect();
    let ordered: Vec<String> = match recipe::load_all(recipes) {
        Ok(tree) if !tree.is_empty() => {
            let installed: HashSet<String> = db.installed()?.into_iter().map(|e| e.name).collect();
            order_with_fallback(&names, &installed, &tree)
        }
        _ => {
            eprintln!("nawa: note: no recipe tree at {} — applying in alphabetical order", recipes.display());
            let mut n = names.clone();
            n.sort();
            n
        }
    };

    let pal = crate::color::Palette::auto(crate::color::Stream::Stdout);
    println!("nawa: {} package(s) to upgrade:", plan.len());
    for name in &ordered {
        let (_, new) = plan.iter().find(|(r, _)| &r.name == name).unwrap();
        let cur = db.entry(name)?.map(|e| e.version_full()).unwrap_or_default();
        println!(
            "  {} {} {} {}",
            pal.name(&format!("{name:<24}")),
            pal.ver(&cur),
            pal.dim("->"),
            pal.ok(new),
        );
    }

    if opts.dry_run {
        println!("nawa: dry run — no changes made");
        return Ok(());
    }

    // the plan is on the table; a real run needs an explicit go-ahead
    // (scripts pass --yes; a piped stdin without it is refused, not guessed)
    crate::confirm::confirm("nawa: proceed with the upgrade?", opts.yes)?;

    // ---- apply ----------------------------------------------------------------
    let mut done = 0usize;
    for name in &ordered {
        let (re, _) = plan
            .iter()
            .find(|(r, _)| &r.name == name)
            .expect("ordered name came from the plan");
        let src = fetch_entry(root, &open, re, opts.allow_unsigned)?;
        install::install_archive(&src, root, opts.force, opts.allow_unsigned, None, "keep")?;
        done += 1;
    }
    println!("nawa: upgraded {done} package(s)");
    Ok(())
}

/// A remote repository is only usable when its content can be authenticated:
/// at least one trusted key must be configured (`nawa trust <key.pub>`), or the
/// caller must opt out explicitly with `--allow-unsigned`. Without a trust
/// store a signature is checked against the key embedded in the `.sig` file,
/// which proves integrity but not origin, and an unsigned repo is accepted
/// as-is: anyone able to modify the transfer (plain http, a compromised mirror)
/// could then run code as root through package files and hooks.
fn require_trust_for_remote(root: &Path, url: &str, allow_unsigned: bool) -> Result<()> {
    if allow_unsigned {
        eprintln!("nawa: warning: --allow-unsigned: {url} is NOT authenticated");
        return Ok(());
    }
    if sign::trusted_keys(root)?.is_empty() {
        bail!(
            "refusing remote repository {url}: no trusted keys configured in {}\n\
             (add the repository's public key with `nawa trust <key.pub>`, or pass --allow-unsigned to accept it unauthenticated)",
            sign::trust_dir(root).display()
        );
    }
    if url.starts_with("http://") {
        eprintln!("nawa: warning: {url} uses plain http — authenticity relies on signatures only");
    }
    Ok(())
}

/// Rollback / freeze protection for the (signed) index timestamp.
/// Remote repos remember the newest `generated` value seen in the cache dir
/// and refuse anything older; a very old index only produces a warning.
fn check_index_freshness(
    repo_dir: &Path,
    index: &repo::Index,
    remote: bool,
    force: bool,
    authenticated: bool,
) -> Result<()> {
    let gen = match index.generated {
        Some(g) => g,
        None => {
            eprintln!("nawa: note: index has no timestamp — no rollback protection");
            return Ok(());
        }
    };
    if remote {
        let seen_path = repo_dir.join("index.seen");
        if let Ok(raw) = std::fs::read_to_string(&seen_path) {
            if let Ok(seen) = raw.trim().parse::<u64>() {
                if gen < seen {
                    bail!(
                        "repository index is older than one already seen (possible rollback attack)"
                    );
                }
            }
        }
        if authenticated {
            let _ = std::fs::write(&seen_path, gen.to_string());
        }
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if let Some(exp) = index.expires {
        if now > exp {
            if remote && !force {
                bail!(
                    "repository index expired {} days ago — the mirror may be frozen or stale\n(use --force to upgrade anyway)",
                    (now - exp) / 86400
                );
            }
            eprintln!("nawa: warning: repository index has expired");
        }
    }
    if now > gen && now - gen > 30 * 24 * 3600 {
        eprintln!(
            "nawa: warning: repository index is {} days old",
            (now - gen) / 86400
        );
    }
    Ok(())
}

/// The `.PKGINFO` inside the archive must agree with the index entry.
fn check_archive_matches_index(archive: &Path, re: &repo::RepoEntry) -> Result<()> {
    let raw = crate::archive::read_pkginfo(archive)?;
    let info = crate::archive::parse_pkginfo(&raw)?;
    let get = |k: &str| info.get(k).cloned().unwrap_or_default();
    let rel: u32 = info.get("pkgrel").and_then(|v| v.parse().ok()).unwrap_or(1);
    let arch = get("arch");
    let host = std::env::var("NAWA_ARCH").unwrap_or_else(|_| std::env::consts::ARCH.to_string());
    if !arch.is_empty() && !matches!(arch.as_str(), "any" | "noarch" | "all") && arch != host {
        bail!("archive {} is built for {arch}, this system is {host}", re.file);
    }
    if get("pkgname") != re.name || get("pkgver") != re.version || rel != re.rel {
        bail!(
            "archive {} contains {}-{}-{} but the index promised {}-{}-{}",
            re.file,
            get("pkgname"),
            get("pkgver"),
            rel,
            re.name,
            re.version,
            re.rel
        );
    }
    Ok(())
}

/// A malicious or compromised index must not be able to escape the cache
/// directory through crafted archive paths.
pub(crate) fn validate_entry_filename(file: &str) -> Result<()> {
    if file.is_empty()
        || file.contains('/')
        || file.contains('\\')
        || file.contains("..")
        || file.starts_with('.')
    {
        bail!("index entry has an unsafe archive path: `{file}`");
    }
    Ok(())
}

/// Order the upgrade plan by the recipe graph where recipes exist; anything
/// the tree does not know about (foreign repo packages) is appended
/// alphabetically. If even the known subset cannot be resolved (missing
/// dep recipes), the whole plan falls back to alphabetical order.
///
/// Every name in `names` is an upgrade TARGET and therefore installed. The
/// resolver treats "installed" as "done", so the targets must be taken out
/// of the satisfied set — otherwise the order comes back empty and nothing
/// is applied (the bug this guard fixes). Conversely, only planned names may
/// appear in the result: a build dependency that is not part of the plan is
/// irrelevant for a binary upgrade and would panic the plan lookup in the
/// caller.
fn order_with_fallback(
    names: &[String],
    installed: &HashSet<String>,
    tree: &BTreeMap<String, recipe::Recipe>,
) -> Vec<String> {
    let planned: HashSet<&String> = names.iter().collect();
    let satisfied: HashSet<String> =
        installed.iter().filter(|n| !planned.contains(*n)).cloned().collect();
    let known: Vec<String> = names.iter().filter(|n| tree.contains_key(*n)).cloned().collect();
    let mut ordered: Vec<String> = match deps::resolve(&known, &satisfied, tree) {
        Ok(order) => order
            .into_iter()
            .map(|r| r.package.name)
            .filter(|n| planned.contains(n))
            .collect(),
        Err(_) => {
            eprintln!("nawa: warning: recipe graph incomplete for this plan — alphabetical order");
            let mut k = known.clone();
            k.sort();
            k
        }
    };
    let mut unknown: Vec<String> =
        names.iter().filter(|n| !tree.contains_key(*n)).cloned().collect();
    unknown.sort();
    ordered.extend(unknown);
    ordered
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive;
    use std::path::PathBuf;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("nawa-upgrade-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

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
    fn upgrade_installs_newer_version_from_repo() {
        let base = tmpdir("e2e");
        let root = base.join("root");

        // v1.0 installed directly from an archive outside the repo
        let v1 = make_pkg(&base, "demo", "1.0", 1, b"one");
        install::install_archive(&v1, &root, false, true, None, "explicit").unwrap();

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
        install::install_archive(&v1, &root, false, true, None, "explicit").unwrap();
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
        };
        assert!(check_archive_matches_index(&a, &ok).is_ok());
        let newer = repo::RepoEntry { version: "2.0".into(), ..ok.clone() };
        assert!(check_archive_matches_index(&a, &newer).is_err());
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
        install::install_archive(&v1, &root, false, true, None, "explicit").unwrap();
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
        let idx = |g| repo::Index { generated: Some(g), expires: None, recipes: None, packages: vec![] };
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
        let idx = repo::Index { generated: Some(1000), expires: Some(2000), recipes: None, packages: vec![] };
        assert!(check_index_freshness(&base, &idx, true, false, true).is_err());
        assert!(check_index_freshness(&base, &idx, true, true, true).is_ok());
        // local repos only warn
        assert!(check_index_freshness(&base, &idx, false, false, true).is_ok());
        let _ = std::fs::remove_dir_all(&base);
    }
}
