//! install / remove / update / autoremove / outdated handlers and the plan runner.

use crate::*;

pub(crate) fn cmd_install(
    ctx: &Ctx,
    targets: &[String],
    flags: &PlanFlags,
    no_deps: bool,
    key: Option<PathBuf>,
    mode: Option<InstallMode>,
    repo: Option<String>,
) -> Result<()> {
    let archives = targets.iter().filter(|t| is_archive_arg(t)).count();
    if archives == targets.len() {
        for t in targets {
            install_archive_file(ctx, Path::new(t), flags.force, flags.allow_unsigned, key.clone())?;
        }
        let changed: Vec<String> = targets
            .iter()
            .filter_map(|t| revdep::archive_name(Path::new(t)))
            .collect();
        warn_revdep(ctx, &changed);
        return Ok(());
    }
    if archives > 0 {
        bail!("do not mix package names and archive files in one `nawa install`");
    }
    if key.is_some() {
        bail!("--key only applies to archive files (package names come from the repository or are built locally)");
    }

    // where packages come from: flag > install_mode (env / nawa.toml / default)
    let mode = mode.unwrap_or(ctx.install_mode);
    let repo_url = repo.or_else(|| ctx.repo.clone());
    let sourcing = open_sourcing(ctx, mode, repo_url.as_deref(), flags)?;

    let known = recipe::load_all(&ctx.recipes)?;
    let installed = installed_map(ctx)?;

    // a name without a recipe can still be installed when the repository has
    // it (a binary-only package: no dependency resolution, no build)
    let mut recipe_targets: Vec<String> = Vec::new();
    let mut extras: Vec<repo::RepoEntry> = Vec::new();
    for t in targets {
        if known.contains_key(t) {
            recipe_targets.push(t.clone());
            continue;
        }
        match sourcing.entry(t) {
            Some(re) => match installed.get(t) {
                Some(cur)
                    if version::vercmp(
                        &format!("{}-{}", cur.version, cur.rel),
                        &format!("{}-{}", re.version, re.rel),
                    ) != std::cmp::Ordering::Less =>
                {
                    println!("nawa: {t} is already installed and up to date");
                }
                _ => extras.push(re.clone()),
            },
            // neither a recipe nor a repository package: the planner reports
            // it with a "did you mean" hint
            None => recipe_targets.push(t.clone()),
        }
    }

    let plan = plan::install_plan(&recipe_targets, &installed, &known, plan::Why::Explicit, !no_deps)?;
    for name in &plan.up_to_date {
        println!("nawa: {name} is already installed and up to date");
    }
    run_plan(ctx, &plan, flags, &sourcing, &extras)
}

/// Where each package of a plan comes from, decided per package.
pub(crate) enum Source {
    /// build it from its recipe
    Build,
    /// install this archive from the repository
    Repo(repo::RepoEntry),
}

/// The repository (when one is usable) plus the policy for using it.
pub(crate) struct Sourcing {
    /// `Auto` (repository when it has the package at least as new as the
    /// recipe, else build), `Repo` (strict: repository only) or `Build`
    pub(crate) mode: InstallMode,
    /// `install_mode = ask`: offer "repository or build?" on a terminal
    pub(crate) ask: bool,
    pub(crate) open: Option<upgrade::OpenRepo>,
}

impl Sourcing {
    /// Always build — what `update` and `--build` do.
    pub(crate) fn build_only() -> Self {
        Sourcing { mode: InstallMode::Build, ask: false, open: None }
    }

    /// The repository's entry for `name`, if a repository is open and
    /// usable on this machine's architecture.
    pub(crate) fn entry(&self, name: &str) -> Option<&repo::RepoEntry> {
        let open = self.open.as_ref()?;
        let re = open.index.find(name)?;
        if arch_fits(&re.arch) {
            Some(re)
        } else {
            None
        }
    }

