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

// Note: trim unused imports in later pass
#[allow(unused_imports)]
use anyhow::{bail, Context, Result};
use std::collections::{HashMap, HashSet};
#[cfg(test)]
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::db::Db;
use crate::fetch::{self, RepoRef};
use crate::install;
use crate::recipe;
use crate::repo;
use crate::sign;
use crate::version::vercmp;

pub(crate) struct UpgradeOpts {
    pub dry_run: bool,
    pub force: bool,
    pub allow_unsigned: bool,
    /// skip the interactive confirmation (non-interactive callers must
    /// pass it — a piped stdin otherwise refuses, never silently proceeds)
    pub yes: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ArchiveAuth {
    /// Legacy v1 policy: each archive needs its own detached signature.
    DetachedSignature,
    /// V2 policy: a trusted signed index authenticates the archive SHA-256.
    SignedIndexSha256,
}

/// A repository materialized on disk: for a local repo its own directory,
/// for a remote one the per-URL cache holding the freshly fetched index.
pub(crate) struct OpenRepo {
    /// directory holding `index.toml` (and, locally, the archives)
    pub dir: PathBuf,
    /// `Some(url)` for a remote repository — archives are fetched on demand
    pub base_url: Option<String>,
    /// the parsed, signature-checked, freshness-checked index
    pub index: repo::Index,
    /// Authentication method selected only after validating the index signer.
    pub archive_auth: ArchiveAuth,
    /// The exact index bytes that were signature-verified (kept so installs
    /// can retain the proof bundle, roadmap §4.5).
    pub index_bytes: Vec<u8>,
    /// The signature sidecar bytes used for that verification, when one exists.
    pub index_signature: Option<Vec<u8>>,
    /// The public key bytes of the trusted signer, when the index was
    /// authenticated by a configured trusted key.
    pub signer_public: Option<Vec<u8>>,
    /// Parsed catalog whose exact bytes match the signed index digest.
    pub groups: Option<crate::groups::GroupCatalog>,
}

/// Fetch (remote) or locate (local) a repository, then parse and
/// authenticate its index: trust policy for remote repos, signature check,
/// rollback/freeze protection. Shared by `nawa upgrade` and `nawa install`.
pub(crate) fn open_repo(
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
            let mut index_refresh_unavailable = false;
            if index_path.exists() {
                // A failed cache write is not a network failure. Keep the
                // authenticated cached index, but report the actual cause.
                match fetch::download_opt(&index_url, &cache.join("index.toml.part")) {
                    Ok(true) => {
                        std::fs::rename(cache.join("index.toml.part"), &index_path)?;
                    }
                    Ok(false) => bail!("remote index not available (HTTP error): {index_url}"),
                    Err(e) if crate::diag::has_io_kind(
                        &e,
                        std::io::ErrorKind::PermissionDenied,
                    ) => {
                        index_refresh_unavailable = true;
                        let _ = std::fs::remove_file(cache.join("index.toml.part"));
                        crate::nawa_warn!("repository cache is not writable; using cached index");
                    }
                    Err(e) => {
                        index_refresh_unavailable = true;
                        let _ = std::fs::remove_file(cache.join("index.toml.part"));
                        crate::nawa_warn!(
                            "cannot reach {url} ({}); using cached index",
                            e.root_cause()
                        );
                    }
                }
            } else {
                crate::nawa_info!("fetching index from {index_url}");
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
                    crate::nawa_note!("remote repo publishes no index signature");
                }
                Err(e) if sig_path.exists() => {
                    let _ = std::fs::remove_file(&sig_part);
                    if !index_refresh_unavailable {
                        crate::nawa_warn!(
                            "cannot refresh repository signature; using cached signature ({})",
                            e.root_cause()
                        );
                    }
                }
                Err(e) => {
                    let _ = std::fs::remove_file(&sig_part);
                    if !index_refresh_unavailable {
                        crate::nawa_note_stderr!(
                            "repository signature unavailable ({})",
                            e.root_cause()
                        );
                    }
                }
            }
            repo_dir = cache;
            base_url = Some(url.clone());
        }
    }

    let index_path = repo_dir.join("index.toml");
    // Read once: parsing and signature verification must consume identical bytes.
    let index_bytes = std::fs::read(&index_path)
        .with_context(|| format!("read repository index {}", index_path.display()))?;
    let index = repo::load_index_bytes(&index_bytes)?;
    let index_trust = sign::verify_index_bytes(&index_bytes, &index_path, root, allow_unsigned)
        .map_err(|e| anyhow::anyhow!("repository index signature is INVALID or untrusted: {e:#}"))?;
    // Group definitions are supplemental metadata: usable only when the
    // index itself was authenticated by a key in the configured trust store.
    // The signed index pins the exact catalog bytes by SHA-256.
    let groups = match &index.groups {
        Some(entry) => {
            if !index_trust.is_trusted() {
                bail!("repository publishes package groups but its index is not authenticated by a trusted key");
            }
            let path = repo_dir.join(&entry.file);
            if let Some(url) = &base_url {
                fetch::download_cached(
                    &format!("{url}/{}", entry.file),
                    &path,
                    Some(&entry.sha256),
                ).with_context(|| format!("fetch signed group catalog {}", entry.file))?;
            } else if !path.is_file() {
                bail!("group catalog {} referenced by the signed index is missing", path.display());
            }
            fetch::verify_sha256(&path, &entry.sha256)?;
            let actual_size = std::fs::metadata(&path)
                .with_context(|| format!("stat group catalog {}", path.display()))?
                .len();
            if actual_size != entry.size {
                bail!("group catalog {} size mismatch: index says {} bytes, file has {} bytes",
                    entry.file, entry.size, actual_size);
            }
            let raw = std::fs::read_to_string(&path)
                .with_context(|| format!("read group catalog {}", path.display()))?;
            Some(crate::groups::GroupCatalog::from_toml(&raw)
                .with_context(|| format!("parse signed group catalog {}", path.display()))?)
        }
        None => None,
    };

    let distributed_locations = index.repository.as_ref()
        .map(|meta| meta.archive_location == repo::ARCHIVE_LOCATION_PER_PACKAGE_URL)
        .unwrap_or(false);
    if distributed_locations && !index_trust.is_trusted() {
        bail!("distributed schema-v3 repositories require a trusted signing key; refusing to use per-package URLs from an unauthenticated index");
    }
    let archive_auth = if index.repository.is_some() && index_trust.is_trusted() {
        crate::nawa_info!("repository schema v{}: package SHA-256 values authenticated by trusted index",
            index.repository.as_ref().map(|meta| meta.schema).unwrap_or(1));
        ArchiveAuth::SignedIndexSha256
    } else {
        if index.repository.is_some() {
            crate::nawa_note_stderr!("v2 index is not authenticated by a configured trusted key; using legacy per-archive signature policy");
        }
        ArchiveAuth::DetachedSignature
    };

    // Retain the exact proof bytes of the verification above so later
    // installs can persist them as artifact provenance (roadmap §4.5).
    let index_signature = std::fs::read(sign::sig_path_for(&index_path)).ok();
    let signer_public = match &index_trust {
        sign::IndexTrust::TrustedKey { key_name } => std::fs::read(
            sign::trust_dir(root).join(key_name),
        )
        .ok(),
        _ => None,
    };

    // Only an index authenticated by a configured trusted key advances the
    // newest-index marker used to prevent remote repository rollback.
    check_index_freshness(
        &repo_dir,
        &index,
        base_url.is_some(),
        force,
        index_trust.is_trusted(),
    )?;
    Ok(OpenRepo {
        dir: repo_dir,
        base_url,
        index,
        archive_auth,
        index_bytes,
        index_signature,
        signer_public,
        groups,
    })
}

