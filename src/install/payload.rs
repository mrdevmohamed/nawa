//! Install payload: staging, file moves, archival caching.

use anyhow::{bail, Context, Result};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::artifact;
use crate::archive;
use crate::db::{Db, Entry};
use crate::install::InstallAuth;
use crate::install::hooks::{hook_status_path, run_tracked_hook, save_hooks};
use crate::install::journal::{recover_and_report, Tx};
use crate::interrupt;
use crate::sign;

pub(crate) fn install_archive_payload(
    archive_path: &Path,
    cache_name_source: &Path,
    root: &Path,
    force: bool,
    reason: crate::db::InstallReason,
    cache_detached_signature: bool,
    auth: InstallAuth,
) -> Result<Entry> {
    let db = Db::new(root);
    // Ctrl+C / SIGTERM / SIGHUP are deferred from here to the end of the
    // function: before the commit point they roll the install back, after it
    // the install simply finishes (see `interrupt`).
    let _interrupts = interrupt::Guard::new();
    // a previous nawa killed mid-install (SIGKILL, power cut) is repaired
    // before anything else looks at the files or the db
    recover_and_report(root);
    // Timestamps stored in the database are Unix SECONDS (the human formatter
    // interprets them that way); temporary directory names get their own
    // uniqueness from nanoseconds + pid, decoupled from install_date.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let uniq = format!(
        "{}-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
        std::process::id()
    );
    let tmp = root.join(".nawa").join(format!("tmp-{uniq}"));
    std::fs::create_dir_all(&tmp)?;
    // remove the staging dir on every exit path (errors included)
    struct TmpGuard(PathBuf);
    impl Drop for TmpGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let _tmp_guard = TmpGuard(tmp.clone());

    let extracted = archive::extract(archive_path, &tmp)?;
    interrupt::check()?;
    let info = archive::parse_pkginfo(&extracted.pkginfo)?;
    let Pkginfo { name, version, rel, arch, size, run } = Pkginfo::from_map(&info);

    // ---- current state of this package (if this is an upgrade/reinstall) ---
    let old = db.entry(&name)?;
    let old_files = match &old {
        Some(e) => db.files(&e.name)?,
        None => Vec::new(),
    };
    let is_upgrade = old.is_some();

    // ---- conflict check against the database ------------------------------
    let (staged_files, staged_hashes) = stage_tree(&tmp)?;
    let owners = db.file_owners()?;
    if !force {
        check_file_conflicts(&tmp, root, &name, &staged_files, &owners)?;
    }

    // ---- remember the previous version for rollback -------------------------
    if let Some(prev) = &old {
        save_undo(&db, root, prev)?;
    }
    let old_hashes = match &old {
        Some(e) => db.hashes(&e.name)?,
        None => std::collections::HashMap::new(),
    };

    // ---- keep a copy of the archive in the package cache --------------------
    cache_archive_named(archive_path, cache_name_source, root, cache_detached_signature)?;
    // ---- publish the verified bytes as a content-addressed object and keep
    // the authentication provenance (roadmap §4.4/§4.5). Failures here only
    // cost the history/undo evidence, not the install itself — but a digest
    // mismatch is always fatal (the bytes are NOT what the policy verified).
    let object_digest = record_artifact(
        root,
        archive_path,
        cache_name_source,
        &auth,
        &PackageIdentity { name: &name, version: &version, rel, arch: &arch },
    );
    if is_upgrade {
        crate::nawa_info!(
            "upgrading {} {} -> {}-{}-{}",
            name,
            old.as_ref().map(|e| e.version_full()).unwrap_or_default(),
            name,
            version,
            rel
        );
    }

    // ---- move staged tree into the root -------------------------------------
    let mut diverted: Vec<(String, String)> = Vec::new(); // (orig, diverted)
    let mut tx = Tx::new(root, root.join(".nawa").join(format!("bak-{uniq}")));
    // last clean exit before any file is touched
    interrupt::check()?;
    if let Err(e) = move_tree(&tmp, root, root, &old_hashes, &mut diverted, &mut tx) {
        tx.rollback();
        return Err(e);
    }
    let _ = std::fs::remove_dir_all(&tmp);
    for (orig, new) in &diverted {
        crate::nawa_note!("installed {new} (local {orig} changed — merge manually)");
    }

    // ---- delete files that disappeared between the two versions -------------
    let obsolete_dirs =
        retire_obsolete_files(&db, &mut tx, root, &name, &old_files, &staged_files, &owners)?;

    // last point where backing out is safe: the files are in place but the
    // db does not know them yet, and the journal can still undo all of it
    if let Err(e) = interrupt::check() {
        tx.rollback();
        return Err(e.into());
    }

    // ---- register in db (atomic: temp file + rename) -------------------------
    let reason = match reason {
        crate::db::InstallReason::Keep => old.as_ref().map(|o| o.reason.clone()).unwrap_or_else(|| "explicit".into()),
        r => r.as_str().to_string(),
    };
    let entry = Entry {
        name: name.clone(),
        version: version.clone(),
        rel,
        arch: arch.clone(),
        reason,
        run,
        install_date: now,
        size,
        artifact_sha256: object_digest.clone().unwrap_or_default(),
    };
    let hooks = match archive::read_hooks(archive_path) {
        Ok(h) => h,
        Err(e) => {
            tx.rollback();
            return Err(e);
        }
    };
    // the db entry is three files; keep a copy so a crash between them is
    // repaired together with the files
    if let Err(e) = tx.snapshot_db(&db.entry_dir(&name)?) {
        tx.rollback();
        return Err(e);
    }
    if let Err(e) = db.add(&entry, &staged_files, &staged_hashes) {
        // files and db must agree: put the previous state back
        tx.rollback();
        return Err(e);
    }
    // Hooks are part of the package's recoverable db state. Persist them
    // before commit so a failure rolls the payload and db back together.
    if let Err(e) = save_hooks(&db, &name, &hooks) {
        tx.rollback();
        return Err(e).context(format!("could not persist hooks for {name}"));
    }
    // point of no return: drop the undo journal, then tidy empty old dirs.
    // From here on interrupts no longer abort the install.
    tx.commit();
    for d in &obsolete_dirs {
        let _ = std::fs::remove_dir(d);
    }

    // ---- hooks ---------------------------------------------------------------
    let (label, script) = if is_upgrade {
        ("post_upgrade", hooks.post_install.as_str())
    } else {
        ("post_install", hooks.post_install.as_str())
    };
    if script.trim().is_empty() {
        let _ = std::fs::remove_file(hook_status_path(&db, &name));
    } else {
        run_tracked_hook(&db, &entry, label, script, root)?;
    }

    if is_upgrade {
        crate::nawa_success!("upgraded {} (previous version kept for rollback)", entry.version_full());
    } else {
        crate::nawa_success!("installed {} into {}", entry.version_full(), root.display());
    }
    Ok(entry)
}

