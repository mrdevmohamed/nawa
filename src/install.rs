//! Install / remove / rollback operations against a target root.
//!
//! Atomicity model:
//! * every archive is extracted into a hidden staging dir, verified against
//!   the local database, then moved into place — a failed install leaves the
//!   previous system state untouched
//! * database writes go through temp-file + rename (crash-safe)
//! * replaced packages leave an undo record (`/var/lib/nawa/undo/<name>`) and
//!   their archive in the package cache (`/var/cache/nawa`) so `nawa rollback`
//!   can restore the previous version
//! * files under /etc that would change content are installed as
//!   `<path>.nawa-new` instead of overwriting local modifications

use anyhow::{bail, Context, Result};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::archive;
use crate::db::{Db, Entry};
use crate::interrupt;
use crate::objects;
use crate::sign;

/// Install a `.nawa.tar.zst` archive into `root`.
///
/// * `force`            — overwrite files owned by other packages
/// * `allow_unsigned`   — accept unsigned archives when trust keys exist
/// * `key`              — verify signature against this pubkey instead of the trust store
/// * `reason`           — "explicit" | "dependency" | "keep" (preserve old reason)
pub fn install_archive(
    archive_path: &Path,
    root: &Path,
    force: bool,
    allow_unsigned: bool,
    key: Option<&Path>,
    reason: &str,
) -> Result<Entry> {
    install_archive_pinned(archive_path, root, force, allow_unsigned, key, reason, None)
}

/// [`install_archive`] for an archive that a repository index already
/// described. With `expected_sha256` the snapshot must hash to the index
/// digest, so the bytes whose detached signature is checked are provably the
/// bytes the index entry (and `.PKGINFO` check) was validated against.
pub fn install_archive_pinned(
    archive_path: &Path,
    root: &Path,
    force: bool,
    allow_unsigned: bool,
    key: Option<&Path>,
    reason: &str,
    expected_sha256: Option<&str>,
) -> Result<Entry> {
    let snapshot_dir = root.join(".nawa");
    let snapshot = crate::fetch::snapshot_file(archive_path, expected_sha256, &snapshot_dir)?;
    // the detached signature to verify (and later to preserve as evidence)
    let sig_path = sign::sig_path_for(archive_path);
    sign::verify_for_install_with_sig(
        snapshot.path(),
        &sig_path,
        root,
        key,
        allow_unsigned,
    )?;
    let authenticated = sig_path.is_file();
    let evidence = objects::InstallEvidence {
        origin: objects::ArtifactOrigin::StandaloneArchive,
        authentication: if authenticated {
            objects::AUTH_DETACHED
        } else {
            objects::AUTH_UNSIGNED
        },
        detached_sig: authenticated.then(|| sig_path.clone()),
        index: None,
        index_sig: None,
        index_trusted: false,
    };
    install_archive_payload(
        snapshot.path(),
        archive_path,
        root,
        force,
        reason,
        true,
        expected_sha256,
        &evidence,
    )
}

/// The bytes/signature of the signed repository index that authenticated an
/// archive installation (`nawa-v1:sha256` over the exact index bytes).
/// Preserved with the artifact so a later restore can re-verify the same
/// evidence instead of trusting "it was fine back then".
#[derive(Debug, Clone)]
pub struct RepoProof {
    /// configured repository identifier (URL/path today, a stable id later)
    pub repo_id: String,
    /// index.toml bytes verified for this installation
    pub index_path: PathBuf,
    /// index.toml.sig when the repository publishes one
    pub index_sig_path: Option<PathBuf>,
    /// whether the index signature matched a key in the target root's
    /// trust store at install time (caller computed this when opening the
    /// repository, before any package plan exists)
    pub index_trusted: bool,
}

impl RepoProof {
    pub fn empty() -> Self {
        RepoProof { repo_id: String::new(), index_path: PathBuf::new(), index_sig_path: None, index_trusted: false }
    }

    pub fn is_empty(&self) -> bool {
        self.index_path.as_os_str().is_empty()
    }
}

/// [`install_archive`] for the output of a local source build (`build
/// --install` / the plan-runner build path): the produced archive digest is
/// recorded with a source-build provenance category. A local build is not
/// automatically signed merely because it has a digest — when a detached
/// signature exists it is verified and preserved, otherwise the artifact is
/// recorded exactly as what it is: unsigned.
pub fn install_built_archive(
    archive_path: &Path,
    root: &Path,
    force: bool,
    allow_unsigned: bool,
    key: Option<&Path>,
    reason: &str,
) -> Result<Entry> {
    let snapshot_dir = root.join(".nawa");
    let snapshot = crate::fetch::snapshot_file(archive_path, None, &snapshot_dir)?;
    let sig_path = sign::sig_path_for(archive_path);
    sign::verify_for_install_with_sig(snapshot.path(), &sig_path, root, key, allow_unsigned)?;
    let authenticated = sig_path.is_file();
    let evidence = objects::InstallEvidence {
        origin: objects::ArtifactOrigin::SourceBuild,
        authentication: if authenticated {
            objects::AUTH_DETACHED
        } else {
            objects::AUTH_UNSIGNED
        },
        detached_sig: authenticated.then(|| sig_path.clone()),
        index: None,
        index_sig: None,
        index_trusted: false,
    };
    install_archive_payload(
        snapshot.path(),
        archive_path,
        root,
        force,
        reason,
        true,
        None,
        &evidence,
    )
}

/// Install an archive authenticated by a trusted v2 repository index.
///
/// The expected digest comes from the already-verified signed index. Recheck
/// it immediately before extraction so the file cannot silently change
/// between repository fetch/validation and the install operation. This path
/// is intentionally separate from standalone archive installation.
pub fn install_archive_from_index_proof(
    archive_path: &Path,
    root: &Path,
    force: bool,
    expected_sha256: &str,
    reason: &str,
    proof: &RepoProof,
) -> Result<Entry> {
    let snapshot_dir = root.join(".nawa");
    let snapshot = crate::fetch::snapshot_file(archive_path, Some(expected_sha256), &snapshot_dir)?;
    let evidence = if proof.is_empty() {
        objects::InstallEvidence {
            origin: objects::ArtifactOrigin::Repository(String::new()),
            authentication: objects::AUTH_UNSIGNED,
            detached_sig: None,
            index: None,
            index_sig: None,
            index_trusted: false,
        }
    } else {
        objects::InstallEvidence {
            origin: objects::ArtifactOrigin::Repository(proof.repo_id.clone()),
            authentication: objects::AUTH_SIGNED_INDEX,
            detached_sig: None,
            index: Some(proof.index_path.clone()),
            index_sig: proof.index_sig_path.clone(),
            index_trusted: proof.index_trusted,
        }
    };
    install_archive_payload(
        snapshot.path(),
        archive_path,
        root,
        force,
        reason,
        false,
        Some(expected_sha256),
        &evidence,
    )
}

