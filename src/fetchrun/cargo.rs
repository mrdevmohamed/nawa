//! Cargo backend (§8).

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::os::unix::fs::PermissionsExt;

use super::common::{
    ecosystem_key, eco_cache_dir, entry_complete, copy_into_view, publish_entry,
    cleanup_failed_populate, spawn_fetch_child, fetch_timeout_secs, fetch_path,
    stream_and_wait,
};
use super::FetchRun;
use crate::recipe::FetchSpec;

/// The §8 precondition + detection result. Everything is checked around the
/// *downloaded source*, after staging.
pub(crate) struct CargoPlan {
    /// Host path of the fetch directory (the crate/workspace root).
    dir: PathBuf,
    /// The tree replaces the crates-io registry with an in-tree `directory`
    /// source that exists: skip the fetch, build as today (§8).
    vendored: bool,
    /// The bytes of the mandatory `Cargo.lock` — a key input (§11) and the
    /// source of the git-dependency warning (§8).
    lock: Vec<u8>,
    /// `source = "git+…"` values of the lock: warned about (§8/§10.2).
    git_deps: Vec<String>,
}


pub(crate) fn plan_cargo(r: &FetchRun<'_>, spec: &FetchSpec) -> Result<CargoPlan> {
    let dir = if spec.dir == "." || spec.dir.is_empty() {
        r.src.to_path_buf()
    } else {
        r.src.join(&spec.dir)
    };
    if !dir.is_dir() {
        bail!(
            "recipe {}: the [fetch.cargo] dir `{}` is {}, not a directory in the workspace",
            r.rec.package.name,
            spec.dir,
            dir.display()
        );
    }
    // §8: Cargo.lock is mandatory (checksums for registry crates, commit
    // pins for git ones); the manifest feeds --manifest-path itself.
    for name in ["Cargo.toml", "Cargo.lock"] {
        let p = dir.join(name);
        if !p.is_file() {
            bail!(
                "recipe {}: {name} is missing under {} — a `[fetch.cargo]` recipe \
                 requires it (fetch-phase.md §8)",
                r.rec.package.name,
                dir.display()
            );
        }
    }
    // §8 source-tree config rules: any [source.*] other than the
    // crates-io → in-tree directory replacement, and any [registries],
    // [http], [net] setting, is refused in v1.
    let vendored = scan_cargo_configs(r, &dir)?;
    let lock = std::fs::read(dir.join("Cargo.lock"))
        .with_context(|| format!("read {}", dir.join("Cargo.lock").display()))?;
    let text = std::str::from_utf8(&lock)
        .map_err(anyhow::Error::from)
        .with_context(|| format!("recipe {}: Cargo.lock is not UTF-8", r.rec.package.name))?;
    let git_deps = text
        .lines()
        .filter_map(|l| {
            let t = l.trim();
            t.strip_prefix("source = \"git+").map(|s| s.trim_end_matches('"').to_string())
        })
        .collect();
    Ok(CargoPlan { dir, vendored, lock, git_deps })
}


/// The §8 config scan: from `dir` up to the workspace root, every
/// `.cargo/config.toml` / `.cargo/config` that Cargo itself would load gets
/// the v1 rules. Returns whether ANY of them makes the tree vendored.
pub(crate) fn scan_cargo_configs(r: &FetchRun, dir: &Path) -> Result<bool> {
    let mut vendored = false;
    let mut scan = dir.to_path_buf();
    loop {
        let in_source_tree = scan.starts_with(r.src);
        for name in [".cargo/config.toml", ".cargo/config"] {
            let cfg = scan.join(name);
            if !cfg.exists() {
                continue;
            }
            if !in_source_tree {
                bail!(
                    "recipe {}: ambient Cargo config {} is outside the downloaded source tree and would be discovered by Cargo from the build working directory; remove it before building (fetch-phase.md §8)",
                    r.rec.package.name,
                    cfg.display()
                );
            }
            let text = std::fs::read_to_string(&cfg)
                .with_context(|| format!("read Cargo config {}", cfg.display()))?;
            if cargo_config_is_vendored(r, &cfg, &text)? {
                vendored = true;
            }
        }
        match scan.parent() {
            Some(parent) => scan = parent.to_path_buf(),
            None => break,
        }
    }
    Ok(vendored)
}