/// The fields of a package's `.PKGINFO` the install needs.
struct Pkginfo {
    name: String,
    version: String,
    rel: u32,
    arch: String,
    size: u64,
    run: Vec<String>,
}

impl Pkginfo {
    fn from_map(info: &std::collections::HashMap<String, String>) -> Self {
        let name = info.get("pkgname").cloned().unwrap_or_default();
        let version = info.get("pkgver").cloned().unwrap_or_default();
        let rel: u32 = info.get("pkgrel").and_then(|v| v.parse().ok()).unwrap_or(1);
        let arch = info.get("arch").cloned().unwrap_or_default();
        let size: u64 = info.get("size").and_then(|v| v.parse().ok()).unwrap_or(0);
        let run: Vec<String> = info
            .get("rundeps")
            .map(|s| s.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect())
            .unwrap_or_default();
        Pkginfo { name, version, rel, arch, size, run }
    }
}

/// Identity of the package being installed, borrowed for provenance records.
struct PackageIdentity<'a> {
    name: &'a str,
    version: &'a str,
    rel: u32,
    arch: &'a str,
}

/// Fail when the staged tree would overwrite a file another package owns.
/// Directories merge silently, and a symlink that already points where this
/// package wants it is a no-op.
fn check_file_conflicts(
    tmp: &Path,
    root: &Path,
    name: &str,
    staged_files: &[String],
    owners: &std::collections::HashMap<String, String>,
) -> Result<()> {
    let mut conflicts: Vec<String> = Vec::new();
    for f in staged_files {
        // directories merge silently across packages — never a conflict
        // (every distro has several packages owning /etc or /usr)
        let p = tmp.join(f.trim_start_matches('/'));
        if std::fs::symlink_metadata(&p).map(|m| m.is_dir()).unwrap_or(false) {
            continue;
        }
        // a symlink that already points exactly where this package wants
        // it (the merged-usr /bin, /lib, ... links) overwrites nothing:
        // installing it is a no-op, whoever else lists the path
        if let (Ok(new_target), Ok(cur_target)) = (
            std::fs::read_link(&p),
            std::fs::read_link(root.join(f.trim_start_matches('/'))),
        ) {
            if new_target == cur_target {
                continue;
            }
        }
        if let Some(owner) = owners.get(f) {
            if owner != name {
                conflicts.push(format!("{f} (owned by {owner})"));
            }
        }
    }
    if !conflicts.is_empty() {
        let _ = std::fs::remove_dir_all(tmp);
        bail!(
            "file conflicts installing {}:\n  {}\n(use --force to override)",
            name,
            conflicts.iter().take(20).cloned().collect::<Vec<_>>().join("\n  ")
        );
    }
    Ok(())
}

