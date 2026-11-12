//! Multiple-repository configuration and administration commands
//! (roadmap Phase 4): repolist, makecache, repoquery, clean.
//!
//! `[[repositories]]` entries in nawa.toml add to (and, when present,
//! replace) the legacy scalar `repo`. Repository ids are validated
//! identifiers; cache stays URL-scoped as before.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

use crate::config::Config;
use crate::fetch::RepoRef;

/// One configured repository (roadmap §8.2).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RepoConfig {
    pub id: String,
    #[serde(default)]
    pub name: String,
    pub url: String,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default = "default_priority")]
    pub priority: u32,
}

fn default_enabled() -> bool {
    true
}

fn default_priority() -> u32 {
    50
}

/// Validate a repository id: conservative identifier charset.
pub(crate) fn check_repo_id(id: &str) -> Result<()> {
    if !crate::db::valid_name(id) {
        bail!("invalid repository id {id:?} (letters, digits, + _ . - only)");
    }
    Ok(())
}

/// The effective repository set (roadmap §8.3):
/// * explicit `[[repositories]]` entries are the set;
/// * otherwise the legacy scalar `repo` becomes one `default` repository;
/// * both present: entries win and the legacy value is ignored (warned).
pub(crate) fn effective(cfg: &Config) -> Result<Vec<RepoConfig>> {
    let entries = parse_file_entries(cfg)?;
    if !entries.is_empty() {
        if cfg.repo.is_some() {
            eprintln!(
                "nawa: note: both `repo` and [[repositories]] are configured — \
                 the explicit repository list wins"
            );
        }
        let mut ids = std::collections::BTreeSet::new();
        for r in &entries {
            check_repo_id(&r.id)?;
            if !ids.insert(r.id.clone()) {
                bail!("duplicate repository id {:?}", r.id);
            }
        }
        return Ok(entries);
    }
    Ok(cfg
        .repo
        .as_ref()
        .map(|url| vec![RepoConfig {
            id: "default".into(),
            name: "Default repository".into(),
            url: url.clone(),
            enabled: true,
            priority: 50,
        }])
        .unwrap_or_default())
}

fn parse_file_entries(cfg: &Config) -> Result<Vec<RepoConfig>> {
    let Some(path) = cfg.file.as_deref() else { return Ok(Vec::new()) };
    let raw = std::fs::read_to_string(path).unwrap_or_default();
    let doc: toml::Table = toml::from_str(&raw).unwrap_or_default();
    let Some(arr) = doc.get("repositories").and_then(|v| v.as_array()) else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for v in arr {
        let r: RepoConfig = v
            .clone()
            .try_into()
            .context("a [[repositories]] entry is malformed")?;
        out.push(r);
    }
    Ok(out)
}

/// `nawa repolist [--all|--enabled]` — configuration + cached state, no
/// downloads (roadmap §8.6).
pub(crate) fn cmd_repolist(cfg: &Config, all: bool) -> Result<()> {
    let repos = effective(cfg)?;
    if repos.is_empty() {
        crate::nawa_info!("no repositories configured");
        return Ok(());
    }
    println!("{:<12}  {:<8}  {:<8}  URL", "ID", "State", "Priority");
    println!("{}", "-".repeat(60));
    for r in &repos {
        if !all && !r.enabled {
            continue;
        }
        println!(
            "{:<12}  {:<8}  {:<8}  {}",
            r.id,
            if r.enabled { "enabled" } else { "disabled" },
            r.priority,
            r.url
        );
    }
    Ok(())
}