/// The v1 rules for ONE config file (§8):
///
/// * `[source.crates-io]` may only `replace-with` a `[source.X]` chain that
///   ends in a `directory` source **inside the downloaded tree** → vendored.
/// * any other `[source.*]` shape — a registry redirect, a directory that
///   does not exist or escapes the tree, unused sources — is an error.
/// * `[registries]`, `[http]`, `[net]` are refused.
pub(crate) fn cargo_config_is_vendored(
    r: &FetchRun,
    cfg: &Path,
    text: &str,
) -> Result<bool> {
    let name = &r.rec.package.name;
    // relative directory values resolve against the parent of the `.cargo`
    // directory that holds the config (probed: independent of the cwd)
    let base = match cfg.parent().and_then(Path::parent) {
        Some(b) => b.to_path_buf(),
        None => return Ok(false),
    };
    let cfg = cfg.display();
    let value: toml::Value = toml::from_str(text).map_err(|e| {
        anyhow::Error::msg(format!(
            "recipe {name}: {cfg} is not valid TOML ({e}); the §8 config rules \
             cannot evaluate what cargo would load"
        ))
    })?;

    for table in ["registries", "http", "net"] {
        if value.get(table).is_some() {
            bail!(
                "recipe {name}: {cfg} sets [{table}] — refused in v1 because it changes \
                 how Cargo reaches the registry (fetch-phase.md §8). Remove it from the \
                 downloaded tree."
            );
        }
    }

    let Some(sources) = value.get("source").and_then(|v| v.as_table()) else {
        return Ok(false);
    };

    let mut referenced: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    referenced.insert("crates-io".into());

    if let Some(crates_io) = sources.get("crates-io").and_then(|v| v.as_table()) {
        if let Some(evil) = crates_io.keys().find(|k| *k != "replace-with") {
            bail!(
                "recipe {name}: {cfg} redirects [source.crates-io] through `{evil}` — a \
                 registry redirect is refused in v1 (fetch-phase.md §8); the only allowed \
                 replacement is an in-tree `directory` source."
            );
        }
        let Some(toml::Value::String(target)) = crates_io.get("replace-with") else {
            bail!(
                "recipe {name}: {cfg}: [source.crates-io] must carry only the \
                 replace-with chain to an in-tree directory source (fetch-phase.md §8)"
            );
        };
        let mut cur = target.clone();
        loop {
            // every hop must appear exactly once in the chain (no cycles)
            if !referenced.insert(cur.clone()) {
                bail!(
                    "recipe {name}: {cfg}: the source replacement chain revisits `{cur}` \
                     (fetch-phase.md §8)"
                );
            }
            match sources.get(&cur) {
                Some(toml::Value::Table(hop)) => {
                    if hop.get("registry").is_some() {
                        bail!(
                            "recipe {name}: {cfg} redirects through the registry source \
                             `{cur}` — refused in v1 (fetch-phase.md §8)"
                        );
                    }
                    if let Some(toml::Value::String(d)) = hop.get("directory") {
                        if hop.keys().count() > 1 {
                            bail!(
                                "recipe {name}: {cfg}: the source `{cur}` mixes `directory` \
                                 with other keys (fetch-phase.md §8)"
                            );
                        }
                        // cargo resolves it against `base` (§8 vendored rule:
                        // the directory must exist *in the tree*)
                        let resolved = base.join(d);
                        let inside = if resolved.strip_prefix(r.src).is_ok() {
                            resolved.is_dir()
                        } else {
                            false
                        };
                        if !inside {
                            bail!(
                                "recipe {name}: {cfg} replaces crates-io with the directory \
                                 source `{cur}` pointing at {} — it must be an existing \
                                 directory inside the downloaded tree (fetch-phase.md §8)",
                                resolved.display()
                            );
                        }
                        return Ok(true);
                    }
                    if let Some(toml::Value::String(next)) = hop.get("replace-with") {
                        cur = next.clone();
                        continue;
                    }
                    bail!(
                        "recipe {name}: {cfg}: the source `{cur}` is neither a directory \
                         nor a replacement chain (fetch-phase.md §8)"
                    );
                }
                _ => bail!(
                    "recipe {name}: {cfg} replaces crates-io with `{cur}`, which is not \
                     defined as a source table in this config (fetch-phase.md §8)"
                ),
            }
        }
    }

    // any [source.X] outside the (absent) crates-io chain is an error
    for id in sources.keys() {
        if id == "crates-io" {
            continue;
        }
        if !referenced.contains(id) {
            bail!(
                "recipe {name}: {cfg} defines the source `{id}` — any [source.*] other \
                 than the crates-io → in-tree directory replacement is refused in v1 \
                 (fetch-phase.md §8)"
            );
        }
    }
    Ok(false)
}


