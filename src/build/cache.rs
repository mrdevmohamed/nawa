//! Build cache identity and lookup.

use anyhow::Result;
use std::path::{Path, PathBuf};

use super::{BuildOpts, BuildResult};
use crate::buildenv::{self, Isolation};
use crate::recipe::Recipe;
use crate::archive;
use crate::slim;

/// Version tag of the `--use-cache` sidecar id. The tag is also part of the
/// hash domain, so bumping it deliberately invalidates every existing sidecar
/// (one rebuild) when the identity inputs or their encoding change. The legacy
/// `.PKGINFO build_id` is deliberately unaffected.
pub(crate) const CACHE_ID_VERSION: &str = "v2";

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


/// The versioned id written to `<out>/<archive>.buildid` for `--use-cache`.
///
/// It is built on top of the legacy [`build_id_for`] value, so every existing
/// identity input keeps contributing, and adds the two inputs the legacy id
/// deliberately omits: the effective isolation level and the host C/C++
/// toolchain. `closure` builds pin their toolchain through the dependency
/// closure archives, so [`host_toolchain_identity`] returns nothing there.
pub(crate) fn cache_id_for(
    build_id: &str,
    isolation: Isolation,
    host_toolchain: &[(&'static str, String)],
) -> String {
    use sha2::Digest;
    let mut h = sha2::Sha256::new();
    // The version is both the output prefix and the first hash input: a
    // future encoding change cannot alias an old sidecar even by accident.
    h.update(CACHE_ID_VERSION.as_bytes());
    h.update(b"\0build-id=");
    h.update(build_id.as_bytes());
    h.update(b"\0isolation=");
    h.update(isolation.as_str().as_bytes());
    for (label, value) in host_toolchain {
        h.update(b"\0");
        h.update(label.as_bytes());
        h.update(b"=");
        h.update(value.as_bytes());
    }
    format!("{CACHE_ID_VERSION}:{}", hex::encode(h.finalize()))
}


/// Identity of the host C/C++ toolchain for the `--use-cache` sidecar at
/// `env` and `sandbox` isolation. Probes the exact `PATH` build sections see
/// ([`buildenv::BUILD_PATH`]) and skips commands that are absent or fail: a
/// host without `make`, for example, simply contributes no entry.
pub(crate) fn host_toolchain_identity(isolation: Isolation) -> Vec<(&'static str, String)> {
    if isolation < Isolation::Env || isolation == Isolation::Closure {
        return Vec::new();
    }
    let mut identity = Vec::new();
    for (name, args) in [
        ("cc", ["--version"]),
        ("c++", ["--version"]),
        ("ld", ["--version"]),
        ("make", ["--version"]),
    ] {
        let Ok(output) = std::process::Command::new(name)
            .args(args)
            .env_clear()
            .env("PATH", buildenv::BUILD_PATH)
            .env("LC_ALL", "C")
            .output()
        else {
            continue;
        };
        if !output.status.success() {
            continue;
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let version = if stdout.trim().is_empty() {
            stderr.trim()
        } else {
            stdout.trim()
        };
        if !version.is_empty() {
            identity.push((name, version.to_string()));
        }
    }
    identity
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
    host_toolchain: &[(&'static str, String)],
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
    let cache_id = cache_id_for(&bid, opts.isolation, host_toolchain);
    if stored.trim() != cache_id {
        crate::nawa_info!("build cache: {} changed (build_id mismatch) — rebuilding", rec.id());
        return Ok(None);
    }
    // A cache hit must still be a readable package archive.
    archive::read_pkginfo(&out_path)?;
    crate::nawa_info!(
        "build cache hit: {} (cache id {cache_id}) — nothing to do",
        out_path.display()
    );
    Ok(Some(BuildResult { archive: out_path }))
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_id_is_versioned_and_covers_isolation_and_host_toolchain() {
        let env = cache_id_for("legacy-id", Isolation::Env, &[]);
        assert!(env.starts_with("v2:"), "sidecar ids must be versioned: {env}");
        assert_ne!(
            env,
            cache_id_for("legacy-id", Isolation::Sandbox, &[]),
            "the same legacy inputs under a different isolation must not alias"
        );
        assert_ne!(
            cache_id_for("legacy-id", Isolation::Env, &[("cc", "cc 13".into())]),
            cache_id_for("legacy-id", Isolation::Env, &[("cc", "cc 14".into())]),
            "a host compiler update must invalidate --use-cache"
        );
        assert_eq!(
            cache_id_for("legacy-id", Isolation::Env, &[("cc", "cc 13".into())]),
            cache_id_for("legacy-id", Isolation::Env, &[("cc", "cc 13".into())]),
            "the identity must be deterministic"
        );
    }

    #[test]
    fn host_toolchain_probe_is_empty_below_env_and_at_closure() {
        assert!(host_toolchain_identity(Isolation::None).is_empty());
        assert!(host_toolchain_identity(Isolation::Closure).is_empty());
        // The probe's contents depend on the host toolchain; only stability
        // is guaranteed here (absent commands are skipped, not errors).
        assert_eq!(
            host_toolchain_identity(Isolation::Env),
            host_toolchain_identity(Isolation::Env)
        );
    }
}