/// Publish the verified archive as a content-addressed object and record its
/// authentication provenance (roadmap §4.4/§4.5). Failures here only cost
/// the history/undo evidence, not the install itself.
fn record_artifact(
    root: &Path,
    archive_path: &Path,
    cache_name_source: &Path,
    auth: &InstallAuth,
    id: &PackageIdentity<'_>,
) -> Option<String> {
    let (name, version, rel, arch) = (id.name, id.version, id.rel, id.arch);
    match artifact::store_verified_copy(root, archive_path, None) {
        Ok((_, digest)) => {
            let size = std::fs::metadata(archive_path).map(|m| m.len()).unwrap_or(0);
            let mut prov = artifact::Provenance::new(auth.kind(), &digest, size);
            prov.package = Some(artifact::PackageIdentity {
                name: name.to_string(),
                version: version.to_string(),
                rel,
                arch: arch.to_string(),
            });
            match auth {
                InstallAuth::SignedIndex { index_bytes, index_signature, signer_public, repo_id } => {
                    if let Err(e) = artifact::save_provenance(root, &prov) {
                        crate::nawa_warn!("could not record artifact provenance: {e:#}");
                    } else if let (Some(idx), Some(key)) = (index_bytes, signer_public) {
                        if let Err(e) = artifact::retain_repository_proof(
                            root, &digest, idx, index_signature.as_deref(), key,
                            repo_id.as_deref().unwrap_or("repository"), true,
                        ) {
                            crate::nawa_warn!("could not retain repository proof: {e:#}");
                        }
                    }
                }
                InstallAuth::Detached { trusted: true } => {
                    if let Err(e) = artifact::save_provenance(root, &prov) {
                        crate::nawa_warn!("could not record artifact provenance: {e:#}");
                    } else {
                        // retain the detached signature that verified these
                        // bytes, so a later rollback can re-verify it
                        let sig = sign::sig_path_for(cache_name_source);
                        if sig.is_file() {
                            let fingerprint = sign::read_sig_file(&sig)
                                .ok()
                                .map(|sf| artifact::key_fingerprint(
                                    &sf.embedded.to_bytes(),
                                ))
                                .unwrap_or_default();
                            let _ = artifact::retain_detached_signature(
                                root,
                                &digest,
                                &sig,
                                &fingerprint,
                                true,
                            );
                        }
                    }
                }
                _ => {
                    let _ = artifact::save_provenance(root, &prov);
                }
            }
            Some(digest)
        }
        Err(e) => {
            crate::nawa_warn!("could not store the archive as a content-addressed object: {e:#}");
            None
        }
    }
}