/// The `[fetch.cargo]` backend (§8): plan, populate-or-hit, expose the
/// Cargo home. Build-time additions: `CARGO_HOME` (the per-build view)
/// and `CARGO_NET_OFFLINE=true` — recipes keep their own commands, e.g.
/// `cargo build --release --locked --offline`.
pub(crate) fn run_cargo(r: &FetchRun<'_>, spec: &FetchSpec) -> Result<Vec<(String, String)>> {
    let plan = plan_cargo(r, spec)?;
    // The per-build view (§6): <work>/fetch/cargo. It exists for every mode,
    // vendored ones included — the sandbox binder requires it either way.
    let view_host = r.work.join("fetch").join("cargo");
    std::fs::create_dir_all(&view_host)
        .with_context(|| format!("create the Cargo fetch view {}", view_host.display()))?;

    if !plan.vendored {
        // §11: the key covers the Cargo.lock bytes and the Cargo version
        // string (registry directory names embed a hash that changes
        // between Cargo releases). The version query runs one cheap
        // `cargo --version` in the same execution context as the fetch.
        let version = cargo_version_query(r, &plan)?;
        let key = ecosystem_key("cargo", &[&plan.lock, version.as_bytes()])?;
        ensure_cargo_entry(r, &plan, &key)?;
        let entry = eco_cache_dir(r, "cargo").join(&key);
        copy_into_view(&entry, &view_host).with_context(|| {
            format!("populate the per-build Cargo fetch view {}", view_host.display())
        })?;
    }

    // §8 answers cache mutation with the per-build copy (§11: never
    // hard-linked): `.package-cache`, `.global-cache` and friends are
    // written into the view — disposable storage — while the published
    // entry stays pristine. Nothing else is needed at build time: the
    // vendored case reads its crates straight from the tree.
    let view = if r.sandboxed {
        format!("{}/fetch/cargo", r.build_inside.trim_end_matches('/'))
    } else {
        view_host.to_string_lossy().into_owned()
    };
    Ok(vec![
        ("CARGO_HOME".into(), view),
        ("CARGO_NET_OFFLINE".into(), "true".into()),
    ])
}


/// Populated-or-hit discipline for a Cargo key (§11), mirroring
/// [`ensure_go_entry`]: marker fast path, offline guard before anything is
/// created, hold-open flock, 0777 sibling temporary, publish, cleanup.
pub(crate) fn ensure_cargo_entry(r: &FetchRun, plan: &CargoPlan, key: &str) -> Result<()> {
    let cache = eco_cache_dir(r, "cargo");
    let entry = cache.join(key);
    if entry_complete(&entry) {
        return Ok(());
    }
    if crate::fetch::offline() {
        bail!(
            "offline: the Cargo cache entry is not populated: {} — run a build \
             once online, or use `nawa fetch` (fetch-phase.md §8, §12)",
            entry.display()
        );
    }
    std::fs::create_dir_all(&cache)
        .with_context(|| format!("create the Cargo fetch cache {}", cache.display()))?;
    let lock_path = cache.join(format!("{key}.lock"));
    let _guard = crate::fetch::lock_digest_entry(&lock_path)?;
    // runner-up recheck: the other build may have published while we waited
    if entry_complete(&entry) {
        return Ok(());
    }
    let temp = cache.join(format!(
        "{}.tmp-{}-{}",
        key,
        std::process::id(),
        crate::unique::next()
    ));
    let _ = std::fs::remove_dir_all(&temp);
    std::fs::create_dir_all(&temp)
        .with_context(|| format!("create {}", temp.display()))?;
    std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o777))?;

    let outcome = run_populate_cargo(r, plan, &temp)
        .and_then(|()| publish_entry(&temp, &entry));
    if let Err(err) = outcome {
        cleanup_failed_populate(&temp);
        return Err(err.context("populating the Cargo fetch cache (fetch-phase.md §8, §11)"));
    }
    Ok(())
}


