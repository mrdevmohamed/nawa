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
use std::path::Path;

use crate::artifact;
use crate::archive;
#[cfg(test)]
use crate::db::Db;
#[cfg(test)]
use std::path::PathBuf;
#[cfg(test)]
use crate::interrupt;
use crate::db::Entry;
use crate::sign;

/// How the bytes of the archive being installed were authenticated. Decides
/// the provenance record written next to the content-addressed object.
#[derive(Debug, Clone, Default)]
pub(crate) enum InstallAuth {
    /// Standalone archive path: detached signature (or bootstrap mode when no
    /// signature exists and the trust policy allowed it in).
    Detached {
        /// true when a signature verified against a trusted/pinned key
        trusted: bool,
    },
    /// Trusted v2/v3 repository index authenticated the archive digest. The
    /// exact index/signature/key bytes are retained as the proof bundle.
    SignedIndex {
        index_bytes: Option<Vec<u8>>,
        index_signature: Option<Vec<u8>>,
        signer_public: Option<Vec<u8>>,
        repo_id: Option<String>,
    },
    /// A local source build: trusted because this machine built it, not
    /// because it carries a signature.
    #[default]
    LocalBuild,
}
impl InstallAuth {
    fn kind(&self) -> artifact::Authentication {
        match self {
            InstallAuth::Detached { trusted } => {
                if *trusted {
                    artifact::Authentication::DetachedSignature
                } else {
                    artifact::Authentication::UnsignedBootstrap
                }
            }
            InstallAuth::SignedIndex { .. } => artifact::Authentication::SignedIndex,
            InstallAuth::LocalBuild => artifact::Authentication::LocalBuild,
        }
    }
}

/// Install a `.nawa.tar.zst` archive into `root`.
///
/// * `force`            — overwrite files owned by other packages
/// * `allow_unsigned`   — accept unsigned archives when trust keys exist
/// * `key`              — verify signature against this pubkey instead of the trust store
/// * `reason`           — "explicit" | "dependency" | "keep" (preserve old reason)
pub(crate) fn install_archive(
    archive_path: &Path,
    root: &Path,
    force: bool,
    allow_unsigned: bool,
    key: Option<&Path>,
    reason: crate::db::InstallReason,
) -> Result<Entry> {
    install_archive_pinned(archive_path, root, InstallOpts { force, allow_unsigned, key, reason, pinned_sha: None })
}

/// Options for [`install_archive_pinned`]: the install booleans and
/// the pinned digest live in one struct so call sites cannot
/// transpose `force`/`allow_unsigned` or pass a mistyped reason.
pub(crate) struct InstallOpts<'a> {
    pub force: bool,
    pub allow_unsigned: bool,
    pub key: Option<&'a Path>,
    pub reason: crate::db::InstallReason,
    pub pinned_sha: Option<&'a str>,
}

/// [`install_archive`] for an archive that a repository index already
/// described. With `pinned_sha` the snapshot must hash to the index
/// digest, so the bytes whose detached signature is checked are provably the
/// bytes the index entry (and `.PKGINFO` check) was validated against.
pub(crate) fn install_archive_pinned(
    archive_path: &Path,
    root: &Path,
    opts: InstallOpts<'_>,
) -> Result<Entry> {
    let InstallOpts { force, allow_unsigned, key, reason, pinned_sha } = opts;
    let snapshot_dir = root.join(".nawa");
    let snapshot = crate::fetch::snapshot_file(archive_path, pinned_sha, &snapshot_dir)?;
    let sig = sign::sig_path_for(archive_path);
    let verified = sign::verify_for_install_with_sig_reported(
        snapshot.path(),
        &sig,
        root,
        key,
        allow_unsigned,
    )?;
    // Only a signature that exists AND verified carries provenance;
    // bootstrap-mode acceptance (no sidecar at all) does not.
    let auth = if sig.exists() && verified {
        InstallAuth::Detached { trusted: true }
    } else {
        InstallAuth::Detached { trusted: false }
    };
    install_archive_payload(
        snapshot.path(),
        archive_path,
        root,
        force,
        reason,
        true,
        auth,
    )
}