/// Resolve an archive URL while keeping legacy base-URL repositories unchanged.
pub(crate) fn archive_download_url(
    base_url: &str,
    index: &repo::Index,
    file: &str,
    download_url: Option<&str>,
) -> Result<String> {
    let distributed = index.repository.as_ref()
        .map(|meta| meta.archive_location == repo::ARCHIVE_LOCATION_PER_PACKAGE_URL)
        .unwrap_or(false);
    if distributed {
        Ok(download_url
            .context("schema-v3 archive entry is missing download_url")?
            .to_string())
    } else {
        Ok(format!("{base_url}/{file}"))
    }
}

/// Make one index entry's archive available locally and fully verified:
/// downloaded into the cache (remote) or located in the repo dir (local),
/// then sha256 against the index, signature policy, and the `.PKGINFO`
/// must match what the index promised.
pub(crate) fn fetch_entry(
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
        None if open.index.repository.as_ref().map(|m| m.archive_location.as_str())
            == Some(repo::ARCHIVE_LOCATION_PER_PACKAGE_URL) =>
        {
            let source_url = archive_download_url(
                "", &open.index, &re.file, re.download_url.as_deref()
            )?;
            let cache = fetch::remote_cache_dir(root, &source_url);
            std::fs::create_dir_all(&cache)
                .with_context(|| format!("create archive cache {}", cache.display()))?;
            let dest = cache.join(&re.file);
            fetch::download_cached(&source_url, &dest, Some(&re.sha256))?;
            dest
        }
        None => {
            let p = repo_dir.join(&re.file);
            if !p.exists() {
                bail!("archive {} listed in the index is missing", p.display());
            }
            p
        }
        Some(url) => {
            let dest = repo_dir.join(&re.file);
            let source_url = archive_download_url(
                url, &open.index, &re.file, re.download_url.as_deref()
            )?;
            fetch::download_cached(&source_url, &dest, Some(&re.sha256))?;
            if open.archive_auth == ArchiveAuth::DetachedSignature {
                // Legacy repositories still require the per-archive sidecar.
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
                        if !sig_dest.exists() {
                            return Err(e).with_context(|| {
                                format!("could not fetch signature for {}", re.file)
                            });
                        }
                        crate::nawa_warn!(
                            "could not refresh signature for {} ({e:#}); using cached signature",
                            re.file
                        );
                    }
                }
            }
            dest
        }
    };
    // Integrity is always checked against the index, regardless of schema.
    fetch::verify_sha256(&src, &re.sha256)?;
    if re.size > 0 {
        let actual = std::fs::metadata(&src)
            .with_context(|| format!("stat repository archive {}", src.display()))?
            .len();
        if actual != re.size {
            bail!(
                "archive {} size mismatch: index says {} bytes, downloaded file has {} bytes",
                re.file, re.size, actual
            );
        }
    }
    // Only a trusted v2 index can replace per-archive signatures.
    if open.archive_auth == ArchiveAuth::DetachedSignature {
        sign::verify_for_install(&src, root, None, allow_unsigned)?;
    }
    // the archive must be the package/version the index promised
    // (blocks replaying an older signed archive as an "upgrade")
    check_archive_matches_index(&src, re, open.index.repository.is_some())?;
    Ok(src)
}

