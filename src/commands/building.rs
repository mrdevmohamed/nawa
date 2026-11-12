//! `build` command handlers.

use anyhow::{bail, Context, Result};
use crate::ctx::Ctx;
use crate::color::Palette;
use std::path::PathBuf;
use crate::cli::PlanFlags;
use crate::color::Stream;
use crate::build;
use crate::buildenv;
use crate::db;
use crate::cli::default_jobs;
use crate::install;
use crate::lock;
use crate::plan;
use crate::profile;
use crate::recipe;
use crate::recovery;
use crate::transaction;


#[allow(clippy::too_many_arguments)]
pub(crate) fn cmd_build(
    ctx: &Ctx,
    name: &str,
    check: build::CheckMode,
    isolation: Option<&str>,
    rebuild_check: bool,
    allow_network: bool,
    allow_degraded: bool,
    timeout: Option<u64>,
    use_cache: bool,
    offline: bool,
    install_after: bool,
    install_force: bool,
) -> Result<()> {
    let rec = recipe::load_by_name(&ctx.recipes, name)?;
    let opts = prepare_build(
        ctx,
        &rec,
        check,
        isolation,
        allow_network,
        allow_degraded,
        timeout,
        use_cache,
        offline,
    )?;
    if rebuild_check {
        build::rebuild_check(&rec, &opts)?;
    }
    let archive = if install_after {
        // --rebuild-check already produced the archive; otherwise ALWAYS
        // build — an archive left in out/ by an earlier run carries the same
        // name-version-rel even when the recipe or patches changed since.
        let expected = opts.outdir.join(format!("{}.nawa.tar.zst", rec.id()));
        if rebuild_check && expected.exists() {
            expected
        } else {
            build::build(&rec, &opts)?.archive
        }
    } else {
        build::build(&rec, &opts)?;
        return Ok(());
    };

    // ---- --install --------------------------------------------------------
    let pal = Palette::auto(Stream::Stdout);
    crate::nawa_info!(
        "installing {} into {}",
        pal.ver(&archive.display().to_string()),
        ctx.root.display()
    );
    // the library functions never lock; this command-level lock covers just
    // the install step (upgrade calls install_archive the same way)
    let _lock = lock::acquire(&ctx.root, lock::Mode::Exclusive)?;
    // this command locks per install step, so `main` has not run recovery; do
    // it before `begin_operation` writes the live transaction's TxWAL
    recovery::recover_and_report(&ctx.root);
    let mut op = transaction::begin_operation(
        &ctx.root,
        transaction::OperationCommand::BuildInstall,
        &[name.to_string()],
        &[],
    )?;
    let db = db::Db::new(&ctx.root);
    let before = db.entry(name)?;
    let target = install::inspect_archive_state(&archive, crate::db::InstallReason::Explicit, before.as_ref())?;
    let actual_name = crate::archive::read_pkginfo(&archive)
        .ok()
        .and_then(|raw| crate::archive::parse_pkginfo(&raw).ok())
        .and_then(|info| info.get("pkgname").cloned())
        .unwrap_or_default();
    if actual_name != name {
        bail!("built archive package name {actual_name:?} does not match requested recipe {name:?}");
    }
    let action = match &before {
        None => transaction::history::StepAction::Install,
        Some(old) if old.version != target.version.clone().unwrap_or_default()
            || Some(old.rel) != target.rel => transaction::history::StepAction::Upgrade,
        Some(_) => transaction::history::StepAction::Reinstall,
    };
    let digest = target.artifact_sha256.clone();
    let mut recorder = op.recorder();
    let step_no = recorder.step_intent_state(name, action, before.as_ref(), Some(&target), &digest)?;
    let file_journal = install::FileJournalId::for_step(recorder.tx(), step_no);
    let result = install::install_archive_pinned(
        &archive, &ctx.root,
        install::InstallOpts { force: install_force, allow_unsigned: false, key: None, reason: crate::db::InstallReason::Explicit, pinned_sha: Some(&digest), file_journal },
    );
    install::record_install_outcome(
        &mut recorder,
        &ctx.root,
        step_no,
        name,
        action,
        before.as_ref(),
        &target,
        "local-build",
        &result,
    )?;
    op.finish(result.as_ref().err())?;
    result?;
    Ok(())
}

