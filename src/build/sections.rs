//! Build sections.

use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Command, Stdio};

use super::{BuildOpts, CheckMode, FAIL_TAIL_LINES};
use super::sandbox::SandboxCfg;
use crate::recipe::{ExpandCtx, Recipe};

/// Walk two archives in lockstep and list members whose content hashes
/// differ (name, hash A, hash B).
pub(crate) fn diff_archives(a: &Path, b: &Path) -> Result<Vec<String>> {
    use sha2::Digest;
    let index = |p: &Path| -> Result<BTreeMap<String, String>> {
        let f = std::fs::File::open(p)?;
        let dec = zstd::Decoder::new(f)?;
        let mut ar = tar::Archive::new(dec);
        let mut out = BTreeMap::new();
        for entry in ar.entries()? {
            let mut entry = entry?;
            let name = entry.path()?.to_string_lossy().into_owned();
            let mut buf = Vec::new();
            std::io::Read::read_to_end(&mut entry, &mut buf)?;
            out.insert(name, hex::encode(sha2::Sha256::digest(&buf)));
        }
        Ok(out)
    };
    let (ia, ib) = (index(a)?, index(b)?);
    let mut diffs = Vec::new();
    for name in ia.keys().chain(ib.keys()).collect::<std::collections::BTreeSet<_>>() {
        let ha = ia.get(name).map(|s| s.as_str()).unwrap_or("<absent>");
        let hb = ib.get(name).map(|s| s.as_str()).unwrap_or("<absent>");
        if ha != hb {
            diffs.push(format!("{name}: {ha} != {hb}"));
        }
    }
    Ok(diffs)
}


/// The soname → owner index for `sandbox`-level lint: the installed db.
pub(crate) fn installed_sonames(root: &Path) -> Result<BTreeMap<String, String>> {
    let db = crate::db::Db::new(root);
    let mut out = BTreeMap::new();
    for entry in db.installed()? {
        for f in db.files(&entry.name)? {
            let rel = f.trim_start_matches('/');
            if let Some(soname) = crate::closure::soname_of(rel) {
                out.entry(soname).or_insert_with(|| entry.name.clone());
            }
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Section execution
// ---------------------------------------------------------------------------


/// Run one build section under `bash -e` — directly at `none`/`env`, inside
/// the sandbox at `sandbox`/`closure` — streaming its combined stdout+stderr
/// to the console as it is produced and to `<work>/build-<label>.log`. On
/// failure the last lines are quoted.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_section(
    rec: &Recipe,
    label: &str,
    script: &str,
    ctx: &ExpandCtx,
    srcdir: &Path,
    work: &Path,
    envs: &[(String, String)],
    env_prefix: &str,
    sandbox_cfg: Option<&SandboxCfg>,
    opts: &BuildOpts,
) -> Result<()> {
    let expanded = rec.expand(script, ctx);
    crate::nawa_info!("[{}] running {} (isolation: {})", rec.id(), label, opts.isolation.as_str());

    let log_path = work.join(format!("build-{label}.log"));
    let mut log = std::fs::File::create(&log_path)
        .with_context(|| format!("create {}", log_path.display()))?;

    // `exec 2>&1` merges stderr into the single stdout pipe, so the output
    // keeps its real ordering and one reader is enough.
    let wrapped = format!("exec 2>&1\n{env_prefix}{expanded}");

    let mut child = if let Some(cfg) = sandbox_cfg {
        let mut spec = cfg.base.clone();
        spec.script = wrapped;
        std::fs::write(&cfg.spec_path, toml::to_string_pretty(&spec)?)
            .with_context(|| format!("write {}", cfg.spec_path.display()))?;
        let exe = std::env::current_exe().context("locate the nawa binary")?;
        let (child, sock) = crate::sandbox::spawn_init(&exe, &cfg.spec_path)
            .context("spawn nawa sandbox-init")?;
        // install the ranged uid map when we are root, then release the child
        crate::sandbox::parent_sync(&child, sock);
        child
    } else {
        Command::new("bash")
            .args(["-e", "-c", &wrapped])
            .current_dir(srcdir)
            .envs(envs.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .env("NAWA_JOBS", ctx.jobs.to_string())
            .env("NAWA_PKGDIR", &ctx.pkgdir)
            .env("NAWA_SRCDIR", &ctx.srcdir)
            // §4.3: the new-schema names are injected exactly like
            // NAWA_SRCDIR; a legacy recipe's ctx has no srcroot, so the
            // name never enters a legacy build
            .envs(ctx.srcroot.iter().map(|r| ("NAWA_SRCROOT", r.to_string_lossy().into_owned())))
            // §5.3: same for the assets snapshot
            .envs(
                ctx.assetsdir
                    .iter()
                    .map(|a| ("NAWA_ASSETSDIR", a.to_string_lossy().into_owned())),
            )
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .spawn()
            .context("failed to spawn bash")?
    };

    let mut reader = BufReader::new(child.stdout.take().context("no stdout pipe")?);
    let mut tail: std::collections::VecDeque<String> =
        std::collections::VecDeque::with_capacity(FAIL_TAIL_LINES + 1);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        // read_until: tolerant of non-UTF-8 bytes and of very long lines
        if reader.read_until(b'\n', &mut buf)? == 0 {
            break;
        }
        let _ = log.write_all(&buf);
        {
            let mut out = std::io::stdout().lock();
            let _ = out.write_all(&buf);
            let _ = out.flush();
        }
        if tail.len() == FAIL_TAIL_LINES {
            tail.pop_front();
        }
        tail.push_back(String::from_utf8_lossy(&buf).trim_end().to_string());
    }
    let status = child.wait().context("waiting for the build section")?;

    if !status.success() {
        bail!(
            "{} failed with status {} — log: {}\n--- last output ---\n{}",
            label,
            status,
            log_path.display(),
            tail.into_iter().collect::<Vec<_>>().join("\n")
        );
    }
    Ok(())
}

/// Run the recipe's build sections in order (`prepare`, `configure`,
/// `compile`, `check`, `install`); `check` follows `--check` / `--check-nonfatal`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_sections(
    rec: &Recipe,
    opts: &BuildOpts,
    ctx: &ExpandCtx,
    srcdir: &Path,
    work: &Path,
    env_vars: &[(String, String)],
    env_prefix: &str,
    sandbox_cfg: Option<&SandboxCfg>,
) -> Result<()> {
    for (label, script) in [
        ("prepare", &rec.build.prepare),
        ("configure", &rec.build.configure),
        ("compile", &rec.build.compile),
        ("check", &rec.build.check),
        ("install", &rec.build.install),
    ] {
        if script.trim().is_empty() {
            continue;
        }
        if label == "check" {
            match opts.check {
                CheckMode::Off => continue,
                CheckMode::Fatal => {}
                CheckMode::Warn => {
                    if let Err(e) = run_section(
                        rec, label, script, &ctx, &srcdir, &work, &env_vars, env_prefix,
                        sandbox_cfg, opts,
                    ) {
                        crate::nawa_warn!("check failed for {} (continuing): {e:#}", rec.id());
                    }
                    continue;
                }
            }
        }
        run_section(
            rec, label, script, &ctx, &srcdir, &work, &env_vars, env_prefix,
            sandbox_cfg, opts,
        )?;
    }
    Ok(())
}

