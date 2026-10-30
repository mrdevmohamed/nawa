//! `build` command handlers.

use crate::*;

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
    println!(
        "nawa: installing {} into {}",
        pal.ver(&archive.display().to_string()),
        ctx.root.display()
    );
    // the library functions never lock; this command-level lock covers just
    // the install step (upgrade calls install_archive the same way)
    let _lock = lock::acquire(&ctx.root, lock::Mode::Exclusive)?;
    install::install_archive(&archive, &ctx.root, install_force, false, None, "explicit")?;
    Ok(())
}

/// `nawa install` takes archive paths *or* package names. Something that
/// ends in `.nawa.tar.zst` or contains a `/` is a file; a bare word is a name.
pub(crate) fn is_archive_arg(arg: &str) -> bool {
    arg.ends_with(".nawa.tar.zst") || arg.contains('/')
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
        println!("nawa: profile {arch}: {}", flags.join(" "));
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

    let profile_flags = env
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(" ");

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
    })
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
    println!("nawa: [{n}/{total}] building {id}");
    let opts = prepare_build(
        ctx,
        &it.rec,
        build::CheckMode::Off,
        flags.isolation.as_deref(),
        false,
        flags.allow_degraded,
        None,
        flags.use_cache,
    )?;
    Ok(build::build(&it.rec, &opts)
        .with_context(|| format!("[{n}/{total}] build of {id} failed"))?
        .archive)
}
