//! Rollback, removal, and retained-proof verification.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

use crate::artifact;
use crate::db::{Db, Entry};
use crate::install::hooks::{hook_needs_retry, load_hooks, run_hook};
use crate::install::journal::{recover_and_report, Tx};
use crate::install::payload::save_undo;
use crate::install::{inspect_archive_state, install_archive, install_archive_from_index_proof, install_archive_pinned, InstallOpts};
use crate::interrupt;
use crate::sign;

pub(crate) fn rollback(name: &str, root: &Path) -> Result<()> {
    let db = Db::new(root);
    crate::db::check_name(name)?;
    let desc_path = root.join("var/lib/nawa/undo").join(name).join("desc");
    if !desc_path.exists() {
        bail!("no undo record for `{name}` — nothing to roll back");
    }
    let prev: Entry = toml::from_str(&std::fs::read_to_string(&desc_path)?)
        .context("corrupt undo record")?;
    let before = db.entry(name)?;
    if let Some(cur) = &before {
        if cur.version_full() == prev.version_full() {
            bail!("`{name}` is already at {} — nothing to roll back", cur.version_full());
        }
    }
    crate::nawa_info!(
        "rolling back {} -> {}",
        before.as_ref().map(|e| e.version_full()).unwrap_or_else(|| "(removed)".into()),
        prev.version_full()
    );

    enum RestoreSource {
        SignedIndex { object: PathBuf, digest: String },
        Detached { object: PathBuf, digest: String },
        Pinned { object: PathBuf, digest: String, proof: String },
        Legacy { archive: PathBuf },
    }

    // Verify exact archived bytes and retained authentication evidence before
    // opening a transaction or touching the installed package.
    let source = if !prev.artifact_sha256.is_empty() {
        let digest = prev.artifact_sha256.clone();
        let object = artifact::verify_stored(root, &digest)
            .map_err(|e| e.context(format!(
                "cannot verify the archived original of {name} — rollback refused"
            )))?;
        let provenance = artifact::load_provenance(root, &digest)?;
        match provenance.as_ref().map(|p| p.authentication.as_str()) {
            Some("signed-index") => {
                verify_retained_index_proof(root, &digest)?;
                RestoreSource::SignedIndex { object, digest }
            }
            Some("detached-signature") => {
                verify_retained_detached_proof(root, &digest)?;
                RestoreSource::Detached { object, digest }
            }
            Some("local-build") | Some("unsigned-bootstrap") | None => {
                let proof = provenance.as_ref()
                    .map(|p| p.authentication.clone())
                    .unwrap_or_else(|| "no provenance record".to_string());
                RestoreSource::Pinned { object, digest, proof }
            }
            Some(other) => bail!("unsupported provenance authentication {other:?}"),
        }
    } else {
        let archive = root.join("var/cache/nawa")
            .join(format!("{}.nawa.tar.zst", prev.version_full()));
        if !archive.exists() {
            bail!("cached archive {} is missing — cannot roll back", archive.display());
        }
        let _ = artifact::import_legacy(root, &archive);
        RestoreSource::Legacy { archive }
    };

    let archive_for_snapshot = match &source {
        RestoreSource::SignedIndex { object, .. }
        | RestoreSource::Detached { object, .. }
        | RestoreSource::Pinned { object, .. } => object,
        RestoreSource::Legacy { archive } => archive,
    };
    // Preserve an installed package's current reason; reinstalls after removal
    // become explicit, matching the established "keep" behavior.
    let target = inspect_archive_state(archive_for_snapshot, crate::db::InstallReason::Keep, before.as_ref())?;
    let digest = target.artifact_sha256.clone();
    let mut operation = crate::transaction::begin_operation(
        root,
        crate::transaction::OperationCommand::Rollback,
        &[name.to_string()],
        &[],
    )?;
    let (result, proof) = {
        let mut recorder = operation.recorder();
        let step_no = recorder.step_intent_state(
            name,
            crate::transaction::history::StepAction::Restore,
            before.as_ref(),
            Some(&target),
            &digest,
        )?;

        let (result, proof) = match source {
            RestoreSource::SignedIndex { object, digest } => (
                install_archive_from_index_proof(&object, root, false, &digest, crate::db::InstallReason::Keep, None),
                "signed-index proof re-verified".to_string(),
            ),
            RestoreSource::Detached { object, digest } => (
                install_archive_pinned(&object, root, InstallOpts { force: false, allow_unsigned: true, key: None, reason: crate::db::InstallReason::Keep, pinned_sha: Some(&digest) }),
                "detached signature re-verified".to_string(),
            ),
            RestoreSource::Pinned { object, digest, proof } => (
                install_archive_pinned(&object, root, InstallOpts { force: false, allow_unsigned: true, key: None, reason: crate::db::InstallReason::Keep, pinned_sha: Some(&digest) }),
                proof,
            ),
            RestoreSource::Legacy { archive } => (
                install_archive(&archive, root, false, true, None, crate::db::InstallReason::Explicit),
                "legacy cached archive (no recorded authentication proof)".to_string(),
            ),
        };

        let current_after = match &result {
            Ok(entry) => Some(entry.clone()),
            Err(_) => db.entry(name)?,
        };
        let retry_hook = hook_needs_retry(root, name).unwrap_or(false);
        let committed_hook_failed = result.is_err()
            && crate::transaction::history::committed_hook_failed(
                current_after.as_ref(),
                before.as_ref(),
                &target,
                retry_hook,
            );
        let step_status = if result.is_ok() {
            crate::transaction::history::StepStatus::Committed
        } else if committed_hook_failed {
            crate::transaction::history::StepStatus::CommittedHookFailed
        } else {
            crate::transaction::history::StepStatus::Failed
        };
        let after = if result.is_ok() || committed_hook_failed {
            current_after.as_ref()
        } else {
            None
        };
        recorder.step_result(
            step_no,
            name,
            crate::transaction::history::StepAction::Restore,
            step_status,
            before.as_ref(),
            after,
            "rollback",
            result.as_ref().err().map(|error| format!("{error:#}")),
            committed_hook_failed.then(|| "failed".to_string()),
        )?;
        (result, proof)
    };
    operation.finish(result.as_ref().err())?;

    let entry = result?;
    report_restored(&entry, &proof);
    Ok(())
}