/// The fixed command of the Cargo backend (§8): `--locked` must fail rather
/// than update the lock (Cargo's own contract), and no `--target` filter —
/// v1 fetches all targets of the lockfile.
const CARGO_POPULATE_SCRIPT: &str = "cargo fetch --locked --manifest-path \"$NAWA_CARGO_MANIFEST\"";


/// §8: git dependencies are pinned by commit but carry no registry
/// checksums, and `cargo fetch` invokes `git` itself. Warn at fetch time,
/// before the populate actually runs.
const CARGO_VERSION_SCRIPT: &str = "cargo --version > \"$NAWA_CARGO_VER_OUT\"";


/// Create an empty Cargo working directory under the system temp root and a
/// fresh CARGO_HOME inside the output bind. Cargo searches cwd ancestors for
/// config files, so refuse any ambient ancestor config rather than allowing
/// an unscanned host setting to redirect the fetch.
pub(crate) fn isolated_cargo_cwd() -> Result<PathBuf> {
    let cwd = std::env::temp_dir().join(format!(
        "nawa-cargo-cwd-{}-{}",
        std::process::id(),
        crate::unique::next()
    ));
    std::fs::create_dir(&cwd).with_context(|| format!("create isolated Cargo cwd {}", cwd.display()))?;
    std::fs::set_permissions(&cwd, std::fs::Permissions::from_mode(0o777))?;
    for parent in cwd.ancestors() {
        for name in [".cargo/config.toml", ".cargo/config"] {
            let candidate = parent.join(name);
            if candidate.is_file() {
                let _ = std::fs::remove_dir_all(&cwd);
                bail!(
                    "Cargo fetch cannot run hermetically: ambient config {} would be discovered from its working directory; remove it or use a clean system temp root (fetch-phase.md §8)",
                    candidate.display()
                );
            }
        }
    }
    Ok(cwd)
}


/// A fresh Cargo home is the output directory itself: the published entry is
/// later mounted/copied as CARGO_HOME, so its git/ and registry/ directories
/// must live at the entry root rather than one level below it.
pub(crate) fn isolated_cargo_paths(out_host: &Path) -> Result<(PathBuf, PathBuf)> {
    std::fs::create_dir_all(out_host).with_context(|| format!("create {}", out_host.display()))?;
    std::fs::set_permissions(out_host, std::fs::Permissions::from_mode(0o777))?;
    let cwd = isolated_cargo_cwd()?;
    Ok((out_host.to_path_buf(), cwd))
}


pub(crate) fn run_populate_cargo(r: &FetchRun, plan: &CargoPlan, temp: &Path) -> Result<()> {
    if !plan.git_deps.is_empty() {
        crate::nawa_warn!(
            "fetch: Cargo.lock uses git dependencies without registry checksums ({}); `cargo fetch` needs git in the fetch root, and such dependencies are verified only by their pinned commit (fetch-phase.md §8, §10.2)",
            plan.git_deps.join(", ")
        );
    }
    let timeout_secs = fetch_timeout_secs()?;
    let label = "fetch-cargo";
    let log_path = r.work.join(format!("{label}.log"));
    let script = format!("exec 2>&1\n{CARGO_POPULATE_SCRIPT}");
    let manifest_rel = plan.dir.strip_prefix(r.src).unwrap_or(Path::new("")).join("Cargo.toml");
    let cwd_host = isolated_cargo_cwd()?;
    let result = (|| -> Result<()> {
        let mut child = if r.sandboxed {
            let base = r.build_inside.trim_end_matches('/');
            let env = [
                cargo_fetch_base_env(&format!("{base}/home")),
                vec![
                    // fetch-out is the published cache entry and is exposed
                    // unchanged as CARGO_HOME during the offline build.
                    ("CARGO_HOME".into(), format!("{base}/fetch-out")),
                    ("NAWA_CARGO_MANIFEST".into(), format!("{}/src/{}", base, manifest_rel.display())),
                ],
            ].concat();
            spawn_fetch_child(r, &script, env, &cwd_host, Some(temp), timeout_secs)?
        } else {
            let home_host = r.work.join("sb").join("home");
            std::fs::create_dir_all(&home_host).with_context(|| format!("create {}", home_host.display()))?;
            let env = [
                cargo_fetch_base_env(&home_host.to_string_lossy()),
                vec![
                    ("CARGO_HOME".into(), temp.to_string_lossy().into_owned()),
                    ("NAWA_CARGO_MANIFEST".into(), plan.dir.join("Cargo.toml").to_string_lossy().into_owned()),
                ],
            ].concat();
            spawn_fetch_child(r, &script, env, &cwd_host, None, timeout_secs)?
        };
        let status = stream_and_wait(&mut child, Some(&log_path), timeout_secs, label, !r.sandboxed)?;
        if !status.success() {
            bail!("fetch run ({label}) failed with status {status} — log: {}", log_path.display());
        }
        Ok(())
    })();
    let _ = std::fs::remove_dir_all(&cwd_host);
    result
}