    /// Decide the source of one recipe-backed package.
    pub(crate) fn pick(&self, rec: &recipe::Recipe) -> Result<Source> {
        if self.mode == InstallMode::Build {
            return Ok(Source::Build);
        }
        let name = &rec.package.name;
        let have = self.entry(name);
        let strict = self.mode == InstallMode::Repo;
        match have {
            None if strict => bail!(
                "`{name}` is not in the repository for this architecture \
                 (use --build to build it, or the default `auto` mode to fall back to a build)"
            ),
            None => Ok(Source::Build),
            Some(re) => {
                let theirs = format!("{}-{}", re.version, re.rel);
                let ours = format!("{}-{}", rec.package.version, rec.package.rel);
                let older = version::vercmp(&theirs, &ours) == std::cmp::Ordering::Less;
                if older && !strict {
                    // the recipe tree is newer than the repository: honour the recipe
                    Ok(Source::Build)
                } else {
                    if older {
                        eprintln!(
                            "nawa: note: repository has {name} {theirs}, the recipe is {ours} \
                             — installing the repository version"
                        );
                    }
                    Ok(Source::Repo(re.clone()))
                }
            }
        }
    }
}

/// Is an archive built for `arch` usable on this machine?
pub(crate) fn arch_fits(arch: &str) -> bool {
    let host = std::env::var("NAWA_ARCH").unwrap_or_else(|_| std::env::consts::ARCH.to_string());
    arch.is_empty() || matches!(arch, "any" | "noarch" | "all") || arch == host
}

/// Open the configured repository according to the install mode. A repository
/// that cannot be used (offline, empty, no trusted key yet, switched off) is
/// an error only in strict `repo` mode; otherwise it is one `note:` line and
/// packages are built from source.
pub(crate) fn open_sourcing(
    ctx: &Ctx,
    mode: InstallMode,
    repo_url: Option<&str>,
    flags: &PlanFlags,
) -> Result<Sourcing> {
    let ask = mode == InstallMode::Ask;
    let eff = if ask { InstallMode::Auto } else { mode };
    if eff == InstallMode::Build {
        return Ok(Sourcing::build_only());
    }
    let strict = eff == InstallMode::Repo;
    let url = match repo_url {
        Some(u) => u,
        None if strict => bail!(
            "no repository configured: pass --repo <dir-or-url>, set `repo` in nawa.toml \
             or /etc/nawa/nawa.toml, or export NAWA_REPO"
        ),
        None => {
            println!("nawa: note: no repository configured — building from source");
            return Ok(Sourcing::build_only());
        }
    };
    let repo_ref = fetch::RepoRef::parse(url)?;
    match upgrade::open_repo(&ctx.root, &repo_ref, flags.allow_unsigned, flags.force) {
        Ok(open) => Ok(Sourcing { mode: eff, ask, open: Some(open) }),
        Err(e) if strict => Err(e).with_context(|| format!("repository {url} is not usable")),
        Err(e) => {
            println!("nawa: note: repository {url} is not usable ({e:#}) — building from source");
            Ok(Sourcing::build_only())
        }
    }
}

/// The installed packages by name, read under a brief shared lock.
pub(crate) fn installed_map(ctx: &Ctx) -> Result<std::collections::HashMap<String, db::Entry>> {
    let _lock = lock::acquire(&ctx.root, lock::Mode::Shared)?;
    Ok(db::Db::new(&ctx.root)
        .installed()?
        .into_iter()
        .map(|e| (e.name.clone(), e))
        .collect())
}

/// Install one ready archive. The library functions never lock; hold the db
/// lock for this one install only, never through a build or a download.
pub(crate) fn install_one(
    ctx: &Ctx,
    archive: &Path,
    flags: &PlanFlags,
    reason: &str,
    what: &str,
    n: usize,
    total: usize,
) -> Result<()> {
    println!("nawa: [{n}/{total}] installing {what}");
    let _lock = lock::acquire(&ctx.root, lock::Mode::Exclusive)?;
    install::install_archive(archive, &ctx.root, flags.force, flags.allow_unsigned, None, reason)
        .with_context(|| format!("[{n}/{total}] install of {what} failed"))?;
    Ok(())
}