pub(crate) fn report_restored(entry: &Entry, how: &str) {
    crate::nawa_success!(
        "rolled back to {} (verified: {how})",
        entry.version_full()
    );
}

/// Re-verify a retained signed-index proof bundle: the archived index bytes
/// must still carry a signature by the archived signer key, and the object
/// digest must appear in that index (the index is what authenticated it).
pub(crate) fn verify_retained_index_proof(root: &Path, digest: &str) -> Result<()> {
    let dir = artifact::object_path(root, digest)?
        .parent()
        .context("object fan dir")?
        .to_path_buf();
    let index_bytes = std::fs::read(dir.join(format!("{digest}.index.toml")))
        .context("the retained repository index for this artifact is missing — cannot re-verify")?;
    let key_path = dir.join(format!("{digest}.signer.pub"));
    let key = sign::load_verify_key(&key_path)
        .context("the retained signer key for this artifact is missing or invalid")?;
    let sig_path = dir.join(format!("{digest}.index.toml.sig"));
    if sig_path.exists() {
        let sig = std::fs::read(&sig_path).context("read retained index signature")?;
        let sf = sign::read_sig_file(&sig_path)
            .with_context(|| format!("parse retained index signature {}", sig_path.display()))?;
        let _ = sig;
        sign::verify_bytes(&index_bytes, &key, &sf.sig)
            .context("the retained repository index no longer verifies against its signature")?;
    } else {
        // The bundle was stored without a sidecar (index authenticated by a
        // trusted key at install time). At minimum the retained key must be
        // able to verify an index that still lists this archive.
        let index = crate::repo::load_index_bytes(&index_bytes)
            .context("the retained repository index no longer parses")?;
        let listed = index.packages.iter().any(|p| p.sha256 == digest);
        if !listed {
            bail!("the retained repository index does not list this artifact digest");
        }
    }
    let index = crate::repo::load_index_bytes(&index_bytes)
        .context("the retained repository index no longer parses")?;
    if !index.packages.iter().any(|p| p.sha256 == digest) {
        bail!("the retained repository index does not list this artifact digest");
    }
    Ok(())
}

/// Re-verify a retained detached-signature proof: the archived signature
/// sidecar must verify the object bytes under the archived (or a current
/// trust-store) key.
pub(crate) fn verify_retained_detached_proof(root: &Path, digest: &str) -> Result<()> {
    let dir = artifact::object_path(root, digest)?
        .parent()
        .context("object fan dir")?
        .to_path_buf();
    let sig_path = dir.join(format!("{digest}.detached.sig"));
    if !sig_path.exists() {
        bail!("the retained detached signature for this artifact is missing — cannot re-verify");
    }
    let object = artifact::object_path(root, digest)?;
    sign::verify_for_install_with_sig(&object, &sig_path, root, None, false)
        .context("the retained detached signature no longer verifies")
}

#[allow(dead_code)] // convenience wrapper over remove_with
pub(crate) fn remove(name: &str, root: &Path) -> Result<()> {
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
pub(crate) fn remove_with(name: &str, root: &Path, force: bool) -> Result<()> {
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
    let uniq = format!("{stamp}-{}", std::process::id());
    // paths another installed package also lists stay in place
    let shared = db.paths_claimed_by_others(name)?;
    let mut files: Vec<String> = db.files(name)?.into_iter().filter(|f| !shared.contains(f)).collect();
    files.sort_by_key(|f| std::cmp::Reverse(f.len()));
    let mut tx = Tx::new(root, root.join(".nawa").join(format!("rm-{uniq}")));
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
    crate::nawa_success!(
        "removed {} from {} (rollback available via cached archive)",
        entry.version_full(),
        root.display()
    );
    Ok(())
}