/// `nawa makecache` — refresh enabled repositories' metadata through the
/// existing authentication/freshness path (roadmap §8.10).
pub(crate) fn cmd_makecache(cfg: &Config, root: &Path, allow_unsigned: bool, force: bool) -> Result<()> {
    let repos = effective(cfg)?;
    let enabled: Vec<&RepoConfig> = repos.iter().filter(|r| r.enabled).collect();
    if enabled.is_empty() {
        crate::nawa_info!("no enabled repositories to refresh");
        return Ok(());
    }
    let total = enabled.len();
    let mut failures = 0;
    for r in enabled {
        let repo_ref = RepoRef::parse(&r.url)?;
        match crate::upgrade::open_repo(root, &repo_ref, allow_unsigned, force) {
            Ok(open) => crate::nawa_info!(
                "{} ({}): cache verified — {} package(s), schema v{}",
                r.id,
                r.url,
                open.index.packages.len(),
                open.index.repository.as_ref().map(|m| m.schema).unwrap_or(1)
            ),
            Err(e) => {
                failures += 1;
                crate::nawa_error!("{} ({}): {e:#}", r.id, r.url);
            }
        }
    }
    if failures > 0 {
        bail!("{failures} of {total} repository refresh(es) failed — caches NOT replaced");
    }
    Ok(())
}

/// `nawa repoquery <name>` — query enabled repositories' cached metadata
/// without downloading any archive (roadmap §8.9).
pub(crate) fn cmd_repoquery(cfg: &Config, root: &Path, name: &str, repo_id: Option<&str>, info: bool) -> Result<()> {
    let repos = effective(cfg)?;
    let enabled: Vec<&RepoConfig> = repos
        .iter()
        .filter(|r| r.enabled && repo_id.map(|i| i == r.id).unwrap_or(true))
        .collect();
    if enabled.is_empty() {
        bail!("no enabled repositories to query");
    }
    let mut found = 0;
    for r in &enabled {
        let repo_ref = RepoRef::parse(&r.url)?;
        // local repos read directly; remote repos use the cache
        let dir = match &repo_ref {
            RepoRef::Local(d) => d.clone(),
            RepoRef::Remote(_) => crate::fetch::remote_cache_dir(root, &r.url),
        };
        let index_path = dir.join("index.toml");
        if !index_path.exists() {
            continue;
        }
        let bytes = std::fs::read(&index_path).unwrap_or_default();
        let Ok(index) = crate::repo::load_index_bytes(&bytes) else { continue };
        if let Some(e) = index.find(name) {
            found += 1;
            if info {
                println!("Package   : {}-{}-{}", e.name, e.version, e.rel);
                println!("Repository: {}", r.id);
                println!("Arch      : {}", e.arch);
                println!("Size      : {}", crate::humanize::format_size(e.size));
                println!("Installed : {}", crate::humanize::format_size(e.installed_size));
                println!("SHA-256   : {}", e.sha256);
                println!("File      : {}", e.file);
            } else {
                println!("{:<12}  {}-{}-{}  {}", r.id, e.name, e.version, e.rel, e.arch);
            }
        }
    }
    if found == 0 {
        bail!("no package {name:?} found in the enabled repositories");
    }
    Ok(())
}