/// Show the plan, ask, then install every item in order — dependencies
/// first, one `[n/total]` line per step. Each item comes from the repository
/// or is built from its recipe (see [`Sourcing`]); `extras` are repository
/// packages without a recipe, installed last.
pub(crate) fn run_plan(
    ctx: &Ctx,
    plan: &plan::Plan,
    flags: &PlanFlags,
    sourcing: &Sourcing,
    extras: &[repo::RepoEntry],
) -> Result<()> {
    let total = plan.items.len() + extras.len();
    if total == 0 {
        println!("nawa: nothing to do");
        return Ok(());
    }

    // decide every source up front: a strict `repo` run must fail BEFORE the
    // first package is touched, not halfway through
    let mut sources: Vec<Source> = Vec::with_capacity(plan.items.len());
    for it in &plan.items {
        sources.push(sourcing.pick(&it.rec)?);
    }

    let pal = Palette::auto(Stream::Stdout);
    let count_repo = |sources: &[Source]| -> usize {
        sources.iter().filter(|s| matches!(s, Source::Repo(_))).count() + extras.len()
    };
    let print_plan = |sources: &[Source]| {
        let from_repo = count_repo(sources);
        println!(
            "nawa: {total} package(s) to install into {} ({from_repo} from the repository, {} to build):",
            ctx.root.display(),
            total - from_repo
        );
        for (i, it) in plan.items.iter().enumerate() {
            let replaces = match &it.replaces {
                Some(old) => format!("  replaces {old}"),
                None => String::new(),
            };
            let tag = match &sources[i] {
                Source::Repo(_) => "repo ",
                Source::Build => "build",
            };
            println!(
                "  {:>3}. {} {}  {} {}{}",
                i + 1,
                pal.name(&format!("{:<22}", it.rec.package.name)),
                pal.ver(&format!(
                    "{:<12}",
                    format!("{}-{}", it.rec.package.version, it.rec.package.rel)
                )),
                pal.dim(&format!("[{tag}]")),
                pal.dim(it.why.label()),
                pal.dim(&replaces),
            );
        }
        for (j, re) in extras.iter().enumerate() {
            println!(
                "  {:>3}. {} {}  {} {}",
                plan.items.len() + j + 1,
                pal.name(&format!("{:<22}", re.name)),
                pal.ver(&format!("{:<12}", format!("{}-{}", re.version, re.rel))),
                pal.dim("[repo ]"),
                pal.dim("explicit  (no recipe: dependencies not resolved)"),
            );
        }
    };
    print_plan(sources.as_slice());
    if flags.dry_run {
        println!("nawa: dry run — nothing was built or installed");
        return Ok(());
    }

    // `install_mode = ask`: on a terminal, let the user choose the source
    // for everything the repository could supply
    if sourcing.ask && count_repo(sources.as_slice()) > 0 {
        let c = confirm::choose(
            "nawa: install the repository-available packages from the repository (r) or build them from source (b)?",
            'r',
            &['r', 'b'],
        )?;
        if c == 'b' {
            if let Some(re) = extras.first() {
                bail!(
                    "`{}` has no recipe, so it cannot be built — it exists only in the repository",
                    re.name
                );
            }
            for s in sources.iter_mut() {
                *s = Source::Build;
            }
            println!("nawa: building all {total} package(s) from source");
        }
    }
    confirm::confirm(&format!("nawa: install {total} package(s)?"), flags.yes)?;

    let started = std::time::Instant::now();
    for (i, it) in plan.items.iter().enumerate() {
        let n = i + 1;
        let id = it.rec.id();
        let step = || -> Result<()> {
            let archive: PathBuf = match &sources[i] {
                Source::Build => build_one(ctx, it, flags, n, total)?,
                Source::Repo(re) => {
                    println!("nawa: [{n}/{total}] fetching {id} from the repository");
                    let open = sourcing
                        .open
                        .as_ref()
                        .context("internal: repository source without an open repository")?;
                    match upgrade::fetch_entry(&ctx.root, open, re, flags.allow_unsigned) {
                        Ok(a) => a,
                        // strict repo mode never builds behind the user's back
                        Err(e) if sourcing.mode == InstallMode::Repo => {
                            return Err(e)
                                .with_context(|| format!("[{n}/{total}] repository fetch of {id} failed"));
                        }
                        // auto: the recipe pins its sources, so a build is a safe fallback
                        Err(e) => {
                            eprintln!(
                                "nawa: warning: cannot use the repository copy of {id} ({e:#}) — building from source"
                            );
                            build_one(ctx, it, flags, n, total)?
                        }
                    }
                }
            };
            install_one(ctx, &archive, flags, it.why.db_reason(), &id, n, total)
        };
        if let Err(e) = step() {
            if i > 0 {
                eprintln!(
                    "nawa: {i} of {total} package(s) were installed before this failure — \
                     re-run the same command to continue (installed packages are skipped)"
                );
            }
            return Err(e);
        }
    }
    for (j, re) in extras.iter().enumerate() {
        let n = plan.items.len() + j + 1;
        let what = format!("{}-{}-{}", re.name, re.version, re.rel);
        let step = || -> Result<()> {
            println!("nawa: [{n}/{total}] fetching {what} from the repository");
            let open = sourcing
                .open
                .as_ref()
                .context("internal: repository package without an open repository")?;
            let archive = upgrade::fetch_entry(&ctx.root, open, re, flags.allow_unsigned)
                .with_context(|| format!("[{n}/{total}] repository fetch of {what} failed"))?;
            install_one(ctx, &archive, flags, "explicit", &what, n, total)
        };
        if let Err(e) = step() {
            if n > 1 {
                eprintln!(
                    "nawa: {} of {total} package(s) were installed before this failure — \
                     re-run the same command to continue (installed packages are skipped)",
                    n - 1
                );
            }
            return Err(e);
        }
    }
    println!(
        "nawa: done — {total} package(s) installed in {}",
        format_duration(started.elapsed().as_secs())
    );
    let mut changed: Vec<String> = plan.items.iter().map(|it| it.rec.package.name.clone()).collect();
    changed.extend(extras.iter().map(|re| re.name.clone()));
    warn_revdep(ctx, &changed);
    Ok(())
}

