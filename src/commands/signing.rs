//! Repository authoring and signing commands: `repo-create`, `repo-update`,
//! `keygen`, `sign`, `trust`. They only touch files given on the command
//! line (and the trust directory), never the package database.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

use crate::ctx::Ctx;
use crate::{repo, sign};

/// `nawa repo-create`: index a directory of archives, optionally signed.
#[allow(clippy::too_many_arguments)]
pub(crate) fn cmd_repo_create(
    dir: &Path,
    out: PathBuf,
    sign: bool,
    key: Option<&Path>,
    index_version: u32,
    locations: Option<&Path>,
    recipes: Option<&Path>,
    recipes_rev: Option<&str>,
    groups: Option<&Path>,
) -> Result<()> {
    if index_version >= repo::INDEX_SCHEMA_V2 && !sign {
        bail!("repo-create --index-version {index_version} requires --sign; repository indexes v2 and newer must be authenticated");
    }
    if index_version == repo::INDEX_SCHEMA_V3 && locations.is_none() {
        bail!("repo-create --index-version 3 requires --locations");
    }
    if index_version != repo::INDEX_SCHEMA_V3 && locations.is_some() {
        bail!("repo-create --locations is only valid with --index-version 3");
    }
    if groups.is_some() && index_version < repo::INDEX_SCHEMA_V2 {
        bail!("repo-create --groups requires --index-version 2 or 3");
    }
    // Resolve the key before writing anything, so a missing signing
    // key cannot leave an unsigned v2 index in the repository.
    let signing_key = if sign {
        let path = sign::resolve_signing_key(key)?;
        sign::load_signing_key(&path)
            .with_context(|| format!("invalid repository signing key {}", path.display()))?;
        Some(path)
    } else {
        None
    };
    // A relative --out lands inside the repo dir, not in the CWD.
    let out = if out.is_absolute() { out } else { dir.join(out) };
    let tree = match (recipes, recipes_rev) {
        (Some(t), Some(r)) => Some((t, r)),
        _ => None,
    };
    repo::create_index_with_locations_and_groups(
        dir, &out, tree, index_version, locations, groups
    )?;
    if let Some(kp) = signing_key {
        let sp = sign::sign(&out, &kp)?;
        crate::nawa_info!("signed index -> {}", sp.display());
    }
    Ok(())

}

/// `nawa repo-update`: derive a new signed index from a verified base index.
#[allow(clippy::too_many_arguments)]
pub(crate) fn cmd_repo_update(
    dir: &Path,
    base: &Path,
    trust_key: &Path,
    out: PathBuf,
    add_entries: Option<&Path>,
    remove: Vec<String>,
    drop: Vec<String>,
    locations: Option<&Path>,
    recipes: Option<&Path>,
    recipes_rev: Option<&str>,
    key: Option<&Path>,
) -> Result<()> {
    // Resolve the signing key first: an update is always signed, and a
    // missing key must not leave an unsigned index behind.
    let signing_key = sign::resolve_signing_key(key)?;
    sign::load_signing_key(&signing_key)
        .with_context(|| format!("invalid repository signing key {}", signing_key.display()))?;
    // Verify the exact bytes that are then parsed: an unverified base
    // must never flow into a signed index.
    let vk = sign::load_verify_key(trust_key)?;
    let base_bytes = std::fs::read(base)
        .with_context(|| format!("read base index {}", base.display()))?;
    let sf = sign::read_sig_file(&sign::sig_path_for(base))?;
    sign::verify_bytes(&base_bytes, &vk, &sf.sig).with_context(|| {
        format!("base index {} is not signed by {}; refusing to update an unverified index", base.display(), trust_key.display())
    })?;
    let base_index = repo::load_index_bytes(&base_bytes)
        .with_context(|| format!("parse base index {}", base.display()))?;
    let out = if out.is_absolute() { out } else { dir.join(out) };
    let tree = match (recipes, recipes_rev) {
        (Some(t), Some(r)) => Some((t, r)),
        _ => None,
    };
    repo::update_index_files(
        &base_index,
        dir,
        &out,
        repo::UpdateIndexFilesInputs {
            entries_path: add_entries,
            remove_files: remove,
            drop_packages: drop,
            locations_path: locations,
            recipes: tree,
        },
    )?;
    let sp = sign::sign(&out, &signing_key)?;
    crate::nawa_info!("signed index -> {}", sp.display());
    Ok(())

}

/// `nawa keygen`
pub(crate) fn cmd_keygen(out: Option<PathBuf>) -> Result<()> {
    let dir = out.unwrap_or_else(sign::default_key_dir);
    let (sec, pub_) = sign::keygen(&dir)?;
    crate::nawa_info!("secret key  {}  (keep safe, mode 0600)", sec.display());
    crate::nawa_info!("public key  {}  (share / `nawa trust` on targets)", pub_.display());
    Ok(())

}

/// `nawa sign`
pub(crate) fn cmd_sign(file: &Path, key: Option<&Path>) -> Result<()> {
    let kp = sign::resolve_signing_key(key)?;
    let sp = sign::sign(file, &kp)?;
    crate::nawa_info!("signed {} -> {}", file.display(), sp.display());
    Ok(())

}

/// `nawa trust`
pub(crate) fn cmd_trust(ctx: &Ctx, keyfile: &Path) -> Result<()> {
    let dest = sign::trust(keyfile, &ctx.root)?;
    crate::nawa_info!("trusted key installed at {}", dest.display());
    Ok(())

}