/// Move the files a previous version owned and the new one dropped into the
/// undo journal; returns the now-obsolete directories, removed after commit.
/// Never touches a path another installed package also lists.
fn retire_obsolete_files(
    db: &Db,
    tx: &mut Tx,
    root: &Path,
    name: &str,
    old_files: &[String],
    staged_files: &[String],
    owners: &std::collections::HashMap<String, String>,
) -> Result<Vec<PathBuf>> {
    let mut obsolete_dirs: Vec<PathBuf> = Vec::new();
    if !old_files.is_empty() {
        let new_set: HashSet<&String> = staged_files.iter().collect();
        // never delete a path another installed package also lists
        let shared = db.paths_claimed_by_others(name)?;
        let mut obsolete: Vec<&String> = old_files
            .iter()
            .filter(|f| !new_set.contains(*f))
            .filter(|f| !shared.contains(*f))
            .filter(|f| owners.get(*f).map(|o| o == name).unwrap_or(false))
            .collect();
        obsolete.sort_by_key(|f| std::cmp::Reverse(f.len()));
        for f in obsolete {
            let p = root.join(f.trim_start_matches('/'));
            match std::fs::symlink_metadata(&p) {
                // directories are removed after the commit (only if empty)
                Ok(md) if md.is_dir() => obsolete_dirs.push(p),
                // files are moved into the undo journal, not deleted yet
                Ok(_) => {
                    let _ = tx.remove(&p);
                }
                Err(_) => {}
            }
        }
    }
    Ok(obsolete_dirs)
}

/// Restore a package to the version recorded in its undo log
/// (`/var/lib/nawa/undo/<name>`), using the cached archive of that version.
/// Works after an upgrade (goes one version back — ping-pong capable) and
/// after a remove (reinstalls the removed version).
///
/// Roadmap §4.7: the cached archive is NOT trusted just because nawa
/// verified it once. When the undo record (or the installed entry it
/// describes) carries an artifact digest, the exact cached bytes are
/// re-verified against it, and the recorded authentication evidence decides
/// how the restore is proven:
///
/// * a signed-index object with a retained proof bundle is re-verified
///   against the bundle (index bytes + signature + signer key);
/// * a detached-signature object with a retained sidecar is re-verified
///   against that sidecar;
/// * legacy undo records without a digest fall back to the previous
///   behaviour (detached-signature policy where a usable signature exists).
///   This weaker legacy path is explicit and never reported as strong proof.
pub(crate) fn copy_atomic(src: &Path, tmp: &Path, dest: &Path, copy_ctx: &str, rename_ctx: &str) -> Result<()> {
    let res = (|| -> Result<()> {
        std::fs::copy(src, tmp).with_context(|| copy_ctx.to_string())?;
        std::fs::rename(tmp, dest).with_context(|| rename_ctx.to_string())?;
        Ok(())
    })();
    if res.is_err() {
        let _ = std::fs::remove_file(tmp);
    }
    res
}

