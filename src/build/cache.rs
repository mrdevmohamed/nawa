//! Build cache identity and lookup.

use anyhow::Result;
use std::path::{Path, PathBuf};

use super::{BuildOpts, BuildResult};
use crate::recipe::Recipe;
use crate::archive;
use crate::slim;

/// The build id for `rec` as `build()` would record it: recipe bytes, pinned
/// source sha256, the closure archive set (names pin versions) and flags.
/// The build id for this recipe/opts/closure combination.
/// `asset_digest` carries the `[assets]` probe (cache decisions) or the
/// snapshot digest (recording, §5.3 "two moments, one digest") — `None`
/// keeps the input set byte-for-byte legacy (§10.3).
pub(crate) fn build_id_for(
    rec: &Recipe,
    opts: &BuildOpts,
    closure_archives: &[PathBuf],
    asset_digest: Option<&str>,
    toolchain_identity: &[(&'static str, String)],
) -> String {
    // slimming changes the archive, so its rules are an input of the id
    let flags = if opts.slim {
        format!("{} slim={}", opts.profile_flags, slim::VERSION)
    } else {
        format!("{} slim=off", opts.profile_flags)
    };
    // §10.3: for recipes with `package.sources` the source key becomes the
    // sorted `name=sha256` list (the recipe bytes already pin every other
    // source field). Legacy recipes keep today's single sha256 byte-for-byte.
    let source_sha256 = if rec.package.sources.is_empty() {
        rec.package.sha256.clone()
    } else {
        let mut pairs: Vec<String> = rec
            .package
            .sources
            .iter()
            .map(|s| format!("{}={}", s.name, s.sha256))
            .collect();
        pairs.sort();
        pairs.join(",")
    };
    // §10.3 extras, in pinned order (assets, fetch backends, then actual
    // toolchain versions for non-closure builds). Legacy ids remain identical.
    let mut extras: Vec<(&str, &str)> = asset_digest
        .map(|d| vec![("assets", d)])
        .unwrap_or_default();
    if rec.fetch.go.is_some() {
        extras.push(("fetch-go", crate::fetchrun::FETCH_GO_SCHEMA_VERSION));
    }
    if rec.fetch.cargo.is_some() {
        extras.push(("fetch-cargo", crate::fetchrun::FETCH_CARGO_SCHEMA_VERSION));
    }
    if let Some((_, version)) = toolchain_identity.iter().find(|(name, _)| *name == "go") {
        extras.push(("toolchain-go", version.as_str()));
    }
    if let Some((_, version)) = toolchain_identity.iter().find(|(name, _)| *name == "cargo") {
        extras.push(("toolchain-cargo", version.as_str()));
    }
    if extras.is_empty() {
        crate::lint::build_id(
            rec.raw.as_bytes(),
            &source_sha256,
            closure_archives,
            &flags,
        )
    } else {
        crate::lint::build_id_with(
            rec.raw.as_bytes(),
            &source_sha256,
            closure_archives,
            &flags,
            &extras,
        )
    }
}


/// The per-build fetch views the recipe's `[fetch.*]` tables bring into
/// the sandbox as writable extra binds (§8/§11): `<work>/fetch/<eco>` →
/// `<build_inside>/fetch/<eco>`, in ecosystem-table order for determinism.
pub(crate) fn fetch_ecosystems(rec: &Recipe) -> Vec<&'static str> {
    let mut v: Vec<&'static str> = Vec::new();
    if rec.fetch.go.is_some() {
        v.push("go");
    }
    if rec.fetch.cargo.is_some() {
        v.push("cargo");
    }
    v
}


/// `<out>/<archive>.buildid` — the sidecar `--use-cache` compares against.
pub(crate) fn buildid_sidecar(out_path: &Path) -> PathBuf {
    out_path.with_file_name(format!(
        "{}.buildid",
        out_path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default()
    ))
}


/// Return the existing archive when its sidecar build id matches the id the
/// current inputs would produce (§7: "is this archive up to date?" becomes
/// a hash comparison). `Ok(None)` = no hit, build as usual.
///
/// §5.3 "two moments, one digest": the assets extra here is the *probe*
/// digest read from the checkout — computing it needs no `<work>` dir. A
/// probe error (missing/symlinked asset) simply means no cache hit; the
/// build then fails in `assets::precheck` with the real message.
pub(crate) fn try_cache_hit(
    rec: &Recipe,
    opts: &BuildOpts,
    closure_archives: &[PathBuf],
    toolchain_identity: &[(&'static str, String)],
) -> Result<Option<BuildResult>> {
    let out_path = opts.outdir.join(format!("{}.nawa.tar.zst", rec.id()));
    let sidecar = buildid_sidecar(&out_path);
    if !out_path.is_file() || !sidecar.is_file() {
        return Ok(None);
    }
    let stored = std::fs::read_to_string(&sidecar).unwrap_or_default();
    let asset_digest: Option<String> = if rec.assets.include.is_empty() {
        None
    } else {
        crate::assets::probe_digest(rec).ok()
    };
    let bid = build_id_for(rec, opts, closure_archives, asset_digest.as_deref(), toolchain_identity);
    if stored.trim() != bid {
        crate::nawa_info!("build cache: {} changed (build_id mismatch) — rebuilding", rec.id());
        return Ok(None);
    }
    // A cache hit must still be a readable package archive.
    archive::read_pkginfo(&out_path)?;
    crate::nawa_info!(
        "build cache hit: {} (build_id {bid}) — nothing to do",
        out_path.display()
    );
    Ok(Some(BuildResult { archive: out_path }))
}