/// `nawa clean <metadata|packages|sources|layers|all> [--dry-run]`
/// (roadmap §8.11). Mark-and-sweep for package objects: everything
/// referenced by installed entries, history records or legacy undo records
/// is retained.
///
/// `sources` (fetch-phase.md §11) operates on the *resolved* source cache —
/// `$NAWA_SRCCACHE`, default `<builddir>/distfiles` — where staging actually
/// puts files, and removes only what Nawa owns: legacy `<sha16>-*` entries,
/// the `sha256/` and `fetch/` directories, and leftover `*.part` / `*.lock`
/// files. The cache root itself is never removed (it may be a directory
/// shared with other tools/users) and unrelated entries are left alone. A
/// `.lock` held by a live fetch keeps its file; read-only cache entries get
/// owner-write restored before removal.
pub(crate) fn cmd_clean(ctx: &crate::Ctx, category: &str, dry_run: bool) -> Result<()> {
    let cat = category.to_ascii_lowercase();
    if !matches!(cat.as_str(), "metadata" | "packages" | "sources" | "layers" | "all") {
        bail!("unknown clean category {cat:?} (metadata | packages | sources | layers | all)");
    }
    let mut removed: u64 = 0;
    let mut retained: u64 = 0;

    if cat == "metadata" || cat == "all" {
        let dir = ctx.root.join("var/cache/nawa/remote");
        removed += clean_dir(&dir, dry_run);
    }
    if cat == "sources" || cat == "all" {
        let srccache = crate::config::srccache_dir(&ctx.builddir, None);
        let (freed, kept) = clean_sources(&srccache, dry_run)?;
        removed += freed;
        retained += kept;
    }
    if cat == "layers" || cat == "all" {
        let dir = ctx.root.join("var/cache/nawa/layers");
        removed += clean_dir(&dir, dry_run);
    }
    if cat == "packages" || cat == "all" {
        // mark: installed entries + history + legacy undo
        let root = &ctx.root;
        let mut keep = std::collections::BTreeSet::new();
        for e in crate::db::Db::new(root).installed()? {
            if !e.artifact_sha256.is_empty() {
                keep.insert(e.artifact_sha256.clone());
            }
            // legacy named caches of installed versions stay too
            let legacy = root.join("var/cache/nawa").join(format!("{}.nawa.tar.zst", e.version_full()));
            if let Ok(md) = std::fs::metadata(&legacy) {
                retained += md.len();
            }
        }
        for digest in crate::transaction::history::referenced_artifacts(root)? {
            keep.insert(digest);
        }
        let store = crate::artifact::store_dir(root);
        if store.exists() {
            for fan in std::fs::read_dir(&store)?.filter_map(|e| e.ok()) {
                let fan_dir = fan.path();
                if !fan_dir.is_dir() {
                    continue;
                }
                for obj in std::fs::read_dir(&fan_dir)?.filter_map(|e| e.ok()) {
                    let p = obj.path();
                    let name = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                    let Some(digest) = name
                        .strip_suffix(".nawa.tar.zst")
                        .map(str::to_string)
                        .and_then(|d| {
                            // reconstruct full digest from fan + file name
                            d.strip_prefix(&fan_dir.file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_default()).map(str::to_string)
                        })
                    else {
                        continue;
                    };
                    let full = format!("{}{}", fan_dir.file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_default(), name.strip_suffix(".nawa.tar.zst").unwrap_or(&name));
                    let _ = digest;
                    if keep.contains(&full) {
                        if let Ok(md) = std::fs::metadata(&p) {
                            retained += md.len();
                        }
                        continue;
                    }
                    if let Ok(md) = std::fs::metadata(&p) {
                        removed += md.len();
                    }
                    if !dry_run {
                        std::fs::remove_file(&p).with_context(|| format!("remove {}", p.display()))?;
                    }
                }
            }
        }
    }
    if dry_run {
        crate::nawa_info!("dry run — would free {}, retaining {}", crate::humanize::format_size(removed), crate::humanize::format_size(retained));
    } else {
        crate::nawa_info!("cleaned {} (retained {} of referenced data)", crate::humanize::format_size(removed), crate::humanize::format_size(retained));
    }
    Ok(())
}

fn clean_dir(dir: &Path, dry_run: bool) -> u64 {
    let mut freed = 0;
    let Ok(rd) = std::fs::read_dir(dir) else { return 0 };
    for e in rd.filter_map(|e| e.ok()) {
        let p = e.path();
        let name = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        // active .part files of live workers are never touched
        if name.ends_with(".part") {
            continue;
        }
        if let Ok(md) = std::fs::metadata(&p) {
            freed += md.len();
        }
        if !dry_run {
            // `remove_dir_all` refuses regular files; pick the right unlink
            // without following symlinks.
            let _ = if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                std::fs::remove_dir_all(&p)
            } else {
                std::fs::remove_file(&p)
            };
        }
    }
    freed
}