/// Install an archive authenticated by a trusted v2 repository index, keeping
/// the exact proof bytes (the verified index, its signature sidecar and the
/// signer public key).
///
/// The expected digest comes from the already-verified signed index. It is
/// rechecked immediately before extraction so the file cannot silently change
/// between repository fetch/validation and the install operation. This path
/// is intentionally separate from standalone archive installation. The
/// proof is persisted next to the content-addressed object so a later
/// rollback/undo can re-verify instead of trusting the cache blindly.
pub(crate) fn install_archive_from_index_proof(
    archive_path: &Path,
    root: &Path,
    force: bool,
    expected_sha256: &str,
    reason: crate::db::InstallReason,
    proof: Option<IndexProof<'_>>,
) -> Result<Entry> {
    let snapshot_dir = root.join(".nawa");
    let snapshot = crate::fetch::snapshot_file(archive_path, Some(expected_sha256), &snapshot_dir)?;
    let auth = match proof {
        Some(p) => InstallAuth::SignedIndex {
            index_bytes: p.index_bytes.map(<[u8]>::to_vec),
            index_signature: p.index_signature.map(<[u8]>::to_vec),
            signer_public: p.signer_public.map(<[u8]>::to_vec),
            repo_id: p.repo_id.map(str::to_string),
        },
        None => InstallAuth::SignedIndex {
            index_bytes: None,
            index_signature: None,
            signer_public: None,
            repo_id: None,
        },
    };
    install_archive_payload(snapshot.path(), archive_path, root, force, reason, false, auth)
}

/// Borrowed proof bundle for a trusted-index install.
pub(crate) struct IndexProof<'a> {
    pub index_bytes: Option<&'a [u8]>,
    pub index_signature: Option<&'a [u8]>,
    pub signer_public: Option<&'a [u8]>,
    pub repo_id: Option<&'a str>,
}

/// Inspect package identity and archive digest before installation. This state
/// is persisted in the WAL intent so recovery can distinguish installs from
/// removals without guessing.
pub(crate) fn inspect_archive_state(
    archive_path: &Path,
    reason: crate::db::InstallReason,
    previous: Option<&Entry>,
) -> Result<crate::transaction::history::PackageState> {
    let raw = archive::read_pkginfo(archive_path)
        .with_context(|| format!("read package metadata from {}", archive_path.display()))?;
    let info = archive::parse_pkginfo(&raw).context("parse archive package metadata")?;
    let name = info.get("pkgname").cloned().unwrap_or_default();
    let version = info.get("pkgver").cloned().unwrap_or_default();
    crate::db::check_name(&name)?;
    if !crate::db::valid_version(&version) {
        bail!("archive {} has an invalid package version {:?}", archive_path.display(), version);
    }
    let rel = info.get("pkgrel").and_then(|value| value.parse::<u32>().ok())
        .filter(|value| *value > 0)
        .ok_or_else(|| anyhow::anyhow!("archive {} has an invalid pkgrel", archive_path.display()))?;
    let arch = info.get("arch").cloned().unwrap_or_default();
    let digest = crate::fetch::sha256_of_file(archive_path)
        .with_context(|| format!("hash package archive {}", archive_path.display()))?;
    let effective_reason = if reason == crate::db::InstallReason::Keep {
        previous.map(|entry| entry.reason.clone()).unwrap_or_else(|| "explicit".to_string())
    } else {
        reason.as_str().to_string()
    };
    Ok(crate::transaction::history::PackageState {
        version: Some(version),
        rel: Some(rel),
        arch,
        reason: effective_reason,
        artifact_sha256: digest,
    })
}


pub mod hooks;
pub mod journal;
pub mod payload;
pub mod rollback;