pub(crate) fn cmd_outdated(ctx: &Ctx) -> Result<()> {
    let known = recipe::load_all(&ctx.recipes)?;
    let installed = db::Db::new(&ctx.root).installed()?;
    let list = plan::outdated(&installed, &known);
    if list.is_empty() {
        println!("nawa: everything is up to date ({} installed)", installed.len());
        return Ok(());
    }
    let pal = Palette::auto(Stream::Stdout);
    for (e, r) in &list {
        println!(
            "{} {} -> {}",
            pal.name(&format!("{:<24}", e.name)),
            pal.dim(&format!("{:<14}", format!("{}-{}", e.version, e.rel))),
            pal.ver(&format!("{}-{}", r.package.version, r.package.rel)),
        );
    }
    println!("nawa: {} package(s) can be updated — run `nawa update`", list.len());
    Ok(())
}

pub(crate) fn cmd_update(ctx: &Ctx, names: &[String], flags: &PlanFlags) -> Result<()> {
    let known = recipe::load_all(&ctx.recipes)?;
    let installed = installed_map(ctx)?;
    for n in names {
        if !installed.contains_key(n) {
            bail!("`{n}` is not installed (use `nawa install {n}`)");
        }
    }
    let list: Vec<db::Entry> = installed.values().cloned().collect();
    let targets: Vec<String> = plan::outdated(&list, &known)
        .into_iter()
        .map(|(e, _)| e.name.clone())
        .filter(|n| names.is_empty() || names.contains(n))
        .collect();
    if targets.is_empty() {
        println!("nawa: everything is up to date");
        return Ok(());
    }
    let plan = plan::install_plan(&targets, &installed, &known, plan::Why::Update, true)?;
    // `update` follows the recipes (rebuilds); binary upgrades are `nawa upgrade`
    let result = run_plan(ctx, &plan, flags, &Sourcing::build_only(), &[]);
    // also after a partial run: whatever was installed is worth checking
    let changed: Vec<String> = plan.items.iter().map(|it| it.rec.package.name.clone()).collect();
    warn_revdep(ctx, &changed);
    result
}