/// §11 `clean sources`: sweep the *resolved* source cache root's Nawa-owned
/// subsets, leaving the root and everything else untouched. Returns
/// (freed, retained) bytes.
fn clean_sources(cache: &Path, dry_run: bool) -> Result<(u64, u64)> {
    let mut freed = 0u64;
    let mut retained = 0u64;
    if !cache.is_dir() {
        return Ok((0, 0));
    }
    for e in std::fs::read_dir(cache)?.filter_map(|e| e.ok()) {
        let p = e.path();
        let name = p
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let md = e.metadata()?;
        if md.is_dir() && (name == "sha256" || name == "fetch") {
            // Nawa-owned directories: content-addressed entries (§11) and
            // per-backend ecosystem entries (§7/§8) live wholly inside.
            freed += sweep_tree(&p, dry_run)?;
        } else if md.is_file() {
            let leftover_lock = name.ends_with(".lock") && lock_leftover(&p);
            if name.ends_with(".part") || leftover_lock {
                // downloads leave *.part; the fetch runners leave *.lock —
                // a lock nobody holds belongs to a dead fetch, a held one
                // belongs to a live worker and is kept
                freed += md.len();
                if !dry_run {
                    std::fs::remove_file(&p)
                        .with_context(|| format!("remove {}", p.display()))?;
                }
            } else if legacy_cache_entry(&name) {
                // the legacy pinned tarball cache: `<sha16>-<file name>` (§1.1)
                freed += md.len();
                if !dry_run {
                    std::fs::remove_file(&p)
                        .with_context(|| format!("remove {}", p.display()))?;
                }
            } else if name.ends_with(".lock") {
                // a lock we couldn't take: a live fetch holds it
                retained += md.len();
                crate::nawa_info!("clean sources: leaving {} (a live fetch holds its lock)", p.display());
            } else {
                retained += md.len();
                crate::nawa_info!("clean sources: leaving {} (not Nawa-owned)", p.display());
            }
        } else {
            retained += md.len();
            crate::nawa_info!("clean sources: leaving {} (not Nawa-owned)", p.display());
        }
    }
    Ok((freed, retained))
}

/// `<sha16>-<file name>` legacy cache entry.
fn legacy_cache_entry(name: &str) -> bool {
    name.len() > 16
        && name.as_bytes()[16] == b'-'
        && name[..16].bytes().all(|b| b.is_ascii_hexdigit())
}

/// A `.lock` file with no flock holder is a leftover of a dead fetch; the
/// probe lock is released the moment the handle closes.
fn lock_leftover(p: &Path) -> bool {
    let Ok(f) = std::fs::OpenOptions::new().read(true).open(p) else {
        return true; // unreadable → nothing holds it meaningfully
    };
    rustix::fs::flock(&f, rustix::fs::FlockOperation::NonBlockingLockExclusive).is_ok()
}