/// One cheap `cargo --version` in the fetch context (§11: the version
/// string enters the ecosystem key). The script writes the version into
/// the fetch-out bind (sandbox flavor) or the scratch dir (plain flavor);
/// the caller reads it back from the host side.
pub(crate) fn cargo_version_query(r: &FetchRun, _plan: &CargoPlan) -> Result<String> {
    let timeout_secs = fetch_timeout_secs()?;
    let label = "fetch-cargo-version";
    let out_dir = r.work.join("fetch").join("verq");
    std::fs::create_dir_all(&out_dir).with_context(|| format!("create {}", out_dir.display()))?;
    std::fs::set_permissions(&out_dir, std::fs::Permissions::from_mode(0o777))?;
    let out_host = out_dir.join("version");
    let script = format!("exec 2>&1\n{CARGO_VERSION_SCRIPT}");
    let (cargo_home_host, cwd_host) = isolated_cargo_paths(&out_dir)?;
    let result = (|| -> Result<String> {
        let mut child = if r.sandboxed {
            let base = r.build_inside.trim_end_matches('/');
            let env = [
                cargo_fetch_base_env(&format!("{base}/home")),
                vec![
                    ("CARGO_HOME".into(), format!("{base}/fetch-out")),
                    ("NAWA_CARGO_VER_OUT".into(), format!("{base}/fetch-out/version")),
                ],
            ].concat();
            spawn_fetch_child(r, &script, env, &cwd_host, Some(&out_dir), timeout_secs)?
        } else {
            let home_host = r.work.join("sb").join("home");
            std::fs::create_dir_all(&home_host).with_context(|| format!("create {}", home_host.display()))?;
            let env = [
                cargo_fetch_base_env(&home_host.to_string_lossy()),
                vec![
                    ("CARGO_HOME".into(), cargo_home_host.to_string_lossy().into_owned()),
                    ("NAWA_CARGO_VER_OUT".into(), out_host.to_string_lossy().into_owned()),
                ],
            ].concat();
            spawn_fetch_child(r, &script, env, &cwd_host, None, timeout_secs)?
        };
        let status = stream_and_wait(&mut child, None, timeout_secs, label, !r.sandboxed)
            .with_context(|| "the cargo --version probe failed (fetch-phase.md §11)")?;
        if !status.success() { bail!("fetch run ({label}) failed with status {status}"); }
        let version = std::fs::read_to_string(&out_host)
            .with_context(|| format!("the cargo --version probe wrote no version file at {}", out_host.display()))?;
        let version = version.trim().to_string();
        if version.is_empty() { bail!("the cargo --version probe produced an empty version string (fetch-phase.md §11)"); }
        Ok(version)
    })();
    let _ = std::fs::remove_dir_all(&cwd_host);
    result
}


/// The §8 fetch base allowlist ("from scratch, same base as §7"): the Go
/// names are absent by construction. The backend adds its own keys.
pub(crate) fn cargo_fetch_base_env(home: &str) -> Vec<(String, String)> {
    vec![
        ("PATH".into(), fetch_path()),
        ("TMPDIR".into(), "/tmp".into()),
        ("LC_ALL".into(), "C".into()),
        ("TZ".into(), "UTC".into()),
        ("HOME".into(), home.into()),
    ]
}