pub(crate) fn cmd_autoremove(ctx: &Ctx, dry_run: bool, yes: bool, build_deps: bool) -> Result<()> {
    let known = recipe::load_all(&ctx.recipes)?;
    if known.is_empty() && !build_deps {
        bail!(
            "no recipes under {} — cannot tell which dependencies are build tools to keep \
             (point --recipes at the tree, or pass --build-deps to ignore build tools)",
            ctx.recipes.display()
        );
    }
    let installed = db::Db::new(&ctx.root).installed()?;
    let orphans = plan::orphans(&installed, &known, build_deps);
    if orphans.is_empty() {
        println!("nawa: nothing to remove");
        return Ok(());
    }
    let order = plan::removal_order(&orphans, &installed);
    let pal = Palette::auto(Stream::Stdout);
    println!("nawa: {} unneeded dependency package(s):", order.len());
    for n in &order {
        println!("  {}", pal.name(n));
    }
    if dry_run {
        println!("nawa: dry run — nothing was removed");
        return Ok(());
    }
    confirm::confirm(&format!("nawa: remove {} package(s)?", order.len()), yes)?;
    for (i, n) in order.iter().enumerate() {
        println!("nawa: [{}/{}] removing {n}", i + 1, order.len());
        install::remove_with(n, &ctx.root, false)?;
    }
    println!("nawa: done — {} package(s) removed", order.len());
    Ok(())
}

pub(crate) fn install_archive_file(
    ctx: &Ctx,
    file: &Path,
    force: bool,
    allow_unsigned: bool,
    key: Option<PathBuf>,
) -> Result<()> {
    if !file.exists() {
        // a near-miss hint saves a trip to `ls build/out`
        let dir = match file.parent() {
            Some(d) if !d.as_os_str().is_empty() => d,
            _ => Path::new("."),
        };
        let wanted = file
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| file.display().to_string());
        let names: Vec<String> = std::fs::read_dir(dir)
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .map(|e| e.file_name().to_string_lossy().to_string())
                    .collect()
            })
            .unwrap_or_default();
        bail!(
            "archive not found: {}{}",
            file.display(),
            suggest::hint(&wanted, &names)
        );
    }
    install::install_archive(file, &ctx.root, force, allow_unsigned, key.as_deref(), "explicit")?;
    Ok(())
}

pub(crate) fn cmd_remove(ctx: &Ctx, name: &str, force: bool, yes: bool) -> Result<()> {
    let db = db::Db::new(&ctx.root);
    let entry = match db.entry(name)? {
        Some(e) => e,
        None => bail!("`{name}` is not installed"),
    };
    let dependents: Vec<String> = db
        .installed()?
        .into_iter()
        .filter(|e| e.name != name && e.run.iter().any(|r| r == name))
        .map(|e| e.name)
        .collect();
    let nfiles = db.files(name)?.len();

    // the confirmation shows what will actually happen; the removal itself
    // re-checks dependents (refusing without --force) after the prompt
    let pal = Palette::auto(Stream::Stdout);
    println!("nawa: {} {} ({} file(s))", pal.warn("removing"), pal.name(name), nfiles);
    println!("     {} {} — {}", pal.dim("version"), pal.ver(&entry.version_full()), pal.dim(&format_date(entry.install_date)));
    if !dependents.is_empty() {
        println!(
            "     {} {}",
            pal.warn("required by:"),
            pal.name(&dependents.join(", "))
        );
        if !force {
            println!(
                "     {}",
                pal.warn("removal will fail unless --force is passed")
            );
        }
    }
    confirm::confirm(
        &format!("nawa: remove {name} from {}?", ctx.root.display()),
        yes,
    )?;
    install::remove_with(name, &ctx.root, force)?;
    // the removed package is gone: this checks what depended on it
    warn_revdep(ctx, &[name.to_string()]);
    Ok(())
}