/// Walk a tree reverting read-only bits to owner-write where needed and
/// summing file sizes, then remove it; cache entries are published without
/// write bits (§11) and cannot be unlinked from 0555 directories otherwise.
fn sweep_tree(dir: &Path, dry_run: bool) -> Result<u64> {
    use std::os::unix::fs::PermissionsExt;
    let mut freed = 0u64;
    for e in walkdir::WalkDir::new(dir).into_iter().flatten() {
        if e.file_type().is_symlink() {
            continue; // links carry no content; their removal needs only the parent
        }
        let Ok(md) = e.metadata() else { continue };
        if md.is_file() {
            freed += md.len();
        }
        if !dry_run {
            let mut perm = md.permissions();
            if perm.mode() & 0o200 == 0 {
                perm.set_mode(perm.mode() | 0o200);
                let _ = std::fs::set_permissions(e.path(), perm);
            }
        }
    }
    if !dry_run {
        std::fs::remove_dir_all(dir).with_context(|| format!("remove {}", dir.display()))?;
    }
    Ok(freed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::tmpdir;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn clean_dir_keeps_active_part_files() {
        let d = tmpdir("clean-dir");
        std::fs::write(d.join("old.bin"), b"old").unwrap();
        std::fs::create_dir(d.join("tree")).unwrap();
        std::fs::write(d.join("tree/nested"), b"nested").unwrap();
        std::fs::write(d.join("live.part"), b"partial").unwrap();

        assert!(clean_dir(&d, true) > 0);
        assert!(d.join("old.bin").exists());
        assert!(d.join("tree/nested").exists());
        assert!(d.join("live.part").exists());

        clean_dir(&d, false);
        assert!(!d.join("old.bin").exists());
        assert!(!d.join("tree").exists());
        assert!(d.join("live.part").exists(), "a live worker's part file must survive");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn legacy_cache_entry_requires_a_sha16_dash_prefix() {
        assert!(legacy_cache_entry(&format!("{}-pkg.tar.zst", "0123456789abcdef")));
        assert!(legacy_cache_entry(&format!("{}-file", "a".repeat(16))));
        assert!(!legacy_cache_entry("short-file"));
        assert!(!legacy_cache_entry(&format!("{}-file", "a".repeat(15))));
        assert!(!legacy_cache_entry(&format!("{}-file", "g".repeat(16))));
        assert!(!legacy_cache_entry(&format!("{}file", "a".repeat(16))));
    }

    #[test]
    fn sweep_tree_dry_run_does_not_change_permissions() {
        let d = tmpdir("clean-sweep");
        let entry = d.join("sha256");
        std::fs::create_dir(&entry).unwrap();
        let file = entry.join("object");
        std::fs::write(&file, b"bytes").unwrap();
        std::fs::set_permissions(&entry, std::fs::Permissions::from_mode(0o555)).unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o444)).unwrap();

        assert_eq!(sweep_tree(&entry, true).unwrap(), 5);
        assert_eq!(std::fs::metadata(&entry).unwrap().permissions().mode() & 0o777, 0o555);
        assert_eq!(std::fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o444);
        assert!(file.exists(), "a dry run must not remove anything");

        sweep_tree(&entry, false).unwrap();
        assert!(!entry.exists());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn clean_sources_sweeps_nawa_data_but_keeps_foreign_files_and_live_locks() {
        let d = tmpdir("clean-sources");
        let cache = d.join("cache");
        std::fs::create_dir_all(cache.join("sha256")).unwrap();
        std::fs::create_dir_all(cache.join("fetch/go")).unwrap();
        std::fs::write(cache.join("sha256/object"), b"object").unwrap();
        std::fs::write(cache.join("fetch/go/entry"), b"entry").unwrap();
        let legacy = format!("{}-legacy.tar.gz", "a".repeat(16));
        std::fs::write(cache.join(&legacy), b"legacy").unwrap();
        std::fs::write(cache.join("keep.txt"), b"foreign").unwrap();
        std::fs::write(cache.join("dead.lock"), b"").unwrap();
        let live = cache.join("live.lock");
        std::fs::write(&live, b"live").unwrap();
        let held = std::fs::OpenOptions::new().read(true).open(&live).unwrap();
        rustix::fs::flock(&held, rustix::fs::FlockOperation::NonBlockingLockExclusive).unwrap();

        let (freed, retained) = clean_sources(&cache, true).unwrap();
        assert!(freed > 0);
        assert!(retained > 0);
        assert!(cache.join("sha256/object").exists());
        assert!(cache.join("fetch/go/entry").exists());
        assert!(cache.join(&legacy).exists());
        assert!(cache.join("dead.lock").exists());
        assert!(cache.join("keep.txt").exists());

        assert!(clean_sources(&cache, false).unwrap().0 > 0);
        assert!(!cache.join("sha256").exists());
        assert!(!cache.join("fetch").exists());
        assert!(!cache.join(&legacy).exists());
        assert!(!cache.join("dead.lock").exists());
        assert!(cache.join("keep.txt").exists(), "unowned files must survive");
        assert!(live.exists(), "a held lock must survive");
        drop(held);
        let _ = std::fs::remove_dir_all(&d);
    }
}