fn install_archive_payload(
    archive_path: &Path,
    cache_name_source: &Path,
    root: &Path,
    force: bool,
    reason: &str,
    cache_detached_signature: bool,
    expected_digest: Option<&str>,
    evidence: &objects::InstallEvidence,
) -> Result<Entry> {
    let db = Db::new(root);
    // Ctrl+C / SIGTERM / SIGHUP are deferred from here to the end of the
    // function: before the commit point they roll the install back, after it
    // the install simply finishes (see `interrupt`).
    let _interrupts = interrupt::Guard::new();
    // a previous nawa killed mid-install (SIGKILL, power cut) is repaired
    // before anything else looks at the files or the db
    recover_and_report(root);
    // The digest of the exact bytes we are installing is computed FIRST: it
    // becomes the immutable artifact identity recorded in the db and the
    // object store, and a caller-supplied expectation must match it before
    // anything is extracted.
    let artifact_digest = crate::fetch::sha256_of_file(archive_path)?;
    if let Some(expected) = expected_digest {
        if artifact_digest != expected.to_lowercase() {
            bail!(
                "archive digest changed: verified {} but installing {}",
                expected,
                artifact_digest
            );
        }
    }
    // Decouple identity from the install date: `stamp` is the recorded
    // install_date (Unix **seconds**, the unit every viewer uses), while
    // `tmp_id` only derives unique temporary directory names and must never
    // be mistaken for a timestamp a formatter will interpret.
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let tmp_id = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0_u128);
    let tmp = root.join(".nawa").join(format!("tmp-{tmp_id}"));
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
    let name = info.get("pkgname").cloned().unwrap_or_default();
    let version = info.get("pkgver").cloned().unwrap_or_default();
    let rel: u32 = info.get("pkgrel").and_then(|v| v.parse().ok()).unwrap_or(1);
    let arch = info.get("arch").cloned().unwrap_or_default();
    let size: u64 = info.get("size").and_then(|v| v.parse().ok()).unwrap_or(0);
    let run: Vec<String> = info
        .get("rundeps")
        .map(|s| s.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect())
        .unwrap_or_default();

    // ---- current state of this package (if this is an upgrade/reinstall) ---
    let old = db.entry(&name)?;
    let old_files = match &old {
        Some(e) => db.files(&e.name)?,
        None => Vec::new(),
    };
    let is_upgrade = old.is_some();

    // ---- conflict check against the database ------------------------------
    let staged_files = list_staged_files(&tmp)?;
    let owners = db.file_owners()?;
    if !force {
        let mut conflicts: Vec<String> = Vec::new();
        for f in &staged_files {
            // directories merge silently across packages — never a conflict
            // (every distro has several packages owning /etc or /usr)
            let p = tmp.join(f.trim_start_matches('/'));
            if std::fs::symlink_metadata(&p).map(|m| m.is_dir()).unwrap_or(false) {
                continue;
            }
            if let Some(owner) = owners.get(f) {
                if owner != &name {
                    conflicts.push(format!("{f} (owned by {owner})"));
                }
            }
        }
        if !conflicts.is_empty() {
            let _ = std::fs::remove_dir_all(&tmp);
            bail!(
                "file conflicts installing {}:\n  {}\n(use --force to override)",
                name,
                conflicts.iter().take(20).cloned().collect::<Vec<_>>().join("\n  ")
            );
        }
    }

    // ---- hash the staged /etc files (baseline for config protection) ---------
    let staged_hashes = hash_staged_etc(&tmp)?;

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
    if is_upgrade {
        println!(
            "nawa: upgrading {} {} -> {}-{}-{}",
            name,
            old.as_ref().map(|e| e.version_full()).unwrap_or_default(),
            name,
            version,
            rel
        );
    }

    // ---- move staged tree into the root -------------------------------------
    let mut diverted: Vec<(String, String)> = Vec::new(); // (orig, diverted)
    let mut tx = Tx::new(root, root.join(".nawa").join(format!("bak-{tmp_id}")));
    // last clean exit before any file is touched
    interrupt::check()?;
    if let Err(e) = move_tree(&tmp, root, root, &old_hashes, &mut diverted, &mut tx) {
        tx.rollback();
        return Err(e);
    }
    let _ = std::fs::remove_dir_all(&tmp);
    for (orig, new) in &diverted {
        println!("nawa: NOTE: installed {new} (local {orig} changed — merge manually)");
    }

    // ---- delete files that disappeared between the two versions -------------
    let mut obsolete_dirs: Vec<PathBuf> = Vec::new();
    if !old_files.is_empty() {
        let new_set: HashSet<&String> = staged_files.iter().collect();
        let mut obsolete: Vec<&String> = old_files
            .iter()
            .filter(|f| !new_set.contains(*f))
            .filter(|f| owners.get(*f).map(|o| o == &name).unwrap_or(false))
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

    // last point where backing out is safe: the files are in place but the
    // db does not know them yet, and the journal can still undo all of it
    if let Err(e) = interrupt::check() {
        tx.rollback();
        return Err(e.into());
    }

    // ---- register in db (atomic: temp file + rename) -------------------------
    let reason = match reason {
        "keep" => old.as_ref().map(|o| o.reason.clone()).unwrap_or_else(|| "explicit".into()),
        r => r.to_string(),
    };
    let entry = Entry {
        name: name.clone(),
        version: version.clone(),
        rel,
        arch: arch.clone(),
        reason,
        run,
        install_date: stamp,
        size,
        // immutable artifact identity: these exact bytes, verifiable forever
        artifact_sha256: Some(artifact_digest.clone()),
        artifact_record: Some(format!("sha256:{artifact_digest}")),
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

    // ---- persist the artifact object + provenance ----------------------------
    // The package is committed; the object store is a (best-effort) cache:
    // a failure here is warned about, never undoes the install, and the db
    // keeps the digest so a later restore can locate/re-verify the bytes.
    let archive_size = std::fs::metadata(archive_path).map(|m| m.len()).unwrap_or(0);
    if let Err(e) = publish_artifact_object(
        root,
        archive_path,
        &artifact_digest,
        archive_size,
        &name,
        &version,
        rel,
        &arch,
        evidence,
        stamp,
    ) {
        eprintln!(
            "nawa: warning: could not record artifact {} in the object store: {e:#}",
            artifact_digest
        );
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
        println!("nawa: upgraded {} (previous version kept for rollback)", entry.version_full());
    } else {
        println!("nawa: installed {} into {}", entry.version_full(), root.display());
    }
    Ok(entry)
}

/// Restore a package to the version recorded in its undo log
/// (`/var/lib/nawa/undo/<name>`).
///
/// Restore policy (content-addressed artifacts, Phase 0):
///
/// 1. The previous archive bytes are resolved by their recorded artifact
///    digest — from the immutable object store, or the legacy version-named
///    cache file which must then match that digest.
/// 2. The exact bytes are re-verified (sha256) before anything is installed.
///    A cached archive is never trusted just because nawa verified it once.
/// 3. Authentication is re-checked according to the provenance recorded at
///    install time: signed-index proof (saved index + signature, re-verified
///    against the current trust store), detached signature, or an explicit
///    `unsigned` record. Records without provenance (pre-object-store
///    installs) fall back to the classic detached-signature policy and are
///    clearly reported as not carrying strong historical proof.
/// 4. The archive's .PKGINFO identity must match the recorded package
///    identity before installation.
/// 5. The normal staged installer does the restore; the original install
///    reason is preserved (`keep`).
///
/// Restoration is refused when the required artifact or proof is missing,
/// corrupt (digest mismatch), or when the recorded signature evidence fails
/// to verify. An invalid signature is always an error, never converted.
pub fn rollback(name: &str, root: &Path) -> Result<()> {
    let db = Db::new(root);
    crate::db::check_name(name)?;
    let undo_dir = root.join("var/lib/nawa/undo").join(name);
    let desc_path = undo_dir.join("desc");
    if !desc_path.exists() {
        bail!("no undo record for `{name}` — nothing to roll back");
    }
    let prev: Entry = toml::from_str(&std::fs::read_to_string(&desc_path)?)
        .context("corrupt undo record")?;

    match db.entry(name)? {
        Some(cur) if cur.version_full() == prev.version_full() => {
            bail!("`{name}` is already at {} — nothing to roll back", cur.version_full());
        }
        _ => {}
    }

    // legacy version-named cache path: name derived from the package identity
    let legacy_archive = root
        .join("var/cache/nawa")
        .join(format!("{}.nawa.tar.zst", prev.version_full()));
    if !legacy_archive.exists() && prev.artifact_sha256.is_none() {
        bail!(
            "cached archive {} is missing — cannot roll back",
            legacy_archive.display()
        );
    }

    let printed_current = db
        .entry(name)?
        .map(|e| e.version_full())
        .unwrap_or_else(|| "(removed)".into());
    println!(
        "nawa: rolling back {} -> {}",
        printed_current,
        prev.version_full()
    );

    // ---- resolve + verify the artifact to restore ---------------------------
    enum Restorable {
        /// digest-verified bytes eligible to restore. `stored` carries the
        /// object-store evidence (sidecars keyed by digest, which is valid
        /// for any copy of the same bytes); it is `None` when the bytes came
        /// from the legacy cache because the object store lost them.
        Verified {
            archive: PathBuf,
            digest: String,
            stored: Option<objects::StoredObject>,
        },
        /// pre-object-store install: the legacy cache file only
        Legacy { archive: PathBuf },
    }
    let restorable = match prev.artifact_sha256.as_deref() {
        Some(recorded) => {
            let recorded = objects::validate_hex_digest(recorded, "rollback artifact digest")?;
            match objects::open_verified_object(root, recorded) {
                Ok(stored) => {
                    let archive = stored.archive.clone();
                    Restorable::Verified {
                        archive,
                        digest: stored.digest.clone(),
                        stored: Some(stored),
                    }
                }
                Err(e) => {
                    // the object store cannot serve these bytes (pruned or
                    // corrupt); the legacy version cache may still hold the
                    // identical bytes — acceptable only on an exact digest
                    // match, anything else is refused
                    eprintln!("nawa: note: {e:#}");
                    if !legacy_archive.is_file() {
                        bail!(
                            "artifact {recorded} cannot be restored: the object store cannot serve it (corrupt or pruned) and no legacy cache archive exists at {}",
                            legacy_archive.display()
                        );
                    }
                    let actual = crate::fetch::sha256_of_file(&legacy_archive)?;
                    if actual != recorded {
                        bail!(
                            "cached archive {} does not match the recorded artifact digest: corrupt (found {actual}) — refusing to restore",
                            legacy_archive.display()
                        );
                    }
                    Restorable::Verified {
                        archive: legacy_archive.clone(),
                        digest: recorded.to_string(),
                        stored: None,
                    }
                }
            }
        }
        None => Restorable::Legacy { archive: legacy_archive.clone() },
    };

    match restorable {
        Restorable::Verified { archive, digest, stored } => {
            // identity must match what the undo record promised, before any
            // file is touched
            let record = stored
                .as_ref()
                .and_then(|o| o.record.clone())
                .or_else(|| objects::read_record(root, &digest));
            if let Some(rec) = &record {
                if rec.package.name != prev.name
                    || rec.artifact_sha256.to_lowercase() != digest
                {
                    bail!(
                        "provenance record for {digest} describes package {} — refusing to use it for `{}`",
                        rec.package.name,
                        prev.name
                    );
                }
                match rec.authentication.as_str() {
                    objects::AUTH_SIGNED_INDEX => {
                        let (sobj, srec) = stored.as_ref().zip(stored.as_ref().and_then(|o| o.record.as_ref()))
                            .ok_or_else(|| anyhow::anyhow!(
                                "signed-index proof for {digest} is incomplete — cannot restore this artifact"
                            ))?;
                        verify_signed_index_restoration(root, &digest, sobj, srec)?;
                    }
                    objects::AUTH_DETACHED => {
                        verify_detached_restoration(
                            root,
                            &digest,
                            &archive,
                            &legacy_archive,
                            stored.as_ref(),
                        )?
                    }
                    objects::AUTH_UNSIGNED => eprintln!(
                        "nawa: note: restoring artifact recorded as UNSIGNED (source build or bootstrap install); it stays unsigned — no authentication evidence exists"
                    ),
                    other => bail!(
                        "unsupported authentication category in the provenance record: {other:?}"
                    ),
                }
            } else {
                // legacy record without object-store provenance: keep the
                // classic signature policy (working where a signature
                // exists), but say loudly that no historical proof is claimed
                eprintln!(
                    "nawa: note: no artifact provenance record for {} — restoring via the legacy signature policy (no strong historical proof)",
                    prev.version_full()
                );
            }
            install_archive_pinned(&archive, root, false, true, None, "keep", Some(&digest))?;
        }
        Restorable::Legacy { archive } => {
            eprintln!(
                "nawa: note: legacy rollback without artifact identity — trust rests with the cached archive only"
            );
            // legacy behavior kept deliberately: this system accepted these
            // bytes when they were installed; rollback stays possible there
            install_archive(&archive, root, false, true, None, "keep")?;
        }
    }
    Ok(())
}

/// Re-verify the signed-index proof of a stored artifact against the
/// CURRENT trust store. The preserved index bytes must hash to the index
/// digest in the provenance record, and the preserved index signature must
/// verify against a currently trusted key. A key removed from the trust
/// store is never substituted by an unrelated key or by the embedded-key
/// fallback — restoring signed-index artifacts requires the store to still
/// be trusted today.
fn verify_signed_index_restoration(
    root: &Path,
    digest: &str,
    stored: &objects::StoredObject,
    record: &objects::ArtifactRecord,
) -> Result<()> {
    use sha2::Digest;
    let index_path = stored.index.clone().ok_or_else(|| {
        anyhow::anyhow!(
            "signed-index proof for {digest} has no preserved index bytes — cannot restore this artifact"
        )
    })?;
    let index_bytes = std::fs::read(&index_path)?;
    // the record's index digest pins the exact evidence bytes
    if let Some(repo) = &record.repository {
        if let Some(recorded) = repo.index_sha256.as_deref() {
            let actual_index = hex::encode(sha2::Sha256::digest(&index_bytes));
            if recorded != actual_index {
                bail!(
                    "provenance mismatch: recorded index digest {recorded}, saved index bytes hash to {actual_index}"
                );
            }
        }
    }
    let sig_path = stored.index_sig.clone().ok_or_else(|| {
        anyhow::anyhow!(
            "signed-index proof for {digest} has no preserved signature — cannot restore"
        )
    })?;
    let sf = sign::read_sig_file(&sig_path)?;
    let trusted = sign::trusted_keys(root)?;
    if trusted.is_empty() {
        bail!(
            "signed-index proof for {digest} cannot be re-verified: no trusted keys are configured in {} — refusing to restore",
            sign::trust_dir(root).display()
        );
    }
    for (_key_name, vk) in trusted {
        if sign::verify_bytes(&index_bytes, &vk, &sf.sig).is_ok() {
            return Ok(());
        }
    }
    bail!(
        "historical signed-index proof for {digest} does not verify against the current trust store — refusing to restore (a key may have been removed from {}; restoration follows the current trust policy, not the install-time one)",
        sign::trust_dir(root).display()
    )
}

/// Re-verify the detached-signature evidence of a stored artifact. Authent-
/// icity is over the hash (`nawa-v1:sha256:<hex>`), so the preserved
/// sidecar signature is valid for any copy of the same bytes; when the
/// object store holds no sidecar but we restore the legacy cache file, the
/// signature kept beside the cache file is the other half of the evidence.
/// The artifact digest itself is re-checked by the installed path.
fn verify_detached_restoration(
    root: &Path,
    digest: &str,
    archive: &Path,
    legacy_archive: &Path,
    stored: Option<&objects::StoredObject>,
) -> Result<()> {
    let sig_for_archive = match stored.and_then(|o| o.sig.clone()) {
        Some(saved) => saved,
        None if archive == legacy_archive => sign::sig_path_for(archive),
        None => bail!(
            "detached-signature evidence for {digest} is missing — cannot restore this artifact"
        ),
    };
    if !sig_for_archive.is_file() {
        bail!(
            "recorded authentication `{}` for {digest} needs a signature, but none is stored — cannot restore",
            objects::AUTH_DETACHED
        );
    }
    sign::verify_for_install_with_sig(archive, &sig_for_archive, root, None, false)
}

#[allow(dead_code)] // convenience wrapper over remove_with
pub fn remove(name: &str, root: &Path) -> Result<()> {
    remove_with(name, root, false)
}

/// Remove a package. Without `force`, refuse when another installed package
/// lists it in its run-time dependencies.
///
/// The removal is journaled like the install: files move into a hidden
/// `.nawa/rm-<stamp>/` dir first (deepest first) and are restored if the db
/// update fails; the db entry is only dropped once every file has been moved.
/// A failure to move any file aborts the removal instead of silently
/// leaving a half-removed package behind.
pub fn remove_with(name: &str, root: &Path, force: bool) -> Result<()> {
    let db = Db::new(root);
    let _interrupts = interrupt::Guard::new();
    recover_and_report(root);
    let entry = match db.entry(name)? {
        Some(e) => e,
        None => bail!("`{name}` is not installed"),
    };

    if !force {
        let dependents: Vec<String> = db
            .installed()?
            .into_iter()
            .filter(|e| e.name != name && e.run.iter().any(|r| r == name))
            .map(|e| e.name)
            .collect();
        if !dependents.is_empty() {
            bail!(
                "cannot remove `{name}`: required by {}\n(use --force to remove anyway)",
                dependents.join(", ")
            );
        }
    }

    // undo record so `nawa rollback` can reinstall this exact version
    save_undo(&db, root, &entry)?;

    let hooks = load_hooks(&db, name);
    run_hook("pre_remove", &hooks.pre_remove, root)?;

    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut files = db.files(name)?;
    files.sort_by_key(|f| std::cmp::Reverse(f.len()));
    let mut tx = Tx::new(root, root.join(".nawa").join(format!("rm-{stamp}")));
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut move_err: Option<anyhow::Error> = None;
    for f in &files {
        if let Err(e) = interrupt::check() {
            move_err = Some(e.into());
            break;
        }
        let p = root.join(f.trim_start_matches('/'));
        match std::fs::symlink_metadata(&p) {
            // directories are removed after the commit (only if empty)
            Ok(md) if md.is_dir() => dirs.push(p),
            // a reversible delete: the file moves into the rm- journal
            Ok(_) => {
                if let Err(e) = tx.remove(&p) {
                    move_err = Some(e);
                    break;
                }
            }
            Err(_) => {} // already gone — nothing to undo for it
        }
    }
    if let Some(e) = move_err {
        tx.rollback();
        return Err(e);
    }
    if let Err(e) = interrupt::check() {
        tx.rollback();
        return Err(e.into());
    }
    if let Err(e) = tx.snapshot_db(&db.entry_dir(name)?) {
        tx.rollback();
        return Err(e);
    }
    if let Err(e) = db.remove(name) {
        // files and db must agree: put everything back
        tx.rollback();
        return Err(e);
    }
    tx.commit();
    for d in &dirs {
        let _ = std::fs::remove_dir(d);
    }

    run_hook("post_remove", &hooks.post_remove, root)?;
    println!(
        "nawa: removed {} from {} (rollback available via cached archive)",
        entry.version_full(),
        root.display()
    );
    Ok(())
}

/// Copy verified bytes into `<root>/var/cache/nawa` (atomically, as a full
/// copy) under the original archive filename. For legacy archives the
/// detached signature is kept next to it; a trusted v2 index is itself the
/// authenticator, so a stray detached signature is never imported into its
/// rollback cache.
fn cache_archive_named(
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
    let cache_result = (|| -> Result<()> {
        std::fs::copy(archive_path, &tmp)
            .with_context(|| format!("cache {}", archive_path.display()))?;
        std::fs::rename(&tmp, &dest)
            .with_context(|| format!("activate cached archive {}", dest.display()))?;
        Ok(())
    })();
    if cache_result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    cache_result?;

    let sig_dest = sign::sig_path_for(&dest);
    if copy_detached_signature {
        let sig = sign::sig_path_for(name_source);
        if sig.is_file() {
            let sig_tmp = sig_dest.with_file_name(format!(
                ".signature-{}-{stamp}.tmp",
                std::process::id()
            ));
            let sig_result = (|| -> Result<()> {
                std::fs::copy(&sig, &sig_tmp)
                    .with_context(|| format!("cache detached signature {}", sig.display()))?;
                std::fs::rename(&sig_tmp, &sig_dest)
                    .with_context(|| format!("activate cached signature {}", sig_dest.display()))?;
                Ok(())
            })();
            if sig_result.is_err() {
                let _ = std::fs::remove_file(&sig_tmp);
            }
            sig_result?;
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

/// Identity of the key inside a `.sig` file: sha256 over the public-key hex
/// bytes — a stable fingerprint of the exact key that verified the artifact
/// (never the key itself, and never a private key).
fn signer_identity(sig_path: &Path) -> Option<String> {
    use sha2::Digest;
    let sf = sign::read_sig_file(sig_path).ok()?;
    let pub_hex = hex::encode(sf.embedded.to_bytes());
    Some(hex::encode(sha2::Sha256::digest(pub_hex.as_bytes())))
}

/// Whether the key embedded in a `.sig` was a *configured trusted* key of
/// the target root at install time.
fn signer_is_trusted(root: &Path, sig_path: &Path) -> bool {
    let Ok(sf) = sign::read_sig_file(sig_path) else {
        return false;
    };
    let embedded = sf.embedded.to_bytes();
    sign::trusted_keys(root)
        .map(|keys| keys.iter().any(|(_, vk)| vk.to_bytes() == embedded))
        .unwrap_or(false)
}

/// Persist the verified archive bytes as an immutable object and write the
/// provenance record (package identity, authentication method, repository
/// evidence, signer fingerprint, trust state at install time).
///
/// Authentication already happened (the caller verified these exact bytes);
/// `publish_object` re-hashes what it stores, so the record can never claim
/// a digest the bytes do not have.
fn publish_artifact_object(
    root: &Path,
    snapshot: &Path,
    digest: &str,
    archive_size: u64,
    name: &str,
    version: &str,
    rel: u32,
    arch: &str,
    evidence: &objects::InstallEvidence,
    verified_at: u64,
) -> Result<PathBuf> {
    let (repository, signer, trusted) = match (&evidence.origin, evidence.authentication) {
        (objects::ArtifactOrigin::Repository(id), objects::AUTH_SIGNED_INDEX) => {
            let index_sha256 = match &evidence.index {
                Some(p) => Some(crate::fetch::sha256_of_file(p)?),
                None => None,
            };
            let signer = evidence.index_sig.as_deref().and_then(signer_identity);
            let trusted = evidence.index_trusted
                || evidence
                    .index_sig
                    .as_deref()
                    .map(|sig| signer_is_trusted(root, sig))
                    .unwrap_or(false);
            (
                Some(objects::ArtifactRepository {
                    id: id.clone(),
                    index_sha256,
                    signer: signer.clone(),
                    trusted_at_install: trusted,
                }),
                signer,
                trusted,
            )
        }
        (_, objects::AUTH_DETACHED) => {
            let signer = evidence.detached_sig.as_deref().and_then(signer_identity);
            let trusted = evidence
                .detached_sig
                .as_deref()
                .map(|sig| signer_is_trusted(root, sig))
                .unwrap_or(false);
            (None, signer, trusted)
        }
        _ => (None, None, false),
    };
    let record = objects::ArtifactRecord {
        schema: objects::OBJECTS_SCHEMA,
        artifact_sha256: digest.to_string(),
        archive_size,
        authentication: evidence.authentication.to_string(),
        verified_at,
        origin: match &evidence.origin {
            objects::ArtifactOrigin::SourceBuild => "source-build",
            objects::ArtifactOrigin::Repository(_) => "repository",
            objects::ArtifactOrigin::StandaloneArchive => "standalone-archive",
        }
        .to_string(),
        package: objects::ArtifactPackage {
            name: name.to_string(),
            version: version.to_string(),
            rel,
            arch: arch.to_string(),
        },
        repository,
        signer,
        trusted_at_install: trusted,
    };
    objects::publish_object(
        root,
        snapshot,
        &record,
        &objects::EvidenceFiles {
            detached_sig: evidence.detached_sig.clone(),
            index: evidence.index.clone(),
            index_sig: evidence.index_sig.clone(),
        },
    )
}

fn save_undo(db: &Db, root: &Path, prev: &Entry) -> Result<()> {    let dir = root.join("var/lib/nawa/undo").join(&prev.name);
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

fn list_staged_files(staged: &Path) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for e in walkdir::WalkDir::new(staged).sort_by_file_name() {
        let e = e?;
        let rel = e.path().strip_prefix(staged)?;
        if rel.as_os_str().is_empty() {
            continue;
        }
        out.push(format!("/{}", rel.display()));
    }
    Ok(out)
}

/// Refuse to touch `path` when resolving it (following symlinks) would leave
/// `root`. The target root is a *tree*, not a chroot: nawa runs from another
/// root (e.g. a build chroot installing into `/sysroot`), so an absolute
/// symlink inside the target such as `usr/lib64 -> /usr/lib` resolves to the
/// RUNNING system's `/usr/lib` and an install would silently rewrite that
/// instead. Relative links that stay inside `root` (merged-usr `lib -> usr/lib`)
/// are fine; with `root` = `/` everything is inside.
fn ensure_inside_root(root: &Path, path: &Path) -> Result<()> {
    let real_root = std::fs::canonicalize(root)
        .with_context(|| format!("resolve root {}", root.display()))?;
    // resolve the nearest ancestor that exists (path itself, else a parent)
    let mut cur = path;
    let resolved = loop {
        match std::fs::canonicalize(cur) {
            Ok(p) => break p,
            Err(_) => {
                // a dangling or looping symlink cannot be resolved, so it
                // cannot be shown to stay inside root
                if std::fs::symlink_metadata(cur).map(|m| m.is_symlink()).unwrap_or(false) {
                    bail!(
                        "refusing to write through unresolvable symlink {} (target root {})",
                        cur.display(),
                        root.display()
                    );
                }
                match cur.parent() {
                    Some(p) => cur = p,
                    None => return Ok(()),
                }
            }
        }
    };
    if !resolved.starts_with(&real_root) {
        bail!(
            "refusing to write through symlink: {} resolves to {}, outside the target root {}",
            path.display(),
            resolved.display(),
            real_root.display()
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// transaction journal
// ---------------------------------------------------------------------------

/// One step of an in-flight transaction, in the order it was journaled.
#[derive(Debug, Clone, PartialEq)]
enum Step {
    /// `path` did not exist and is about to be created.
    Created(PathBuf),
    /// `dest` is about to be moved to `bak` (and replaced, or deleted).
    Replaced { dest: PathBuf, bak: PathBuf },
    /// The package's db directory. `backup` holds a complete copy of its
    /// previous contents (`None`: it did not exist before).
    Db { dir: PathBuf, backup: Option<PathBuf> },
}

/// Percent-encode a path for one journal field: bytes outside printable
/// ASCII, plus `%`, tab and newline, become `%XX` (file names are arbitrary
/// bytes on Linux).
fn enc(p: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    let mut out = String::new();
    for &b in p.as_os_str().as_bytes() {
        if (0x21..=0x7e).contains(&b) && b != b'%' {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02x}"));
        }
    }
    if out.is_empty() {
        out.push('%');
    }
    out
}

fn dec(s: &str) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    if s == "%" {
        return Some(PathBuf::new());
    }
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hex = s.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    Some(PathBuf::from(std::ffi::OsString::from_vec(out)))
}

/// Append-only on-disk record of what a transaction has done so far. Each
/// line is one `write(2)`, so it survives the process being killed; it is
/// fsynced every few entries, before the db is touched and at commit, which
/// bounds what a power cut can lose.
struct Journal {
    path: PathBuf,
    file: std::fs::File,
    unsynced: u32,
}

impl Journal {
    fn create(path: PathBuf, bak_root: &Path) -> Result<Self> {
        use std::io::Write;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .with_context(|| format!("create journal {}", path.display()))?;
        file.write_all(format!("H\t{}\n", enc(bak_root)).as_bytes())?;
        file.sync_data()?;
        if let Some(dir) = path.parent() {
            if let Ok(d) = std::fs::File::open(dir) {
                let _ = d.sync_all();
            }
        }
        Ok(Journal { path, file, unsynced: 0 })
    }

    fn record(&mut self, step: &Step) -> Result<()> {
        use std::io::Write;
        let line = match step {
            Step::Created(p) => format!("C\t{}\n", enc(p)),
            Step::Replaced { dest, bak } => format!("R\t{}\t{}\n", enc(dest), enc(bak)),
            Step::Db { dir, backup } => format!(
                "D\t{}\t{}\n",
                enc(dir),
                backup.as_deref().map(enc).unwrap_or_else(|| "-".to_string())
            ),
        };
        self.file
            .write_all(line.as_bytes())
            .with_context(|| format!("write journal {}", self.path.display()))?;
        self.unsynced += 1;
        // the db is about to change: everything before it must be durable
        if self.unsynced >= 32 || matches!(step, Step::Db { .. }) {
            self.sync()?;
        }
        Ok(())
    }

    fn sync(&mut self) -> Result<()> {
        self.file.sync_data()?;
        self.unsynced = 0;
        Ok(())
    }

    fn mark_committed(&mut self) -> Result<()> {
        use std::io::Write;
        self.file.write_all(b"COMMIT\n")?;
        self.sync()
    }
}

/// Parsed journal: the bak dir, the steps, and whether it was committed.
struct Parsed {
    bak_root: PathBuf,
    steps: Vec<Step>,
    committed: bool,
}

fn parse_journal(text: &str, fallback_bak: PathBuf) -> Parsed {
    let mut bak_root = fallback_bak;
    let mut steps = Vec::new();
    let mut committed = false;
    for line in text.lines() {
        let f: Vec<&str> = line.split('\t').collect();
        match f.as_slice() {
            ["H", b] => {
                if let Some(p) = dec(b) {
                    bak_root = p;
                }
            }
            ["C", p] => {
                if let Some(p) = dec(p) {
                    steps.push(Step::Created(p));
                }
            }
            ["R", d, b] => {
                if let (Some(dest), Some(bak)) = (dec(d), dec(b)) {
                    steps.push(Step::Replaced { dest, bak });
                }
            }
            ["D", d, b] => {
                if let Some(dir) = dec(d) {
                    let backup = if *b == "-" { None } else { dec(b) };
                    steps.push(Step::Db { dir, backup });
                }
            }
            ["COMMIT"] => committed = true,
            // a torn last line (power cut) or an unknown record is ignored:
            // the steps before it are all that was ever acted on
            _ => {}
        }
    }
    Parsed { bak_root, steps, committed }
}

/// Copy a (flat or nested) directory of small files.
fn copy_dir(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to)?;
    for e in std::fs::read_dir(from)? {
        let e = e?;
        let src = e.path();
        let dst = to.join(e.file_name());
        if e.file_type()?.is_dir() {
            copy_dir(&src, &dst)?;
        } else {
            std::fs::copy(&src, &dst)?;
        }
    }
    Ok(())
}

/// Put everything a transaction changed back, newest step first. Idempotent:
/// it can be run again after being interrupted itself, and it never deletes
/// a file whose backup is missing (nothing was moved yet in that case).
fn undo_steps(steps: &[Step], bak_root: &Path) {
    for step in steps.iter().rev() {
        match step {
            Step::Created(p) => {
                let _ = std::fs::remove_file(p);
            }
            Step::Replaced { dest, bak } => {
                if std::fs::symlink_metadata(bak).is_ok() {
                    let _ = std::fs::remove_file(dest);
                    let _ = std::fs::rename(bak, dest);
                }
            }
            Step::Db { dir, backup } => {
                let _ = std::fs::remove_dir_all(dir);
                if let Some(b) = backup {
                    let _ = copy_dir(b, dir);
                }
            }
        }
    }
    let _ = std::fs::remove_dir_all(bak_root);
}

/// Undo journal for `move_tree`: every file that replaces an existing one is
/// first moved to a backup dir, every newly created file is remembered, so a
/// failure half-way through can restore the previous state (all-or-nothing
/// file placement).
///
/// Every step is also written to an on-disk journal *before* it happens, so
/// the same undo works after a SIGKILL or power cut: [`recover`] replays it
/// the next time nawa takes the database lock. Dropping a `Tx` that was
/// neither committed nor rolled back (an early return, a panic) rolls back.
struct Tx {
    root: PathBuf,
    bak_root: PathBuf,
    journal: Option<Journal>,
    steps: Vec<Step>,
    done: bool,
}

fn journal_path(root: &Path, bak_root: &Path) -> PathBuf {
    let name = bak_root.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    root.join(".nawa").join(format!("{name}.journal"))
}

impl Tx {
    fn new(root: &Path, bak_root: PathBuf) -> Self {
        Tx { root: root.to_path_buf(), bak_root, journal: None, steps: Vec::new(), done: false }
    }

    /// Journal `step` (creating the journal on first use), then remember it.
    fn log(&mut self, step: Step) -> Result<()> {
        if self.journal.is_none() {
            self.journal = Some(Journal::create(journal_path(&self.root, &self.bak_root), &self.bak_root)?);
        }
        if let Some(j) = self.journal.as_mut() {
            j.record(&step)?;
        }
        self.steps.push(step);
        Ok(())
    }

    /// Move `from` to `to`, backing up whatever `to` currently is.
    fn place(&mut self, from: &Path, to: &Path) -> Result<()> {
        // the leaf may be a symlink (it is replaced, not followed); its
        // parent directories must stay inside the target root
        if let Some(parent) = to.parent() {
            ensure_inside_root(&self.root, parent)?;
        }
        if std::fs::symlink_metadata(to).is_ok() {
            let rel = to.strip_prefix(&self.root)?;
            let bak = self.bak_root.join(rel);
            self.log(Step::Replaced { dest: to.to_path_buf(), bak: bak.clone() })?;
            if let Some(parent) = bak.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::rename(to, &bak)
                .with_context(|| format!("back up {}", to.display()))?;
        } else {
            self.log(Step::Created(to.to_path_buf()))?;
        }
        std::fs::rename(from, to)
            .with_context(|| format!("move {} -> {}", from.display(), to.display()))
    }

    /// Move an existing path into the backup dir (a reversible delete).
    fn remove(&mut self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            ensure_inside_root(&self.root, parent)?;
        }
        let rel = path.strip_prefix(&self.root)?;
        let bak = self.bak_root.join(rel);
        self.log(Step::Replaced { dest: path.to_path_buf(), bak: bak.clone() })?;
        if let Some(parent) = bak.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::rename(path, &bak).with_context(|| format!("remove {}", path.display()))?;
        Ok(())
    }

    /// Copy the package's db directory aside and journal it. Call this right
    /// before the db is modified: a rollback (or crash recovery) then puts
    /// the db entry back together with the files.
    fn snapshot_db(&mut self, dir: &Path) -> Result<()> {
        let backup = if dir.exists() {
            let name = dir.file_name().map(|n| n.to_os_string()).unwrap_or_default();
            let b = self.bak_root.join(".db").join(name);
            let _ = std::fs::remove_dir_all(&b);
            // complete the copy BEFORE journaling it: a crash mid-copy then
            // leaves no record, and the db has not been touched yet
            copy_dir(dir, &b)?;
            Some(b)
        } else {
            None
        };
        self.log(Step::Db { dir: dir.to_path_buf(), backup })
    }

    /// Restore the pre-install state (best effort, idempotent).
    fn rollback(&mut self) {
        if self.done {
            return;
        }
        undo_steps(&self.steps, &self.bak_root);
        self.steps.clear();
        if let Some(j) = self.journal.take() {
            let _ = std::fs::remove_file(&j.path);
        }
        self.done = true;
    }

    fn commit(mut self) {
        if let Some(j) = self.journal.as_mut() {
            // once COMMIT is durable the transaction counts as done: a crash
            // from here on only has bak leftovers to tidy, never to restore
            let _ = j.mark_committed();
        }
        let _ = std::fs::remove_dir_all(&self.bak_root);
        if let Some(j) = self.journal.take() {
            let _ = std::fs::remove_file(&j.path);
        }
        self.done = true;
    }
}

impl Drop for Tx {
    fn drop(&mut self) {
        self.rollback();
    }
}

/// Repair what an interrupted nawa left behind. Call it while holding the
/// exclusive database lock (nothing else can be mid-transaction then):
///
/// * a journal without `COMMIT` is rolled back (files and db entry),
/// * a journal with `COMMIT` is just cleaned up (the install had finished),
/// * leftover staging directories of dead installs are removed.
///
/// Returns one line per repair for the caller to show.
pub fn recover(root: &Path) -> Result<Vec<String>> {
    let dir = root.join(".nawa");
    let mut notes = Vec::new();
    let rd = match std::fs::read_dir(&dir) {
        Ok(rd) => rd,
        Err(_) => return Ok(notes),
    };
    let mut journals = Vec::new();
    let mut stale_dirs = Vec::new();
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        let path = e.path();
        if name.ends_with(".journal") && (name.starts_with("bak-") || name.starts_with("rm-")) {
            journals.push((name, path));
        } else if name.starts_with("tmp-") && path.is_dir() {
            stale_dirs.push(path);
        }
    }
    journals.sort();
    for (name, path) in journals {
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        let fallback = dir.join(name.trim_end_matches(".journal"));
        let parsed = parse_journal(&text, fallback);
        if parsed.committed {
            let _ = std::fs::remove_dir_all(&parsed.bak_root);
            notes.push(format!("finished cleaning up a completed transaction ({name})"));
        } else if parsed.steps.is_empty() {
            let _ = std::fs::remove_dir_all(&parsed.bak_root);
        } else {
            undo_steps(&parsed.steps, &parsed.bak_root);
            notes.push(format!(
                "rolled back an interrupted transaction ({name}, {} step(s)): \
                 the system is back to its state before it started",
                parsed.steps.len()
            ));
        }
        let _ = std::fs::remove_file(&path);
    }
    for d in stale_dirs {
        let _ = std::fs::remove_dir_all(&d);
    }
    Ok(notes)
}

/// [`recover`], reporting on stderr. Failures to repair are warnings: the
/// command at hand should still be able to run.
pub fn recover_and_report(root: &Path) {
    match recover(root) {
        Ok(notes) => {
            for n in notes {
                eprintln!("nawa: recovered: {n}");
            }
        }
        Err(e) => eprintln!("nawa: warning: could not check for an interrupted transaction: {e:#}"),
    }
    // an exclusive-lock caller is also the right place to convert legacy
    // nanosecond install_date values (idempotent, keeps a recoverable backup)
    match Db::new(root).repair_legacy_dates() {
        Ok(0) => {}
        Ok(n) => eprintln!("nawa: repaired {n} package(s): legacy nanosecond install dates converted to Unix seconds (originals kept as desc.legacy-nanos)"),
        Err(e) => eprintln!("nawa: warning: could not repair legacy install dates: {e:#}"),
    }
}

/// Recursively move `src` into `dst`, merging directories.
///
/// Config protection (pacman-style): a file under `<root>/etc` that would
/// change content is only overwritten in place when the on-disk copy still
/// matches the hash recorded at install time (i.e. the admin never touched
/// it). Locally modified configs are preserved and the new version lands
/// next to them as `<path>.nawa-new` (recorded in `diverted`).
fn move_tree(
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

fn is_under_etc(p: &Path, root: &Path) -> bool {
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

/// sha256 map for every /etc file in the staging tree: path (as it will be
/// on the target, e.g. "/etc/foo.conf") -> hash. Symlinks hash their target
/// string so the baseline stays stable across identical reinstalls.
fn hash_staged_etc(staged: &Path) -> Result<std::collections::HashMap<String, String>> {
    let mut out = std::collections::HashMap::new();
    for e in walkdir::WalkDir::new(staged).sort_by_file_name() {
        let e = e?;
        let rel = e.path().strip_prefix(staged)?;
        if rel.as_os_str().is_empty() || rel.components().next().map(|c| c.as_os_str() != "etc").unwrap_or(true) {
            continue;
        }
        let pkgpath = format!("/{}", rel.display());
        if let Some(h) = path_hash(e.path()) {
            out.insert(pkgpath, h);
        }
    }
    Ok(out)
}

/// sha256 of a file's bytes; for symlinks, of the target path string.
fn path_hash(p: &Path) -> Option<String> {
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

fn contents_differ(staged: &Path, dest: &Path) -> bool {
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

fn hooks_path(db: &Db, name: &str) -> PathBuf {
    db.local_dir().join(name).join("hooks")
}

fn hook_status_path(db: &Db, name: &str) -> PathBuf {
    db.local_dir().join(name).join("hook-status")
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct HookStatus {
    version: String,
    label: String,
    status: String,
}

fn save_hook_status(db: &Db, name: &str, state: &HookStatus) -> Result<()> {
    let path = hook_status_path(db, name);
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, toml::to_string(state)?)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Return true when the installed package has a post-install hook that has
/// not been confirmed complete for its currently installed version.
///
/// Missing status files are treated as pending for one-time migration from
/// Nawa versions that stored hooks but did not persist their completion state.
pub(crate) fn hook_needs_retry(root: &Path, name: &str) -> Result<bool> {
    let db = Db::new(root);
    let entry = match db.entry(name)? {
        Some(entry) => entry,
        None => return Ok(false),
    };
    if load_hooks(&db, name).post_install.trim().is_empty() {
        return Ok(false);
    }
    let state = std::fs::read_to_string(hook_status_path(&db, name))
        .ok()
        .and_then(|raw| toml::from_str::<HookStatus>(&raw).ok());
    Ok(!matches!(
        state,
        Some(s) if s.version == entry.version_full() && s.status == "completed"
    ))
}

/// Retry only the saved post-install hook; package payload files are not
/// downloaded, extracted, replaced, or re-registered.
pub(crate) fn retry_pending_hook(name: &str, root: &Path) -> Result<()> {
    let db = Db::new(root);
    let entry = db.entry(name)?.ok_or_else(|| anyhow::anyhow!("package {name} is not installed"))?;
    if !hook_needs_retry(root, name)? {
        return Ok(());
    }
    let hooks = load_hooks(&db, name);
    if hooks.post_install.trim().is_empty() {
        return Ok(());
    }
    let prior = std::fs::read_to_string(hook_status_path(&db, name))
        .ok()
        .and_then(|raw| toml::from_str::<HookStatus>(&raw).ok());
    let label = prior
        .filter(|s| s.version == entry.version_full())
        .map(|s| s.label)
        .filter(|s| s == "post_install" || s == "post_upgrade")
        .unwrap_or_else(|| "post_install".to_string());
    run_tracked_hook(&db, &entry, &label, &hooks.post_install, root)
}

fn run_tracked_hook(db: &Db, entry: &Entry, label: &str, script: &str, root: &Path) -> Result<()> {
    let mut state = HookStatus {
        version: entry.version_full(),
        label: label.to_string(),
        status: "pending".to_string(),
    };
    save_hook_status(db, &entry.name, &state)
        .with_context(|| format!("could not mark {label} hook pending for {}", entry.name))?;
    match run_hook(label, script, root) {
        Ok(()) => {
            state.status = "completed".to_string();
            save_hook_status(db, &entry.name, &state)
                .with_context(|| format!("could not mark {label} hook complete for {}", entry.name))?;
            Ok(())
        }
        Err(error) => {
            let failure_status = if crate::interrupt::is_interrupted() {
                "interrupted"
            } else {
                "failed"
            };
            state.status = failure_status.to_string();
            if let Err(status_error) = save_hook_status(db, &entry.name, &state) {
                return Err(error.context(format!(
                    "also failed to persist hook failure state: {status_error:#}"
                )));
            }
            Err(error)
        }
    }
}

/// Persist a package's hooks next to its db entry (needed at remove time).
fn save_hooks(db: &Db, name: &str, hooks: &archive::Hooks) -> Result<()> {
    let p = hooks_path(db, name);
    if hooks.is_empty() {
        let _ = std::fs::remove_file(&p);
        return Ok(());
    }
    let tmp = p.with_extension("tmp");
    std::fs::write(&tmp, toml::to_string(hooks)?)?;
    std::fs::rename(&tmp, &p)?;
    Ok(())
}

fn load_hooks(db: &Db, name: &str) -> archive::Hooks {
    std::fs::read_to_string(hooks_path(db, name))
        .ok()
        .and_then(|s| toml::from_str(&s).ok())
        .unwrap_or_default()
}

/// True when this process runs as root. Uses the effective uid directly:
/// it must not depend on /proc being mounted (it is not inside a fresh chroot).
fn running_as_root() -> bool {
    rustix::process::geteuid().is_root()
}

/// Does `path` exist as seen from inside a chroot at `root`? Symlinks are
/// resolved relative to `root` (an absolute target like `/usr/bin` means
/// `root/usr/bin`, not the host's), unlike `root.join(path).exists()`, which
/// follows absolute links out of the root and can report a shell that the
/// chroot does not actually have. `resolve_in_root` returns the real host
/// path of the final target (what to open to read it), `None` if any link
/// in the chain is dangling.
pub(crate) fn resolve_in_root(root: &Path, path: &str) -> Option<PathBuf> {
    let mut cur = root.to_path_buf();
    let mut todo: Vec<std::ffi::OsString> = Path::new(path)
        .components()
        .rev()
        .filter_map(|c| match c {
            std::path::Component::Normal(n) => Some(n.to_os_string()),
            _ => None,
        })
        .collect();
    let mut hops = 0;
    while let Some(part) = todo.pop() {
        cur.push(&part);
        match std::fs::symlink_metadata(&cur) {
            Err(_) => return None,
            Ok(md) if md.file_type().is_symlink() => {
                hops += 1;
                if hops > 40 {
                    return None;
                }
                let target = match std::fs::read_link(&cur) {
                    Ok(t) => t,
                    Err(_) => return None,
                };
                cur.pop();
                if target.is_absolute() {
                    cur = root.to_path_buf();
                }
                for c in target.components().rev() {
                    match c {
                        std::path::Component::Normal(n) => todo.push(n.to_os_string()),
                        std::path::Component::ParentDir => todo.push("..".into()),
                        _ => {}
                    }
                }
            }
            Ok(_) => {
                if part == ".." {
                    cur.pop();
                    cur.pop();
                }
            }
        }
    }
    Some(cur)
}

/// [`resolve_in_root`] as a yes/no question.
pub(crate) fn exists_in_root(root: &Path, path: &str) -> bool {
    resolve_in_root(root, path).is_some()
}

/// Hooks run either on the live system (`--root /`) or, when nawa runs as
/// root and the target has a shell, inside a chroot of the target. Anything
/// else is skipped: executing a package's script on the host while
/// installing into another root would run it on the wrong filesystem.
/// (No /proc or /dev is mounted in the chroot.)
/// A hook that exited non-zero. When the user pressed Ctrl+C the hook was
/// almost certainly killed with us (same terminal process group): say so,
/// because by then the package itself is already completely installed.
fn hook_failed(label: &str) -> anyhow::Error {
    if interrupt::is_interrupted() {
        anyhow::anyhow!(
            "{label} hook interrupted - the package files are installed and registered, \
             but the hook did not finish; install the package again to re-run it"
        )
    } else {
        anyhow::anyhow!("{label} hook failed")
    }
}

fn run_hook(label: &str, script: &str, root: &Path) -> Result<()> {
    if script.trim().is_empty() {
        return Ok(());
    }
    let live = std::fs::canonicalize(root)
        .map(|p| p == Path::new("/"))
        .unwrap_or(false);
    let status = if live {
        println!("nawa: running {label} hook");
        // Installing glibc over a seed with a different glibc swaps the
        // loader (/usr/lib64/ld-linux-x86-64.so.2) BEFORE /etc/ld.so.cache is
        // regenerated, so the hook's own bash would run on the new loader with
        // the OLD libc from the cache ("__vdso_gettimeofday: invalid mode for
        // dlopen()"). Put /usr/lib first so loader and libc always match.
        let ld_path = match std::env::var_os("LD_LIBRARY_PATH") {
            Some(v) if !v.is_empty() => {
                let mut p = std::ffi::OsString::from("/usr/lib:");
                p.push(v);
                p
            }
            _ => std::ffi::OsString::from("/usr/lib"),
        };
        Command::new("bash")
            .args(["-e", "-c", script])
            .env("LD_LIBRARY_PATH", ld_path)
            .env("NAWA_ROOT", root)
            .env(crate::lock::HELD_ENV, "1") // we hold the db lock; nested nawa must not wait for it
            .status()?
    } else {
        let shell = ["/bin/bash", "/bin/sh"]
            .into_iter()
            .find(|sh| exists_in_root(root, sh));
        match shell {
            Some(sh) if running_as_root() => {
                println!("nawa: running {label} hook in chroot {}", root.display());
                let status = chroot_command(root, sh, script)?;
                if !status.success() {
                    return Err(hook_failed(label));
                }
                return Ok(());
            }
            _ => {
                println!("nawa: note: skipping {label} hook (--root is not / and no chroot possible)");
                return Ok(());
            }
        }
    };
    if !status.success() {
        return Err(hook_failed(label));
    }
    Ok(())
}

/// Run `sh -e -c script` inside a chroot of `root` using chroot(2) + chdir(2)
/// in the child's pre-exec hook — no dependence on the external coreutils
/// `chroot` binary (which is exactly what gets upgraded/installed by the
/// hooks this runs). (No /proc or /dev is mounted, same as before.)
fn chroot_command(root: &Path, sh: &str, script: &str) -> Result<std::process::ExitStatus> {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::process::CommandExt;
    let root_c = std::ffi::CString::new(root.as_os_str().as_bytes())
        .map_err(|_| anyhow::anyhow!("chroot path contains NUL: {}", root.display()))?;
    unsafe {
        Command::new(sh)
            .args(["-e", "-c", script])
            .env("NAWA_ROOT", "/")
            .env(crate::lock::HELD_ENV, "1") // we hold the db lock; nested nawa must not wait
            .pre_exec(move || {
                rustix::process::chroot(&root_c).map_err(std::io::Error::from)?;
                rustix::process::chdir(c"/").map_err(std::io::Error::from)?;
                Ok(())
            })
            .status()
            .context("spawn chrooted hook")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("nawa-install-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Build a fake package archive with the given files.
    fn make_pkg(dir: &Path, name: &str, version: &str, files: &[(&str, &[u8])]) -> PathBuf {
        let staging = dir.join(format!("staging-{name}-{version}"));
        std::fs::create_dir_all(&staging).unwrap();
        for (p, body) in files {
            let full = staging.join(p.trim_start_matches('/'));
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(full, body).unwrap();
        }
        let pkginfo = format!(
            "pkgname = {name}\npkgver = {version}\npkgrel = 1\narch = x86_64\nrundeps = \nsize = 1\n"
        );
        let out = dir.join(format!("{name}-{version}-1.nawa.tar.zst"));
        archive::create(&staging, &pkginfo, &out).unwrap();
        out
    }

    #[test]
    fn hook_status_requires_retry_until_current_version_is_completed() {
        let base = tmpdir("hook-status");
        let root = base.join("root");
        let db = Db::new(&root);
        let entry = Entry {
            name: "demo".into(),
            version: "1.0".into(),
            rel: 1,
            arch: "x86_64".into(),
            reason: "explicit".into(),
            run: vec![],
            install_date: 1,
            artifact_sha256: None,
            artifact_record: None,
            size: 1,
        };
        db.add(&entry, &[], &std::collections::HashMap::new()).unwrap();
        save_hooks(&db, "demo", &archive::Hooks {
            post_install: "true".into(),
            ..Default::default()
        }).unwrap();

        // Legacy installs have hooks but no status file: retry once.
        assert!(hook_needs_retry(&root, "demo").unwrap());

        let mut state = HookStatus {
            version: entry.version_full(),
            label: "post_install".into(),
            status: "interrupted".into(),
        };
        save_hook_status(&db, "demo", &state).unwrap();
        assert!(hook_needs_retry(&root, "demo").unwrap());

        state.status = "completed".into();
        save_hook_status(&db, "demo", &state).unwrap();
        assert!(!hook_needs_retry(&root, "demo").unwrap());

        state.version = "demo-0.9-1".into();
        save_hook_status(&db, "demo", &state).unwrap();
        assert!(hook_needs_retry(&root, "demo").unwrap());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn upgrade_updates_files_and_removes_obsolete() {
        let base = tmpdir("upgrade");
        let root = base.join("root");
        let v1 = make_pkg(&base, "demo", "1.0", &[
            ("/usr/bin/demo", b"v1"),
            ("/usr/lib/demo/old.so", b"legacy"),
        ]);
        let v2 = make_pkg(&base, "demo", "2.0", &[
            ("/usr/bin/demo", b"v2"),
        ]);

        install_archive(&v1, &root, false, true, None, "explicit").unwrap();
        assert_eq!(std::fs::read(root.join("usr/bin/demo")).unwrap(), b"v1");

        install_archive(&v2, &root, false, true, None, "explicit").unwrap();
        assert_eq!(std::fs::read(root.join("usr/bin/demo")).unwrap(), b"v2");
        // obsolete file was removed
        assert!(!root.join("usr/lib/demo/old.so").exists());

        let db = Db::new(&root);
        let e = db.entry("demo").unwrap().unwrap();
        assert_eq!(e.version, "2.0");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn upgrade_protects_modified_etc_files() {
        let base = tmpdir("etcnew");
        let root = base.join("root");
        let v1 = make_pkg(&base, "srv", "1.0", &[
            ("/etc/srv.conf", b"original\n"),
            ("/usr/bin/srv", b"bin1"),
        ]);
        let v2 = make_pkg(&base, "srv", "2.0", &[
            ("/etc/srv.conf", b"changed upstream\n"),
            ("/usr/bin/srv", b"bin2"),
        ]);

        install_archive(&v1, &root, false, true, None, "explicit").unwrap();
        // admin edits the config
        std::fs::write(root.join("etc/srv.conf"), b"locally edited\n").unwrap();

        install_archive(&v2, &root, false, true, None, "explicit").unwrap();

        // local edit preserved, upstream change diverted to .nawa-new
        assert_eq!(std::fs::read(root.join("etc/srv.conf")).unwrap(), b"locally edited\n");
        let diverted = root.join("etc/srv.conf.nawa-new");
        assert!(diverted.exists(), ".nawa-new diversion missing");
        assert_eq!(std::fs::read(&diverted).unwrap(), b"changed upstream\n");

        // non-etc files are replaced without diversion
        assert_eq!(std::fs::read(root.join("usr/bin/srv")).unwrap(), b"bin2");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn unmodified_etc_files_upgrade_in_place() {
        let base = tmpdir("etcsame");
        let root = base.join("root");
        let v1 = make_pkg(&base, "srv", "1.0", &[("/etc/srv.conf", b"same\n")]);
        let v2 = make_pkg(&base, "srv", "2.0", &[("/etc/srv.conf", b"same\n")]);
        install_archive(&v1, &root, false, true, None, "explicit").unwrap();
        install_archive(&v2, &root, false, true, None, "explicit").unwrap();
        assert!(root.join("etc/srv.conf").exists());
        assert!(!root.join("etc/srv.conf.nawa-new").exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn upstream_config_change_without_local_edits_overwrites_in_place() {
        // admin never touched the config (hash baseline matches) -> a version
        // bump that changes /etc content replaces it directly, no diversion
        let base = tmpdir("etcupstream");
        let root = base.join("root");
        let v1 = make_pkg(&base, "srv", "1.0", &[("/etc/srv.conf", b"stock v1\n")]);
        let v2 = make_pkg(&base, "srv", "2.0", &[("/etc/srv.conf", b"stock v2\n")]);
        install_archive(&v1, &root, false, true, None, "explicit").unwrap();
        install_archive(&v2, &root, false, true, None, "explicit").unwrap();
        assert_eq!(std::fs::read(root.join("etc/srv.conf")).unwrap(), b"stock v2\n");
        assert!(!root.join("etc/srv.conf.nawa-new").exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn hashes_baseline_missing_falls_back_to_diversion() {
        // simulate a legacy db entry (installed before hash tracking): the
        // on-disk config has no baseline -> safe default is diversion
        let base = tmpdir("etchash");
        let root = base.join("root");
        let v1 = make_pkg(&base, "srv", "1.0", &[("/etc/srv.conf", b"v1\n")]);
        let v2 = make_pkg(&base, "srv", "2.0", &[("/etc/srv.conf", b"v2\n")]);
        install_archive(&v1, &root, false, true, None, "explicit").unwrap();
        // wipe the baseline to emulate a pre-v0.5 database
        let db = Db::new(&root);
        std::fs::remove_file(db.local_dir().join("srv/hashes")).unwrap();
        install_archive(&v2, &root, false, true, None, "explicit").unwrap();
        assert_eq!(std::fs::read(root.join("etc/srv.conf")).unwrap(), b"v1\n");
        assert_eq!(std::fs::read(root.join("etc/srv.conf.nawa-new")).unwrap(), b"v2\n");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn rollback_reverts_upgrade_and_ping_pongs() {
        let base = tmpdir("rollback");
        let root = base.join("root");
        let v1 = make_pkg(&base, "demo", "1.0", &[("/usr/bin/demo", b"v1")]);
        let v2 = make_pkg(&base, "demo", "2.0", &[("/usr/bin/demo", b"v2")]);

        install_archive(&v1, &root, false, true, None, "explicit").unwrap();
        install_archive(&v2, &root, false, true, None, "explicit").unwrap();
        assert_eq!(std::fs::read(root.join("usr/bin/demo")).unwrap(), b"v2");

        // roll back to 1.0
        rollback("demo", &root).unwrap();
        assert_eq!(std::fs::read(root.join("usr/bin/demo")).unwrap(), b"v1");
        let db = Db::new(&root);
        assert_eq!(db.entry("demo").unwrap().unwrap().version, "1.0");

        // ping-pong: undo record now points at 2.0
        rollback("demo", &root).unwrap();
        assert_eq!(std::fs::read(root.join("usr/bin/demo")).unwrap(), b"v2");

        // and back again — the undo log alternates indefinitely
        rollback("demo", &root).unwrap();
        assert_eq!(std::fs::read(root.join("usr/bin/demo")).unwrap(), b"v1");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn rollback_after_remove_reinstalls() {
        let base = tmpdir("rmrollback");
        let root = base.join("root");
        let v1 = make_pkg(&base, "gone", "1.0", &[("/usr/bin/gone", b"x")]);
        install_archive(&v1, &root, false, true, None, "explicit").unwrap();
        remove("gone", &root).unwrap();
        assert!(!root.join("usr/bin/gone").exists());
        rollback("gone", &root).unwrap();
        assert_eq!(std::fs::read(root.join("usr/bin/gone")).unwrap(), b"x");
        let db = Db::new(&root);
        assert!(db.entry("gone").unwrap().is_some());

        // undo record now equals the installed version -> nothing to roll back
        assert!(rollback("gone", &root).is_err());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn reason_keep_preserves_explicit_flag() {
        let base = tmpdir("reason");
        let root = base.join("root");
        let v1 = make_pkg(&base, "r", "1.0", &[("/usr/bin/r", b"a")]);
        let v2 = make_pkg(&base, "r", "2.0", &[("/usr/bin/r", b"b")]);
        install_archive(&v1, &root, false, true, None, "dependency").unwrap();
        let db = Db::new(&root);
        assert_eq!(db.entry("r").unwrap().unwrap().reason, "dependency");
        install_archive(&v2, &root, false, true, None, "keep").unwrap();
        assert_eq!(db.entry("r").unwrap().unwrap().reason, "dependency");
        let _ = std::fs::remove_dir_all(&base);
    }

    // ---- content-addressed artifact identity (Phase 0) ---------------------

    #[test]
    fn install_records_artifact_identity_in_db_and_object_store() {
        let base = tmpdir("objectsdb");
        let root = base.join("root");
        let v1 = make_pkg(&base, "obj", "1.0", &[("/usr/bin/obj", b"o")]);
        let digest = crate::fetch::sha256_of_file(&v1).unwrap();

        install_archive(&v1, &root, false, true, None, "explicit").unwrap();

        let db = Db::new(&root);
        let e = db.entry("obj").unwrap().unwrap();
        assert_eq!(e.artifact_sha256.as_deref(), Some(digest.as_str()));
        assert_eq!(e.artifact_record.as_deref(), Some(format!("sha256:{digest}").as_str()));

        // the object store holds the byte-identical archive under its digest
        let stored = objects::open_verified_object(&root, &digest)
            .expect("published object verifies");
        assert_eq!(crate::fetch::sha256_of_file(&stored.archive).unwrap(), digest);
        // path is derived from the digest only (sharded)
        assert!(stored.archive.starts_with(root.join("var/cache/nawa/objects/sha256").join(&digest[..2])));
        let rec = stored.record.expect("provenance record written");
        assert_eq!(rec.artifact_sha256, digest);
        assert_eq!(rec.package.name, "obj");
        assert_eq!(rec.package.version, "1.0");
        assert_eq!(rec.origin, "standalone-archive");
        // unsigned local archive: recorded as exactly what it is
        assert_eq!(rec.authentication, objects::AUTH_UNSIGNED);
        assert_eq!(rec.signer, None);

        // re-installing the identical bytes must not complain or duplicate
        install_archive(&v1, &root, false, true, None, "explicit").unwrap();
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn rollback_restores_from_object_store_without_legacy_cache() {
        let base = tmpdir("objrollback");
        let root = base.join("root");
        let v1 = make_pkg(&base, "demo", "1.0", &[("/usr/bin/demo", b"v1")]);
        let v2 = make_pkg(&base, "demo", "2.0", &[("/usr/bin/demo", b"v2")]);
        install_archive(&v1, &root, false, true, None, "explicit").unwrap();
        install_archive(&v2, &root, false, true, None, "explicit").unwrap();

        // prune the whole version-named cache: the object store is the
        // restore source now
        for p in std::fs::read_dir(root.join("var/cache/nawa")).unwrap().flatten() {
            let path = p.path();
            if path.extension().is_some() {
                let _ = std::fs::remove_file(&path);
            }
        }
        assert!(!root.join("var/cache/nawa/demo-1.0-1.nawa.tar.zst").exists());

        rollback("demo", &root).unwrap();
        assert_eq!(std::fs::read(root.join("usr/bin/demo")).unwrap(), b"v1");
        // the restored entry keeps the same artifact identity as the original
        let db = Db::new(&root);
        let e = db.entry("demo").unwrap().unwrap();
        let v1_digest = crate::fetch::sha256_of_file(&v1).unwrap();
        assert_eq!(e.artifact_sha256.as_deref(), Some(v1_digest.as_str()));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn rollback_refuses_when_every_artifact_copy_is_corrupt_or_missing() {
        let base = tmpdir("objcorrupt");
        let root = base.join("root");
        let v1 = make_pkg(&base, "demo", "1.0", &[("/usr/bin/demo", b"v1")]);
        let v2 = make_pkg(&base, "demo", "2.0", &[("/usr/bin/demo", b"v2")]);
        install_archive(&v1, &root, false, true, None, "explicit").unwrap();
        install_archive(&v2, &root, false, true, None, "explicit").unwrap();

        // corrupt the stored object for 1.0 and prune the version cache
        let v1_digest = crate::fetch::sha256_of_file(&v1).unwrap();
        let shard = root.join("var/cache/nawa/objects/sha256").join(&v1_digest[..2]);
        for p in std::fs::read_dir(shard).unwrap().flatten() {
            let path = p.path();
            if path.extension().map(|e| e == "zst").unwrap_or(false) {
                std::fs::write(&path, b"tampered-bytes").unwrap();
            }
        }
        for p in std::fs::read_dir(root.join("var/cache/nawa")).unwrap().flatten() {
            let path = p.path();
            if path.extension().is_some() {
                let _ = std::fs::remove_file(&path);
            }
        }

        let err = rollback("demo", &root).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("cannot serve it (corrupt or pruned)"),
            "unexpected refusal reason: {msg}"
        );
        // the failed restore changed nothing
        let db = Db::new(&root);
        assert_eq!(db.entry("demo").unwrap().unwrap().version, "2.0");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn rollback_refuses_a_digest_that_names_a_foreign_package_record() {
        let base = tmpdir("objforeign");
        let root = base.join("root");
        let v1 = make_pkg(&base, "demo", "1.0", &[("/usr/bin/demo", b"v1")]);
        let v2 = make_pkg(&base, "demo", "2.0", &[("/usr/bin/demo", b"v2")]);
        install_archive(&v1, &root, false, true, None, "explicit").unwrap();
        install_archive(&v2, &root, false, true, None, "explicit").unwrap();

        // the undo record for 1.0 is tampered to claim a digest whose
        // provenance describes a different package identity
        let other = make_pkg(&base, "other", "9.0", &[("/usr/bin/other", b"x")]);
        install_archive(&other, &root, false, true, None, "explicit").unwrap();
        let other_digest = crate::fetch::sha256_of_file(&other).unwrap();
        let undo_desc = root.join("var/lib/nawa/undo/demo/desc");
        let raw = std::fs::read_to_string(&undo_desc).unwrap();
        let patched = raw
            .split("artifact_sha256 = \"")
            .next()
            .unwrap()
            .to_string()
            + "artifact_sha256 = \""
            + &other_digest
            + "\"\n";
        std::fs::write(&undo_desc, patched).unwrap();

        let err = rollback("demo", &root).unwrap_err();
        assert!(
            format!("{err:#}").contains("describes package other"),
            "unexpected refusal reason: {err:#}"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn remove_refuses_when_required_by_another_package() {
        let base = tmpdir("revdeps");
        let root = base.join("root");
        let lib = make_pkg(&base, "libx", "1.0", &[("/usr/lib/libx", b"l")]);
        let st = base.join("st-app");
        std::fs::create_dir_all(st.join("usr/bin")).unwrap();
        std::fs::write(st.join("usr/bin/app"), b"a").unwrap();
        let app = base.join("app-1.0-1.nawa.tar.zst");
        archive::create(
            &st,
            "pkgname = app\npkgver = 1.0\npkgrel = 1\narch = x86_64\nrundeps = libx\nsize = 1\n",
            &app,
        )
        .unwrap();
        install_archive(&lib, &root, false, true, None, "explicit").unwrap();
        install_archive(&app, &root, false, true, None, "explicit").unwrap();

        assert!(remove("libx", &root).is_err());
        assert!(root.join("usr/lib/libx").exists());
        remove_with("libx", &root, true).unwrap();
        assert!(!root.join("usr/lib/libx").exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn tx_rollback_restores_replaced_and_removes_created() {
        let base = tmpdir("tx");
        let root = base.join("root");
        std::fs::create_dir_all(root.join("usr/bin")).unwrap();
        std::fs::write(root.join("usr/bin/old"), b"orig").unwrap();
        let stage = base.join("stage");
        std::fs::create_dir_all(&stage).unwrap();
        std::fs::write(stage.join("new-content"), b"new").unwrap();
        std::fs::write(stage.join("fresh"), b"fresh").unwrap();

        let mut tx = Tx::new(&root, base.join("bak"));
        tx.place(&stage.join("new-content"), &root.join("usr/bin/old")).unwrap();
        tx.place(&stage.join("fresh"), &root.join("usr/bin/fresh")).unwrap();
        assert_eq!(std::fs::read(root.join("usr/bin/old")).unwrap(), b"new");

        tx.rollback();
        assert_eq!(std::fs::read(root.join("usr/bin/old")).unwrap(), b"orig");
        assert!(!root.join("usr/bin/fresh").exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn tx_remove_is_reversible() {
        let base = tmpdir("tx-remove");
        let root = base.join("root");
        std::fs::create_dir_all(root.join("etc")).unwrap();
        std::fs::write(root.join("etc/gone"), b"keep me").unwrap();
        let mut tx = Tx::new(&root, base.join("bak"));
        tx.remove(&root.join("etc/gone")).unwrap();
        assert!(!root.join("etc/gone").exists());
        tx.rollback();
        assert_eq!(std::fs::read(root.join("etc/gone")).unwrap(), b"keep me");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn place_refuses_symlink_that_escapes_root() {
        let base = tmpdir("escape");
        let root = base.join("root");
        let other = base.join("other-root-usr-lib");
        std::fs::create_dir_all(root.join("usr")).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        // absolute link inside the target tree, like `usr/lib64 -> /usr/lib`
        std::os::unix::fs::symlink(&other, root.join("usr/lib64")).unwrap();
        let stage = base.join("stage");
        std::fs::create_dir_all(&stage).unwrap();
        std::fs::write(stage.join("ld.so"), b"loader").unwrap();

        let mut tx = Tx::new(&root, base.join("bak"));
        let err = tx.place(&stage.join("ld.so"), &root.join("usr/lib64/ld.so")).unwrap_err();
        assert!(format!("{err:#}").contains("outside the target root"), "{err:#}");
        assert!(!other.join("ld.so").exists(), "file leaked through the symlink");
        assert!(stage.join("ld.so").exists(), "source must stay untouched");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn place_allows_relative_symlink_inside_root() {
        let base = tmpdir("merged-usr");
        let root = base.join("root");
        std::fs::create_dir_all(root.join("usr/lib")).unwrap();
        std::os::unix::fs::symlink("usr/lib", root.join("lib")).unwrap(); // merged-usr
        let stage = base.join("stage");
        std::fs::create_dir_all(&stage).unwrap();
        std::fs::write(stage.join("libx.so"), b"x").unwrap();

        let mut tx = Tx::new(&root, base.join("bak"));
        tx.place(&stage.join("libx.so"), &root.join("lib/libx.so")).unwrap();
        assert_eq!(std::fs::read(root.join("usr/lib/libx.so")).unwrap(), b"x");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn remove_refuses_symlink_that_escapes_root() {
        let base = tmpdir("escape-rm");
        let root = base.join("root");
        let other = base.join("elsewhere");
        std::fs::create_dir_all(root.join("usr")).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(other.join("victim"), b"keep").unwrap();
        std::os::unix::fs::symlink(&other, root.join("usr/lib64")).unwrap();

        let mut tx = Tx::new(&root, base.join("bak"));
        assert!(tx.remove(&root.join("usr/lib64/victim")).is_err());
        assert_eq!(std::fs::read(other.join("victim")).unwrap(), b"keep");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn install_refuses_directory_symlinked_outside_root() {
        let base = tmpdir("escape-install");
        let root = base.join("root");
        let other = base.join("host-usr-lib");
        std::fs::create_dir_all(root.join("usr")).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(other.join("ld-linux.so.2"), b"REAL LOADER").unwrap();
        std::os::unix::fs::symlink(&other, root.join("usr/lib64")).unwrap();
        let pkg = make_pkg(&base, "demo", "1.0", &[("/usr/lib64/ld-linux.so.2", b"new")]);

        assert!(install_archive(&pkg, &root, false, true, None, "explicit").is_err());
        assert_eq!(std::fs::read(other.join("ld-linux.so.2")).unwrap(), b"REAL LOADER");
        let _ = std::fs::remove_dir_all(&base);
    }
    #[test]
    fn verified_snapshot_is_independent_of_the_source_path() {
        let base = tmpdir("verified-snapshot");
        let archive = make_pkg(&base, "snapshot-demo", "1.0", &[("/usr/share/snapshot-demo", b"original")]);
        let original = std::fs::read(&archive).unwrap();
        let digest = crate::fetch::sha256_of_file(&archive).unwrap();

        let snapshot = crate::fetch::snapshot_file(&archive, Some(&digest), &base.join(".nawa")).unwrap();
        assert_ne!(snapshot.path(), archive);
        assert_eq!(std::fs::read(snapshot.path()).unwrap(), original);
        std::fs::write(&archive, b"replaced after snapshot creation").unwrap();
        assert_eq!(std::fs::read(snapshot.path()).unwrap(), original);
        let snapshot_path = snapshot.path().to_path_buf();
        drop(snapshot);
        assert!(!snapshot_path.exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn pinned_install_rejects_bytes_that_differ_from_the_index_digest() {
        let base = tmpdir("pinned-install");
        let root = base.join("root");
        std::fs::create_dir_all(&root).unwrap();
        let archive = make_pkg(&base, "pinned-demo", "1.0", &[("/usr/share/pinned-demo", b"x")]);
        let digest = crate::fetch::sha256_of_file(&archive).unwrap();

        let wrong = "0".repeat(64);
        let err = install_archive_pinned(&archive, &root, false, true, None, "explicit", Some(&wrong))
            .unwrap_err();
        assert!(format!("{err:#}").contains("sha256 mismatch"), "{err:#}");
        assert!(!root.join("usr/share/pinned-demo").exists());

        install_archive_pinned(&archive, &root, false, true, None, "explicit", Some(&digest.to_uppercase()))
            .unwrap();
        assert!(root.join("usr/share/pinned-demo").exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn stale_snapshots_of_dead_processes_are_removed() {
        if !Path::new("/proc/self").exists() {
            return;
        }
        let base = tmpdir("stale-snapshots");
        let archive = make_pkg(&base, "stale-demo", "1.0", &[("/usr/share/stale-demo", b"x")]);
        let dir = base.join(".nawa");
        std::fs::create_dir_all(&dir).unwrap();
        // a pid above any kernel pid_max can never be alive
        let dead = dir.join("verified-4294967290-0.tmp");
        let mine = dir.join(format!("verified-{}-999999.tmp", std::process::id()));
        let unrelated = dir.join("tmp-123");
        std::fs::write(&dead, b"old").unwrap();
        std::fs::write(&mine, b"live").unwrap();
        std::fs::write(&unrelated, b"keep").unwrap();

        let _snapshot = crate::fetch::snapshot_file(&archive, None, &dir).unwrap();
        assert!(!dead.exists());
        assert!(mine.exists());
        assert!(unrelated.exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    // ---- interruption / crash safety ---------------------------------------

    fn leftovers(root: &Path) -> Vec<String> {
        // anything in .nawa other than the (empty or absent) bookkeeping dirs
        match std::fs::read_dir(root.join(".nawa")) {
            Ok(rd) => rd.flatten().map(|e| e.file_name().to_string_lossy().to_string()).collect(),
            Err(_) => Vec::new(),
        }
    }

    #[test]
    fn journal_path_encoding_roundtrips_arbitrary_bytes() {
        use std::os::unix::ffi::OsStrExt;
        for raw in [
            &b"/usr/bin/plain"[..],
            b"/usr/share/with space/and\ttab",
            b"/usr/share/new\nline",
            b"/usr/share/100%/\xff\xfe-bytes",
            b"",
        ] {
            let p = PathBuf::from(std::ffi::OsStr::from_bytes(raw));
            let e = enc(&p);
            assert!(!e.contains('\t') && !e.contains('\n') && !e.contains(' '), "{e:?}");
            assert_eq!(dec(&e).unwrap(), p, "{e}");
        }
    }

    #[test]
    fn interrupt_before_install_changes_nothing() {
        let base = tmpdir("intr-fresh");
        let root = base.join("root");
        let pkg = make_pkg(&base, "demo", "1.0", &[("/usr/bin/demo", b"v1")]);

        interrupt::simulate_interrupt();
        let err = install_archive(&pkg, &root, false, true, None, "explicit").unwrap_err();
        interrupt::clear_simulated_interrupt();

        assert!(err.downcast_ref::<interrupt::Interrupted>().is_some(), "{err:#}");
        assert!(!root.join("usr/bin/demo").exists());
        assert!(Db::new(&root).entry("demo").unwrap().is_none());
        assert!(leftovers(&root).is_empty(), "{:?}", leftovers(&root));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn interrupt_during_upgrade_keeps_the_old_version_intact() {
        let base = tmpdir("intr-upgrade");
        let root = base.join("root");
        let v1 = make_pkg(&base, "demo", "1.0", &[("/usr/bin/demo", b"v1")]);
        let v2 = make_pkg(&base, "demo", "2.0", &[("/usr/bin/demo", b"v2"), ("/usr/bin/extra", b"x")]);
        install_archive(&v1, &root, false, true, None, "explicit").unwrap();

        interrupt::simulate_interrupt();
        let err = install_archive(&v2, &root, false, true, None, "explicit").unwrap_err();
        interrupt::clear_simulated_interrupt();

        assert!(err.downcast_ref::<interrupt::Interrupted>().is_some());
        assert_eq!(std::fs::read(root.join("usr/bin/demo")).unwrap(), b"v1");
        assert!(!root.join("usr/bin/extra").exists());
        assert_eq!(Db::new(&root).entry("demo").unwrap().unwrap().version, "1.0");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn interrupt_stops_a_removal_without_removing_anything() {
        let base = tmpdir("intr-remove");
        let root = base.join("root");
        let v1 = make_pkg(&base, "demo", "1.0", &[("/usr/bin/demo", b"v1"), ("/usr/bin/two", b"2")]);
        install_archive(&v1, &root, false, true, None, "explicit").unwrap();

        interrupt::simulate_interrupt();
        let err = remove_with("demo", &root, false).unwrap_err();
        interrupt::clear_simulated_interrupt();

        assert!(err.downcast_ref::<interrupt::Interrupted>().is_some());
        assert!(root.join("usr/bin/demo").exists() && root.join("usr/bin/two").exists());
        assert!(Db::new(&root).entry("demo").unwrap().is_some());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn dropping_an_uncommitted_tx_rolls_back() {
        let base = tmpdir("tx-drop");
        let root = base.join("root");
        std::fs::create_dir_all(root.join("usr/bin")).unwrap();
        std::fs::write(root.join("usr/bin/old"), b"orig").unwrap();
        let stage = base.join("stage");
        std::fs::create_dir_all(&stage).unwrap();
        std::fs::write(stage.join("n"), b"new").unwrap();
        {
            let mut tx = Tx::new(&root, root.join(".nawa/bak-1"));
            tx.place(&stage.join("n"), &root.join("usr/bin/old")).unwrap();
            // early return / panic path: no commit, no explicit rollback
        }
        assert_eq!(std::fs::read(root.join("usr/bin/old")).unwrap(), b"orig");
        assert!(leftovers(&root).is_empty(), "{:?}", leftovers(&root));
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A process killed with SIGKILL never runs Drop: the journal and the
    /// backup dir stay on disk and `recover` must put everything back,
    /// including the three-file db entry written halfway.
    #[test]
    fn recover_rolls_back_a_killed_upgrade_files_and_db() {
        let base = tmpdir("recover-kill");
        let root = base.join("root");
        let v1 = make_pkg(&base, "demo", "1.0", &[("/usr/bin/demo", b"v1"), ("/etc/demo.conf", b"c1")]);
        install_archive(&v1, &root, false, true, None, "explicit").unwrap();
        let db = Db::new(&root);
        let before_desc = std::fs::read(db.entry_dir("demo").unwrap().join("desc")).unwrap();
        let before_files = std::fs::read(db.entry_dir("demo").unwrap().join("files")).unwrap();

        // --- "install v2", die after the db's desc file was replaced -------
        let stage = base.join("stage");
        std::fs::create_dir_all(&stage).unwrap();
        std::fs::write(stage.join("demo"), b"v2").unwrap();
        std::fs::write(stage.join("newfile"), b"n").unwrap();
        let mut tx = Tx::new(&root, root.join(".nawa/bak-777"));
        tx.place(&stage.join("demo"), &root.join("usr/bin/demo")).unwrap();
        tx.place(&stage.join("newfile"), &root.join("usr/bin/newfile")).unwrap();
        tx.remove(&root.join("etc/demo.conf")).unwrap();
        tx.snapshot_db(&db.entry_dir("demo").unwrap()).unwrap();
        std::fs::write(db.entry_dir("demo").unwrap().join("desc"), b"garbage = half written").unwrap();
        std::mem::forget(tx); // SIGKILL: no Drop, no rollback

        assert_eq!(std::fs::read(root.join("usr/bin/demo")).unwrap(), b"v2");
        assert!(db.entry("demo").is_err(), "db is corrupt before recovery");

        let notes = recover(&root).unwrap();
        assert_eq!(notes.len(), 1, "{notes:?}");

        assert_eq!(std::fs::read(root.join("usr/bin/demo")).unwrap(), b"v1");
        assert_eq!(std::fs::read(root.join("etc/demo.conf")).unwrap(), b"c1");
        assert!(!root.join("usr/bin/newfile").exists());
        assert_eq!(std::fs::read(db.entry_dir("demo").unwrap().join("desc")).unwrap(), before_desc);
        assert_eq!(std::fs::read(db.entry_dir("demo").unwrap().join("files")).unwrap(), before_files);
        assert_eq!(db.entry("demo").unwrap().unwrap().version, "1.0");
        assert!(leftovers(&root).is_empty(), "{:?}", leftovers(&root));
        // running it again is a no-op
        assert!(recover(&root).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn recover_removes_a_killed_fresh_install_completely() {
        let base = tmpdir("recover-fresh");
        let root = base.join("root");
        let db = Db::new(&root);
        let stage = base.join("stage");
        std::fs::create_dir_all(&stage).unwrap();
        std::fs::write(stage.join("chrome"), b"bin").unwrap();
        let mut tx = Tx::new(&root, root.join(".nawa/bak-5"));
        std::fs::create_dir_all(root.join("usr/bin")).unwrap();
        tx.place(&stage.join("chrome"), &root.join("usr/bin/chrome")).unwrap();
        tx.snapshot_db(&db.entry_dir("chrome").unwrap()).unwrap(); // did not exist
        std::fs::create_dir_all(db.entry_dir("chrome").unwrap()).unwrap();
        std::fs::write(db.entry_dir("chrome").unwrap().join("desc"), b"partial").unwrap();
        std::mem::forget(tx);

        recover(&root).unwrap();
        assert!(!root.join("usr/bin/chrome").exists());
        assert!(!db.entry_dir("chrome").unwrap().exists());
        assert!(leftovers(&root).is_empty());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn recover_keeps_a_committed_install_and_only_cleans_up() {
        let base = tmpdir("recover-commit");
        let root = base.join("root");
        let stage = base.join("stage");
        std::fs::create_dir_all(&stage).unwrap();
        std::fs::create_dir_all(root.join("usr/bin")).unwrap();
        std::fs::write(root.join("usr/bin/tool"), b"old").unwrap();
        std::fs::write(stage.join("tool"), b"new").unwrap();
        let mut tx = Tx::new(&root, root.join(".nawa/bak-9"));
        tx.place(&stage.join("tool"), &root.join("usr/bin/tool")).unwrap();
        // killed right after COMMIT became durable, before the cleanup
        tx.journal.as_mut().unwrap().mark_committed().unwrap();
        std::mem::forget(tx);

        let notes = recover(&root).unwrap();
        assert_eq!(notes.len(), 1);
        assert_eq!(std::fs::read(root.join("usr/bin/tool")).unwrap(), b"new");
        assert!(leftovers(&root).is_empty(), "{:?}", leftovers(&root));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn recover_survives_a_torn_journal_and_clears_stale_staging() {
        let base = tmpdir("recover-torn");
        let root = base.join("root");
        std::fs::create_dir_all(root.join("usr/bin")).unwrap();
        std::fs::write(root.join("usr/bin/keep"), b"orig").unwrap();
        let nawa = root.join(".nawa");
        std::fs::create_dir_all(nawa.join("tmp-123/usr")).unwrap(); // dead staging dir
        // a journal that stops mid-line, as after a power cut
        let bak = nawa.join("bak-1");
        let bak_enc = enc(&bak);
        std::fs::write(nawa.join("bak-1.journal"), format!("H\t{bak_enc}\nC\t%2fno/such/file\nR\t%2fusr/bi")).unwrap();

        recover(&root).unwrap();
        assert_eq!(std::fs::read(root.join("usr/bin/keep")).unwrap(), b"orig");
        assert!(leftovers(&root).is_empty(), "{:?}", leftovers(&root));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_replaced_entry_without_a_backup_is_never_deleted_by_recovery() {
        // crash after the journal line but before the backup rename: the
        // original is still at `dest` and must survive the replay
        let base = tmpdir("recover-nobak");
        let root = base.join("root");
        std::fs::create_dir_all(root.join("usr/bin")).unwrap();
        std::fs::write(root.join("usr/bin/precious"), b"orig").unwrap();
        let nawa = root.join(".nawa");
        std::fs::create_dir_all(&nawa).unwrap();
        let bak = nawa.join("bak-3");
        let line = format!(
            "H\t{}\nR\t{}\t{}\n",
            enc(&bak),
            enc(&root.join("usr/bin/precious")),
            enc(&bak.join("usr/bin/precious"))
        );
        std::fs::write(nawa.join("bak-3.journal"), line).unwrap();

        recover(&root).unwrap();
        assert_eq!(std::fs::read(root.join("usr/bin/precious")).unwrap(), b"orig");
        let _ = std::fs::remove_dir_all(&base);
    }
}