// ---- re-exports preserving the pre-split API ------------------------------
#[allow(unused_imports)]
pub(crate) use hooks::{
    exists_in_root, hook_needs_retry, hook_status_path, resolve_in_root, retry_pending_hook,
    run_tracked_hook, save_hooks, save_hook_status,
};
#[allow(unused_imports)]
pub(crate) use journal::{
    dec, enc, recover, recover_and_report, Tx,
};
#[allow(unused_imports)]
pub(crate) use payload::{
    ensure_inside_root, install_archive_payload, save_undo, stage_tree,
};
#[allow(unused_imports)]
pub(crate) use rollback::{remove, remove_with, rollback, verify_retained_detached_proof, verify_retained_index_proof};
#[allow(unused_imports)]
pub(crate) use hooks::HookStatus;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive;

    use crate::testutil::tmpdir;

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
            reason: "explicit".to_string(),
            run: vec![],
            install_date: 1,
            size: 1,
            artifact_sha256: String::new(),
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

    /// merged-usr base package: `bin -> usr/bin` plus one real file
    fn make_base_with_bin_symlink(dir: &Path, version: &str) -> PathBuf {
        let staging = dir.join(format!("staging-base-{version}"));
        std::fs::create_dir_all(staging.join("usr/bin")).unwrap();
        std::fs::write(staging.join("usr/bin/sh"), format!("sh {version}")).unwrap();
        std::os::unix::fs::symlink("usr/bin", staging.join("bin")).unwrap();
        let pkginfo = format!(
            "pkgname = base\npkgver = {version}\npkgrel = 1\narch = x86_64\nrundeps = \nsize = 1\n"
        );
        let out = dir.join(format!("base-{version}-1.nawa.tar.zst"));
        archive::create(&staging, &pkginfo, &out).unwrap();
        out
    }

    /// Record `name` as installed with a database claim on `/bin` — the shape
    /// the pre-merged-usr shadow package left behind (a `/bin` directory entry).
    fn claim_bin_dir(root: &Path, name: &str) {
        let entry = Entry {
            name: name.into(),
            version: "1.0".into(),
            rel: 1,
            arch: "x86_64".into(),
            reason: "explicit".to_string(),
            run: vec![],
            install_date: 1,
            size: 1,
            artifact_sha256: String::new(),
        };
        Db::new(root)
            .add(&entry, &["/bin".into(), "/usr/bin/su".into()], &std::collections::HashMap::new())
            .unwrap();
    }

    #[test]
    fn identical_symlink_is_not_a_conflict_with_another_packages_claim() {
        let base = tmpdir("same-symlink");
        let root = base.join("root");
        install_archive(&make_base_with_bin_symlink(&base, "1"), &root, false, true, None, crate::db::InstallReason::Explicit).unwrap();
        claim_bin_dir(&root, "zshadow");
        // upgrade of the base package re-ships the very same /bin link
        install_archive(&make_base_with_bin_symlink(&base, "2"), &root, false, true, None, crate::db::InstallReason::Explicit)
            .expect("an identical symlink must not be reported as a file conflict");
        assert_eq!(std::fs::read_link(root.join("bin")).unwrap(), Path::new("usr/bin"));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_different_symlink_target_is_still_a_conflict() {
        let base = tmpdir("other-symlink");
        let root = base.join("root");
        claim_bin_dir(&root, "zshadow");
        std::fs::create_dir_all(&root).unwrap();
        std::os::unix::fs::symlink("elsewhere", root.join("bin")).unwrap();
        let err = install_archive(&make_base_with_bin_symlink(&base, "1"), &root, false, true, None, crate::db::InstallReason::Explicit)
            .unwrap_err();
        assert!(format!("{err:#}").contains("/bin (owned by zshadow)"), "{err:#}");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn upgrade_keeps_paths_another_package_still_lists() {
        let base = tmpdir("shared-upgrade");
        let root = base.join("root");
        install_archive(&make_base_with_bin_symlink(&base, "1"), &root, false, true, None, crate::db::InstallReason::Explicit).unwrap();
        claim_bin_dir(&root, "zshadow");
        // zshadow 2.0 no longer ships /bin: the old entry is obsolete for it,
        // but base's /bin symlink must survive
        let v2 = make_pkg(&base, "zshadow", "2.0", &[("/usr/bin/su", b"su2")]);
        install_archive(&v2, &root, true, true, None, crate::db::InstallReason::Explicit).unwrap();
        assert_eq!(std::fs::read_link(root.join("bin")).unwrap(), Path::new("usr/bin"));
        assert!(root.join("usr/bin/su").is_file());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn remove_keeps_paths_another_package_still_lists() {
        let base = tmpdir("shared-remove");
        let root = base.join("root");
        install_archive(&make_base_with_bin_symlink(&base, "1"), &root, false, true, None, crate::db::InstallReason::Explicit).unwrap();
        claim_bin_dir(&root, "zshadow");
        remove_with("zshadow", &root, true).unwrap();
        assert_eq!(std::fs::read_link(root.join("bin")).unwrap(), Path::new("usr/bin"));
        assert!(root.join("usr/bin/sh").is_file());
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

        install_archive(&v1, &root, false, true, None, crate::db::InstallReason::Explicit).unwrap();
        assert_eq!(std::fs::read(root.join("usr/bin/demo")).unwrap(), b"v1");

        install_archive(&v2, &root, false, true, None, crate::db::InstallReason::Explicit).unwrap();
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

        install_archive(&v1, &root, false, true, None, crate::db::InstallReason::Explicit).unwrap();
        // admin edits the config
        std::fs::write(root.join("etc/srv.conf"), b"locally edited\n").unwrap();

        install_archive(&v2, &root, false, true, None, crate::db::InstallReason::Explicit).unwrap();

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
        install_archive(&v1, &root, false, true, None, crate::db::InstallReason::Explicit).unwrap();
        install_archive(&v2, &root, false, true, None, crate::db::InstallReason::Explicit).unwrap();
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
        install_archive(&v1, &root, false, true, None, crate::db::InstallReason::Explicit).unwrap();
        install_archive(&v2, &root, false, true, None, crate::db::InstallReason::Explicit).unwrap();
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
        install_archive(&v1, &root, false, true, None, crate::db::InstallReason::Explicit).unwrap();
        // wipe the baseline to emulate a pre-v0.5 database
        let db = Db::new(&root);
        std::fs::remove_file(db.local_dir().join("srv/hashes")).unwrap();
        install_archive(&v2, &root, false, true, None, crate::db::InstallReason::Explicit).unwrap();
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

        install_archive(&v1, &root, false, true, None, crate::db::InstallReason::Explicit).unwrap();
        install_archive(&v2, &root, false, true, None, crate::db::InstallReason::Explicit).unwrap();
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
        install_archive(&v1, &root, false, true, None, crate::db::InstallReason::Explicit).unwrap();
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
        install_archive(&v1, &root, false, true, None, crate::db::InstallReason::Dependency).unwrap();
        let db = Db::new(&root);
        assert_eq!(db.entry("r").unwrap().unwrap().reason, "dependency");
        install_archive(&v2, &root, false, true, None, crate::db::InstallReason::Keep).unwrap();
        assert_eq!(db.entry("r").unwrap().unwrap().reason, "dependency");
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
        install_archive(&lib, &root, false, true, None, crate::db::InstallReason::Explicit).unwrap();
        install_archive(&app, &root, false, true, None, crate::db::InstallReason::Explicit).unwrap();

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

        assert!(install_archive(&pkg, &root, false, true, None, crate::db::InstallReason::Explicit).is_err());
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
        let err = install_archive_pinned(&archive, &root, InstallOpts { force: false, allow_unsigned: true, key: None, reason: crate::db::InstallReason::Explicit, pinned_sha: Some(&wrong) })
            .unwrap_err();
        assert!(format!("{err:#}").contains("sha256 mismatch"), "{err:#}");
        assert!(!root.join("usr/share/pinned-demo").exists());

        install_archive_pinned(&archive, &root, InstallOpts { force: false, allow_unsigned: true, key: None, reason: crate::db::InstallReason::Explicit, pinned_sha: Some(&digest.to_uppercase()) })
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
        let err = install_archive(&pkg, &root, false, true, None, crate::db::InstallReason::Explicit).unwrap_err();
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
        install_archive(&v1, &root, false, true, None, crate::db::InstallReason::Explicit).unwrap();

        interrupt::simulate_interrupt();
        let err = install_archive(&v2, &root, false, true, None, crate::db::InstallReason::Explicit).unwrap_err();
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
        install_archive(&v1, &root, false, true, None, crate::db::InstallReason::Explicit).unwrap();

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
        install_archive(&v1, &root, false, true, None, crate::db::InstallReason::Explicit).unwrap();
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

    // ---- Phase 0 (roadmap §4): artifact identity, provenance, rollback ----

    #[test]
    fn install_records_artifact_reference_and_seconds_timestamp() {
        let base = tmpdir("artifact-ref");
        let root = base.join("root");
        let v1 = make_pkg(&base, "demo", "1.0", &[("/usr/bin/demo", b"v1")]);
        let digest = crate::fetch::sha256_of_file(&v1).unwrap();
        let before = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        install_archive(&v1, &root, false, true, None, crate::db::InstallReason::Explicit).unwrap();

        let db = Db::new(&root);
        let e = db.entry("demo").unwrap().unwrap();
        assert_eq!(e.artifact_sha256, digest, "the exact bytes are referenced by digest");
        // install_date is Unix seconds, within a sane window around "now"
        assert!(e.install_date >= before && e.install_date <= before + 60,
            "install_date {} is not seconds (before={before})", e.install_date);
        // the content-addressed object exists and verifies
        let obj = crate::artifact::object_path(&root, &digest).unwrap();
        assert!(obj.exists(), "object missing at {}", obj.display());
        assert!(crate::artifact::verify_stored(&root, &digest).is_ok());
        // a provenance record describes the authentication
        let prov = crate::artifact::load_provenance(&root, &digest).unwrap().unwrap();
        assert_eq!(prov.authentication, "unsigned-bootstrap");
        assert_eq!(prov.package.unwrap().name, "demo");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn rollback_rejects_corrupted_object_bytes() {
        let base = tmpdir("artifact-corrupt");
        let root = base.join("root");
        let v1 = make_pkg(&base, "demo", "1.0", &[("/usr/bin/demo", b"v1")]);
        let v2 = make_pkg(&base, "demo", "2.0", &[("/usr/bin/demo", b"v2")]);
        install_archive(&v1, &root, false, true, None, crate::db::InstallReason::Explicit).unwrap();
        install_archive(&v2, &root, false, true, None, crate::db::InstallReason::Explicit).unwrap();
        // tamper with the stored object of the OLD version
        let old_digest = crate::fetch::sha256_of_file(&v1).unwrap();
        let obj = crate::artifact::object_path(&root, &old_digest).unwrap();
        std::fs::write(&obj, b"tampered bytes").unwrap();
        assert!(rollback("demo", &root).is_err(), "a corrupted cached archive must not be restorable");
        // the system stays at 2.0
        assert_eq!(std::fs::read(root.join("usr/bin/demo")).unwrap(), b"v2");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn rollback_rejects_missing_object() {
        let base = tmpdir("artifact-missing");
        let root = base.join("root");
        let v1 = make_pkg(&base, "demo", "1.0", &[("/usr/bin/demo", b"v1")]);
        let v2 = make_pkg(&base, "demo", "2.0", &[("/usr/bin/demo", b"v2")]);
        install_archive(&v1, &root, false, true, None, crate::db::InstallReason::Explicit).unwrap();
        install_archive(&v2, &root, false, true, None, crate::db::InstallReason::Explicit).unwrap();
        let old_digest = crate::fetch::sha256_of_file(&v1).unwrap();
        let obj = crate::artifact::object_path(&root, &old_digest).unwrap();
        std::fs::remove_file(&obj).unwrap();
        // and hide the legacy named cache copy so only the object could have served
        let _ = std::fs::remove_file(root.join("var/cache/nawa/demo-1.0-1.nawa.tar.zst"));
        assert!(rollback("demo", &root).is_err(), "rollback must refuse without the original bytes");
        assert_eq!(std::fs::read(root.join("usr/bin/demo")).unwrap(), b"v2");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn rollback_restores_through_verified_object_and_ping_pongs() {
        let base = tmpdir("artifact-rollback");
        let root = base.join("root");
        let v1 = make_pkg(&base, "demo", "1.0", &[("/usr/bin/demo", b"v1")]);
        let v2 = make_pkg(&base, "demo", "2.0", &[("/usr/bin/demo", b"v2")]);
        install_archive(&v1, &root, false, true, None, crate::db::InstallReason::Explicit).unwrap();
        install_archive(&v2, &root, false, true, None, crate::db::InstallReason::Explicit).unwrap();
        // remove the legacy named caches: the object store is now the only source
        let _ = std::fs::remove_file(root.join("var/cache/nawa/demo-1.0-1.nawa.tar.zst"));
        let _ = std::fs::remove_file(root.join("var/cache/nawa/demo-2.0-1.nawa.tar.zst"));

        rollback("demo", &root).unwrap();
        assert_eq!(std::fs::read(root.join("usr/bin/demo")).unwrap(), b"v1");
        let db = Db::new(&root);
        assert_eq!(db.entry("demo").unwrap().unwrap().version, "1.0");
        // the restored entry itself references the verified object
        let digest1 = crate::fetch::sha256_of_file(&v1).unwrap();
        assert_eq!(db.entry("demo").unwrap().unwrap().artifact_sha256, digest1);
        // ping-pong still works through the object store
        rollback("demo", &root).unwrap();
        assert_eq!(std::fs::read(root.join("usr/bin/demo")).unwrap(), b"v2");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn rollback_is_recorded_and_can_be_undone() {
        let base = tmpdir("rollback-history");
        let root = base.join("root");
        let v1 = make_pkg(&base, "demo", "1.0", &[("/usr/bin/demo", b"v1")]);
        let v2 = make_pkg(&base, "demo", "2.0", &[("/usr/bin/demo", b"v2")]);
        install_archive(&v1, &root, false, true, None, crate::db::InstallReason::Explicit).unwrap();
        install_archive(&v2, &root, false, true, None, crate::db::InstallReason::Explicit).unwrap();

        rollback("demo", &root).unwrap();
        let records = crate::transaction::history::list_records(&root).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].command, "rollback");
        assert_eq!(records[0].status, "completed");
        assert_eq!(records[0].package_changes.len(), 1);
        assert_eq!(records[0].package_changes[0].action, "restore");
        assert_eq!(records[0].package_changes[0].source, "rollback");

        let plan = crate::transaction::undo::plan_undo(&root, records[0].id).unwrap();
        assert_eq!(plan.steps.len(), 1);
        assert!(matches!(
            &plan.steps[0].action,
            crate::transaction::undo::UndoAction::RestorePrevious { .. }
        ));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn signed_index_install_records_repository_proof() {
        let base = tmpdir("index-proof");
        let root = base.join("root");
        // build a repo with a signed v2 index
        let repo_dir = base.join("repo");
        std::fs::create_dir_all(&repo_dir).unwrap();
        let v1 = make_pkg(&base, "demo", "1.0", &[("/usr/bin/demo", b"v1")]);
        std::fs::copy(&v1, repo_dir.join("demo-1.0-1.nawa.tar.zst")).unwrap();
        let index_path = repo_dir.join("index.toml");
        crate::repo::create_index_with_version(&repo_dir, &index_path, None, crate::repo::INDEX_SCHEMA_V2).unwrap();
        let (secret, _public) = crate::sign::keygen(&base.join("publisher")).unwrap();
        crate::sign::sign(&index_path, &secret).unwrap();
        // trust the publisher
        let (_, pub_path) = crate::sign::keygen(&base.join("publisher2")).unwrap();
        let _ = pub_path;
        // trust via the repo's own key: copy the publisher public key into the trust store
        let pub_key = std::fs::read_to_string(secret.with_file_name("nawa.pub")).unwrap();
        let trust_file = base.join("publisher.pub");
        std::fs::write(&trust_file, pub_key).unwrap();
        crate::sign::trust(&trust_file, &root).unwrap();

        let open = crate::upgrade::open_repo(&root, &crate::fetch::RepoRef::Local(repo_dir.clone()), false, false).unwrap();
        let re = open.index.find("demo").unwrap().clone();
        let archive = crate::upgrade::fetch_entry(&root, &open, &re, false).unwrap();
        let index_bytes = std::fs::read(&index_path).unwrap();
        let sig_bytes = std::fs::read(index_path.with_extension("toml.sig")).ok();
        // reconstruct the signer public bytes for the proof bundle
        let signer_pub: Vec<u8> = std::fs::read(secret.with_file_name("nawa.pub")).unwrap();
        install_archive_from_index_proof(
            &archive, &root, false, &re.sha256, crate::db::InstallReason::Explicit,
            Some(crate::install::IndexProof {
                index_bytes: Some(&index_bytes),
                index_signature: sig_bytes.as_deref(),
                signer_public: Some(&signer_pub),
                repo_id: Some("local-test"),
            }),
        ).unwrap();

        let digest = re.sha256.clone();
        let prov = crate::artifact::load_provenance(&root, &digest).unwrap().unwrap();
        assert_eq!(prov.authentication, "signed-index");
        let repo_proof = prov.repository.unwrap();
        assert_eq!(repo_proof.id, "local-test");
        assert!(repo_proof.trusted_at_install);
        // the proof bundle retains the index, signature and signer key
        let dir = crate::artifact::object_path(&root, &digest).unwrap().parent().unwrap().to_path_buf();
        assert!(dir.join(format!("{digest}.index.toml")).exists());
        assert!(dir.join(format!("{digest}.index.toml.sig")).exists());
        assert!(dir.join(format!("{digest}.signer.pub")).exists());
        let db = Db::new(&root);
        assert_eq!(db.entry("demo").unwrap().unwrap().artifact_sha256, digest);
        let _ = std::fs::remove_dir_all(&base);
    }

}