/// Everything `build` derives from the profile and flags: jobs, flags env,
/// isolation level, timeout — shared by `build`, `install <name>` and `update`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn prepare_build(
    ctx: &Ctx,
    rec: &recipe::Recipe,
    check: build::CheckMode,
    isolation: Option<&str>,
    allow_network: bool,
    allow_degraded: bool,
    timeout: Option<u64>,
    use_cache: bool,
    offline_flag: bool,
) -> Result<build::BuildOpts> {
    let arch = rec.arch();
    let profile = profile::Profile::load(&ctx.profiles, &arch)?;
    let env = profile.as_ref().map(|p| p.env()).unwrap_or_default();
    // precedence: --jobs > profile `jobs` > number of CPUs
    let jobs = ctx
        .jobs
        .or_else(|| profile.as_ref().and_then(|p| p.jobs))
        .unwrap_or_else(default_jobs);
    if !env.is_empty() {
        let flags: Vec<String> = env.iter().map(|(k, v)| format!("{k}={v}")).collect();
        crate::nawa_info!("profile {arch}: {}", flags.join(" "));
    }

    // isolation precedence: --isolation > profile [build] isolation > none
    let build_profile = std::fs::read_to_string(ctx.profiles.join(format!("{arch}.toml")))
        .ok()
        .and_then(|raw| profile::Profile::parse_build(&raw).ok())
        .unwrap_or_default();
    let isolation = match isolation {
        Some(s) => buildenv::Isolation::parse(s)?,
        None => match build_profile.isolation.as_deref() {
            Some(s) if !s.trim().is_empty() => buildenv::Isolation::parse(s)?,
            _ => buildenv::Isolation::None,
        },
    };
    let timeout_secs = timeout.or(build_profile.timeout).unwrap_or(0);

    // slimming: NAWA_SLIM (explicit) > profile [build] slim > on
    let slim = match std::env::var("NAWA_SLIM").ok().as_deref().map(str::trim) {
        Some("0" | "off" | "no" | "false") => false,
        Some("1" | "on" | "yes" | "true") => true,
        _ => build_profile.slim.unwrap_or(true),
    };

    let profile_flags = env
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(" ");

    // NAWA_OFFLINE is the environment equivalent of --offline (§15): the
    // dedicated flag wins when given; unknown values fail loudly instead
    // of silently degrading to online.
    let offline = offline_flag || offline_from_env()?;

    Ok(build::BuildOpts {
        jobs,
        builddir: ctx.builddir.clone(),
        outdir: ctx.builddir.join("out"),
        env,
        check,
        isolation,
        timeout_secs,
        allow_network,
        allow_degraded,
        root: ctx.root.clone(),
        recipes_root: ctx.recipes.clone(),
        allow_unsigned: false,
        profile_flags,
        use_cache,
        offline,
        slim,
    })
}

/// `NAWA_OFFLINE` → offline switch: `1|on|yes|true` enable, `0|off|no|false`
/// (and unset) keep it off, anything else is a hard error (`§15`).
pub(crate) fn offline_from_env() -> Result<bool> {
    let Some(v) = std::env::var_os("NAWA_OFFLINE") else {
        return Ok(false);
    };
    match v.to_string_lossy().trim().to_lowercase().as_str() {
        "" | "0" | "off" | "no" | "false" => Ok(false),
        "1" | "on" | "yes" | "true" => Ok(true),
        other => bail!(
            "unsupported NAWA_OFFLINE value {other:?} \
             (expected 1|on|yes|true|0|off|no|false)"
        ),
    }
}

/// Build one planned package from its recipe and return the archive.
pub(crate) fn build_one(
    ctx: &Ctx,
    it: &plan::Item,
    flags: &PlanFlags,
    n: usize,
    total: usize,
) -> Result<PathBuf> {
    let id = it.rec.id();
    crate::nawa_info!("[{n}/{total}] building {id}");
    let opts = prepare_build(
        ctx,
        &it.rec,
        build::CheckMode::Off,
        flags.isolation.as_deref(),
        false,
        flags.allow_degraded,
        None,
        flags.use_cache,
        flags.offline,
    )?;
    Ok(build::build(&it.rec, &opts)
        .with_context(|| format!("[{n}/{total}] build of {id} failed"))?
        .archive)
}

#[cfg(test)]
mod tests {
    use super::offline_from_env;

    #[test]
    fn nawa_offline_env_values() {
        // process-global env var: no other test touches NAWA_OFFLINE, so a
        // set check pair is safe
        std::env::set_var("NAWA_OFFLINE", "1");
        assert!(offline_from_env().unwrap());
        std::env::set_var("NAWA_OFFLINE", "no");
        assert!(!offline_from_env().unwrap());
        std::env::set_var("NAWA_OFFLINE", "  ");
        assert!(!offline_from_env().unwrap(), "blank means off");
        std::env::set_var("NAWA_OFFLINE", "perhaps");
        let e = offline_from_env().unwrap_err().to_string();
        assert!(e.contains("NAWA_OFFLINE"), "{e}");
        std::env::remove_var("NAWA_OFFLINE");
        assert!(!offline_from_env().unwrap(), "unset means off");
    }
}