/// Copy verified bytes into `<root>/var/cache/nawa` (atomically, as a full
/// copy) under the original archive filename. For legacy archives the
/// detached signature is kept next to it; a trusted v2 index is itself the
/// authenticator, so a stray detached signature is never imported into its
/// rollback cache.
pub(crate) fn cache_archive_named(
    archive_path: &Path,
    name_source: &Path,
    root: &Path,
    copy_detached_signature: bool,
) -> Result<PathBuf> {
    let dir = root.join("var/cache/nawa");
    std::fs::create_dir_all(&dir)?;
    let dest = dir.join(name_source.file_name().unwrap_or_default());
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp = dir.join(format!(
        ".archive-{}-{}-{stamp}.tmp",
        std::process::id(),
        dest.file_name().unwrap_or_default().to_string_lossy()
    ));
    copy_atomic(
        archive_path,
        &tmp,
        &dest,
        &format!("cache {}", archive_path.display()),
        &format!("activate cached archive {}", dest.display()),
    )?;

    let sig_dest = sign::sig_path_for(&dest);
    if copy_detached_signature {
        let sig = sign::sig_path_for(name_source);
        if sig.is_file() {
            let sig_tmp = sig_dest.with_file_name(format!(
                ".signature-{}-{stamp}.tmp",
                std::process::id()
            ));
            copy_atomic(
                &sig,
                &sig_tmp,
                &sig_dest,
                &format!("cache detached signature {}", sig.display()),
                &format!("activate cached signature {}", sig_dest.display()),
            )?;
        } else if sig_dest.exists() {
            std::fs::remove_file(&sig_dest)
                .with_context(|| format!("remove stale cached signature {}", sig_dest.display()))?;
        }
    } else if sig_dest.exists() {
        std::fs::remove_file(&sig_dest)
            .with_context(|| format!("remove untrusted cached signature {}", sig_dest.display()))?;
    }
    Ok(dest)
}


pub(crate) fn save_undo(db: &Db, root: &Path, prev: &Entry) -> Result<()> {
    let dir = root.join("var/lib/nawa/undo").join(&prev.name);
    std::fs::create_dir_all(&dir)?;
    let desc = toml::to_string_pretty(prev)?;
    // write+rename so an interrupted save can't corrupt the record
    let tmp = dir.join(".desc.tmp");
    std::fs::write(&tmp, desc)?;
    std::fs::rename(&tmp, dir.join("desc"))?;
    let files = db.files(&prev.name)?;
    let ftmp = dir.join(".files.tmp");
    std::fs::write(&ftmp, files.join("\n") + "\n")?;
    std::fs::rename(&ftmp, dir.join("files"))?;
    Ok(())
}

/// One walk over the staging tree: every file to install plus the
/// `/etc` hash baseline (config protection). Sorted, like both
/// former walks, so conflict order is unchanged.
pub(crate) fn stage_tree(staged: &Path) -> Result<(Vec<String>, std::collections::HashMap<String, String>)> {
    let mut files = Vec::new();
    let mut etc_hashes = std::collections::HashMap::new();
    for e in walkdir::WalkDir::new(staged).sort_by_file_name() {
        let e = e?;
        let rel = e.path().strip_prefix(staged)?;
        if rel.as_os_str().is_empty() {
            continue;
        }
        let pkgpath = format!("/{}", rel.display());
        files.push(pkgpath.clone());
        if rel.components().next().is_some_and(|c| c.as_os_str() == "etc") {
            if let Some(h) = path_hash(e.path()) {
                etc_hashes.insert(pkgpath, h);
            }
        }
    }
    Ok((files, etc_hashes))
}

/// Re-export of the shared containment check (see `src/pathutil.rs`).
pub(crate) use crate::pathutil::ensure_inside_root;