pub(crate) fn upgrade(
    root: &Path,
    repo_ref: &RepoRef,
    recipes: &Path,
    opts: &UpgradeOpts,
) -> Result<()> {
    let open = open_repo(root, repo_ref, opts.allow_unsigned, opts.force)?;
    let index = &open.index;

    // ---- compute the upgrade plan -------------------------------------------
    let db = Db::new(root);
    let installed_map: HashMap<String, crate::db::Entry> =
        db.installed()?.into_iter().map(|e| (e.name.clone(), e)).collect();
    let mut plan: Vec<(&repo::RepoEntry, String)> = Vec::new(); // (entry, new full version)
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
            plan.push((re, new));
        }
    }

    if plan.is_empty() {
        crate::nawa_info!("everything up to date ({} installed)", installed_map.len());
        return Ok(());
    }

    // dependency order when the recipe tree is available; packages that have
    // no recipe (third-party repo content) are appended alphabetically so a
    // partial tree never aborts the upgrade
    let names: Vec<String> = plan.iter().map(|(r, _)| r.name.clone()).collect();
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

    let pal = crate::color::Palette::auto(crate::color::Stream::Stdout);
    crate::nawa_info!("{} package(s) to upgrade:", plan.len());
    for name in &ordered {
        let (_, new) = plan.iter().find(|(r, _)| &r.name == name).unwrap();
        let cur = installed_map.get(name).map(|e| e.version_full()).unwrap_or_default();
        println!(
            "  {} {} {} {}",
            pal.name(&format!("{name:<24}")),
            pal.ver(&cur),
            pal.dim("->"),
            pal.ok(new),
        );
    }

    if opts.dry_run {
        crate::nawa_info!("dry run — no changes made");
        return Ok(());
    }

    // the plan is on the table; a real run needs an explicit go-ahead
    // (scripts pass --yes; a piped stdin without it is refused, not guessed)
    crate::confirm::confirm("nawa: proceed with the upgrade?", opts.yes)?;

    // ---- apply ----------------------------------------------------------------
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
    for name in &ordered {
        // a Ctrl+C that arrived during the previous package was deferred
        // until it was complete; stop before starting the next one
        if let Err(error) = crate::interrupt::check() {
            first_err = Some(error.into());
            break;
        }
        let (re, _) = plan
            .iter()
            .find(|(r, _)| &r.name == name)
            .expect("ordered name came from the plan");
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
        let outcome = fetch_entry(root, &open, re, opts.allow_unsigned).and_then(|src| {
            if open.archive_auth == ArchiveAuth::SignedIndexSha256 {
                install::install_archive_from_index_proof(
                    &src,
                    root,
                    opts.force,
                    &re.sha256,
                    "keep",
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
                    install::InstallOpts { force: opts.force, allow_unsigned: opts.allow_unsigned, key: None, reason: "keep", pinned_sha: Some(&re.sha256) },
                )
            }
        });
        match &outcome {
            Ok(entry) => {
                recorder.step_result(
                    step_no, name, crate::transaction::history::StepAction::Upgrade,
                    crate::transaction::history::StepStatus::Committed,
                    before.as_ref(), Some(entry), "repository", None, None,
                )?;
                done += 1;
            }
            Err(error) => {
                let current = db_here.entry(name)?;
                let retry_hook = install::hook_needs_retry(root, name).unwrap_or(false);
                let committed_hook_failed = crate::transaction::history::committed_hook_failed(
                    current.as_ref(), before.as_ref(), &target, retry_hook,
                );
                recorder.step_result(
                    step_no, name, crate::transaction::history::StepAction::Upgrade,
                    if committed_hook_failed {
                        crate::transaction::history::StepStatus::CommittedHookFailed
                    } else {
                        crate::transaction::history::StepStatus::Failed
                    },
                    before.as_ref(),
                    if committed_hook_failed { current.as_ref() } else { None },
                    "repository", Some(format!("{error:#}")),
                    committed_hook_failed.then(|| "failed".to_string()),
                )?;
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

#[allow(unused_imports)]
pub(crate) use verify::{check_archive_matches_index, check_index_freshness, require_trust_for_remote, validate_entry_filename};
#[allow(unused_imports)]
pub(crate) use order::order_with_fallback;

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