/// Recursively move `src` into `dst`, merging directories.
///
/// Config protection (pacman-style): a file under `<root>/etc` that would
/// change content is only overwritten in place when the on-disk copy still
/// matches the hash recorded at install time (i.e. the admin never touched
/// it). Locally modified configs are preserved and the new version lands
/// next to them as `<path>.nawa-new` (recorded in `diverted`).
pub(crate) fn move_tree(
    src: &Path,
    dst: &Path,
    root: &Path,
    old_hashes: &std::collections::HashMap<String, String>,
    diverted: &mut Vec<(String, String)>,
    tx: &mut Tx,
) -> Result<()> {
    for entry in std::fs::read_dir(src).with_context(|| format!("read {}", src.display()))? {
        let entry = entry?;
        // between two files the transaction is consistent and journaled, so
        // this is a safe place to give up (the caller rolls back)
        interrupt::check()?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        let is_dir = from.is_dir() && !std::fs::symlink_metadata(&from)?.is_symlink();
        if is_dir {
            // checked BEFORE create_dir_all, which would happily create
            // directories through a symlink pointing outside the root
            ensure_inside_root(root, &to)?;
            std::fs::create_dir_all(&to)?;
            // carry the staged directory's mode over: create_dir_all applies
            // the umask, which would strip e.g. the sticky bit from /tmp
            let md = std::fs::symlink_metadata(&from)?;
            let _ = std::fs::set_permissions(&to, md.permissions());
            move_tree(&from, &to, root, old_hashes, diverted, tx)?;
        } else {
            // a runsvdir link the admin disabled stays disabled across upgrades
            if crate::service::keep_disabled(root, &to) {
                continue;
            }
            if let Some(parent) = to.parent() {
                std::fs::create_dir_all(parent)?;
            }
            if contents_differ(&from, &to) && is_under_etc(&to, root) {
                let pkgpath = format!("/{}", to.strip_prefix(root)?.display());
                let admin_modified = match old_hashes.get(&pkgpath) {
                    // baseline exists: divert only if the disk copy drifted
                    Some(h) => path_hash(&to).as_deref() != Some(h.as_str()),
                    // no baseline (legacy db entry): be safe, divert
                    None => true,
                };
                if admin_modified {
                    let mut alt = to.clone().into_os_string();
                    alt.push(".nawa-new");
                    tx.place(&from, Path::new(&alt))?;
                    diverted.push((to.to_string_lossy().to_string(), alt.to_string_lossy().to_string()));
                } else {
                    tx.place(&from, &to)?;
                }
            } else {
                tx.place(&from, &to)?;
            }
        }
    }
    Ok(())
}

pub(crate) fn is_under_etc(p: &Path, root: &Path) -> bool {
    // p is an absolute path inside the target root; compare against the
    // root prefix so a root dir literally named ".../etc/..." can't match
    match p.strip_prefix(root) {
        Ok(rel) => rel
            .components()
            .next()
            .map(|c| c.as_os_str() == "etc")
            .unwrap_or(false),
        Err(_) => false,
    }
}

/// sha256 of a file's bytes; for symlinks, of the target path string.
pub(crate) fn path_hash(p: &Path) -> Option<String> {
    use sha2::{Digest, Sha256};
    let md = std::fs::symlink_metadata(p).ok()?;
    let mut h = Sha256::new();
    if md.is_symlink() {
        let t = std::fs::read_link(p).ok()?;
        h.update(t.to_string_lossy().as_bytes());
    } else if md.is_file() {
        let bytes = std::fs::read(p).ok()?;
        h.update(&bytes);
    } else {
        return None;
    }
    Some(hex::encode(h.finalize()))
}

pub(crate) fn contents_differ(staged: &Path, dest: &Path) -> bool {
    let (ma, mb) = match (std::fs::symlink_metadata(staged), std::fs::symlink_metadata(dest)) {
        (Ok(a), Ok(b)) => (a, b),
        // destination missing -> nothing to protect
        _ => return false,
    };
    match (ma.is_symlink(), mb.is_symlink()) {
        (true, true) => std::fs::read_link(staged).ok() != std::fs::read_link(dest).ok(),
        (true, false) | (false, true) => true, // type change
        (false, false) => {
            if ma.is_dir() || mb.is_dir() {
                true // dir/file flip — cannot compare
            } else {
                match (std::fs::read(staged), std::fs::read(dest)) {
                    (Ok(a), Ok(b)) => a != b,
                    _ => true, // unreadable -> be safe, divert
                }
            }
        }
    }
}

