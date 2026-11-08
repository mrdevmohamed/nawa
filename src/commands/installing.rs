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
    cmd_install_with_why(ctx, targets, flags, no_deps, key, mode, repo, plan::Why::Explicit)
}

#[allow(clippy::too_many_arguments)] // mirrors cmd_install and adds an explicit install reason
pub(crate) fn cmd_install_with_why(
    ctx: &Ctx,
    targets: &[String],
    flags: &PlanFlags,
    no_deps: bool,
    key: Option<PathBuf>,
    mode: Option<InstallMode>,
    repo: Option<String>,
    target_why: plan::Why,
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

    let known = recipe::load_all(&ctx.recipes)?;
    let installed = installed_map(ctx)?;

    // Complete interrupted (or legacy, untracked) hooks before consulting the
    // repository. A retry must work for binary-only packages even if the
    // repository is temporarily unavailable, and must not reinstall payloads.
    let mut remaining_targets = Vec::new();
    for t in targets {
        if installed.contains_key(t) && install::hook_needs_retry(&ctx.root, t)? {
            crate::nawa_info!("retrying incomplete post-install hook for {t}");
            let _lock = lock::acquire(&ctx.root, lock::Mode::Exclusive)?;
            install::retry_pending_hook(t, &ctx.root)?;
            // Recipe-backed targets still go through normal source/version
            // selection after the hook is repaired.
            if known.contains_key(t) {
                remaining_targets.push(t.clone());
            }
        } else {
            remaining_targets.push(t.clone());
        }
    }
    if remaining_targets.is_empty() {
        return Ok(());
    }

    // where packages come from: flag > install_mode (env / nawa.toml / default)
    let mode = mode.unwrap_or(ctx.install_mode);
    let repo_url = repo.or_else(|| ctx.repo.clone());
    let sourcing = open_sourcing(ctx, mode, repo_url.as_deref(), flags)?;

    // a name without a recipe can still be installed when the repository has
    // it (a binary-only package: no dependency resolution, no build)
    let mut recipe_targets: Vec<String> = Vec::new();
    let mut extras: Vec<repo::RepoEntry> = Vec::new();
    for t in &remaining_targets {
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
                    crate::nawa_info!("{t} is already installed and up to date");
                }
                _ => extras.push(re.clone()),
            },
            // neither a recipe nor a repository package: the planner reports
            // it with a "did you mean" hint
            None => recipe_targets.push(t.clone()),
        }
    }

    let plan = plan::install_plan(&recipe_targets, &installed, &known, target_why, !no_deps)?;
    for name in &plan.up_to_date {
        crate::nawa_info!("{name} is already installed and up to date");
    }
    if plan.items.is_empty() && extras.is_empty() {
        return Ok(());
    }
    // one transaction for the whole command (roadmap §5.1): the id is
    // allocated under the exclusive db lock inside begin_operation
    let requested: Vec<String> = remaining_targets.clone();
    let repo_ids: Vec<String> = sourcing
        .open
        .as_ref()
        .and_then(|o| o.base_url.clone())
        .into_iter()
        .collect();
    {
        let _lock = lock::acquire(&ctx.root, lock::Mode::Exclusive)?;
        let mut op = transaction::begin_operation(
            &ctx.root,
            transaction::OperationCommand::Install,
            &requested,
            &repo_ids,
        )?;
        drop(_lock);
        let result = run_plan(ctx, &plan, flags, &sourcing, &extras, &mut op, target_why);
        let err = result.err();
        {
            let _lock = lock::acquire(&ctx.root, lock::Mode::Exclusive)?;
            op.finish(err.as_ref())?;
        }
        match err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
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
    /// Always build — the default for `update` and explicit `--build`.
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
            crate::nawa_note!("no repository configured — building from source");
            return Ok(Sourcing::build_only());
        }
    };
    let repo_ref = fetch::RepoRef::parse(url)?;
    match upgrade::open_repo(&ctx.root, &repo_ref, flags.allow_unsigned, flags.force) {
        Ok(open) => Ok(Sourcing { mode: eff, ask, open: Some(open) }),
        Err(e) if strict => Err(e).with_context(|| format!("repository {url} is not usable")),
        Err(e) => {
            crate::nawa_note!("repository {url} is not usable ({e:#}) — building from source");
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

/// Details for one staged archive installation.
struct InstallRequest<'a> {
    reason: &'a str,
    what: &'a str,
    number: usize,
    total: usize,
    /// SHA-256 the repository index promised for this archive (repository
    /// sources only). Always pinned, so the installed bytes are the bytes
    /// that were validated against the index entry.
    index_sha256: Option<&'a str>,
    /// The index was authenticated by a trusted key and replaces the
    /// per-archive detached signature (schema v2).
    signed_index: bool,
    /// Where this archive comes from, for the history record.
    source: &'a str,
}

/// Install one ready archive. The library functions never lock; hold the db
/// lock for this one install only, never through a build or a download.
fn install_one(
    ctx: &Ctx,
    archive: &Path,
    flags: &PlanFlags,
    request: InstallRequest<'_>,
    open_repo: Option<&upgrade::OpenRepo>,
    op: &mut transaction::OperationHandle,
) -> Result<()> {
    crate::nawa_info!(
        "[{}/{}] installing {}",
        request.number, request.total, request.what
    );
    let _lock = lock::acquire(&ctx.root, lock::Mode::Exclusive)?;
    // Resolve the real package identity from .PKGINFO; archive filenames
    // are not authoritative package metadata.
    let package = archive_name_of(archive)
        .ok_or_else(|| anyhow::anyhow!("cannot read package name from {}", archive.display()))?;
    let db = db::Db::new(&ctx.root);
    let before = db.entry(&package)?;
    // A package that was explicitly requested must stay explicit even when
    // a group later upgrades it. "keep" preserves the existing DB reason.
    let effective_reason = if request.reason == "group"
        && before.as_ref().is_some_and(|entry| entry.reason == "explicit")
    {
        "keep"
    } else {
        request.reason
    };
    let target = install::inspect_archive_state(archive, effective_reason, before.as_ref())?;
    if let Some(expected) = request.index_sha256 {
        if expected != target.artifact_sha256 {
            bail!(
                "archive {} changed after repository verification: index expected {}, inspected {}",
                archive.display(), expected, target.artifact_sha256
            );
        }
    }
    let action = match &before {
        None => transaction::history::StepAction::Install,
        Some(old) if old.version != target.version.clone().unwrap_or_default()
            || Some(old.rel) != target.rel => transaction::history::StepAction::Upgrade,
        Some(_) => transaction::history::StepAction::Reinstall,
    };
    let mut recorder = op.recorder();
    let step_no = recorder.step_intent_state(
        &package, action, before.as_ref(), Some(&target), &target.artifact_sha256,
    )?;
    let installed = match request.index_sha256 {
        Some(expected) if request.signed_index => {
            let proof = open_repo.map(|open| install::IndexProof {
                index_bytes: Some(&open.index_bytes),
                index_signature: open.index_signature.as_deref(),
                signer_public: open.signer_public.as_deref(),
                repo_id: open.base_url.as_deref(),
            });
            install::install_archive_from_index_proof(
                archive, &ctx.root, flags.force, expected, effective_reason, proof,
            )
        }
        pinned => {
            let expected = pinned.unwrap_or(target.artifact_sha256.as_str());
            install::install_archive_pinned(
                archive, &ctx.root, flags.force, flags.allow_unsigned, None,
                effective_reason, Some(expected),
            )
        },
    };
    match &installed {
        Ok(entry) => {
            recorder.step_result(
                step_no, &entry.name, action,
                transaction::history::StepStatus::Committed,
                before.as_ref(), Some(entry), request.source, None, None,
            )?;
        }
        Err(error) => {
            let current = db.entry(&package)?;
            let target_is_current = current.as_ref().is_some_and(|entry| target.matches_entry(entry));
            let retry_hook = install::hook_needs_retry(&ctx.root, &package).unwrap_or(false);
            let committed_hook_failed = target_is_current
                && (before.as_ref().is_none_or(|old| !target.matches_entry(old)) || retry_hook);
            recorder.step_result(
                step_no, &package, action,
                if committed_hook_failed {
                    transaction::history::StepStatus::CommittedHookFailed
                } else {
                    transaction::history::StepStatus::Failed
                },
                before.as_ref(),
                if committed_hook_failed { current.as_ref() } else { None },
                request.source,
                Some(format!("{error:#}")),
                committed_hook_failed.then(|| "failed".to_string()),
            )?;
        }
    }
    installed.with_context(|| {
        format!(
            "[{}/{}] install of {} failed",
            request.number, request.total, request.what
        )
    })?;
    Ok(())
}

/// The package name inside an archive (from .PKGINFO), when readable.
fn archive_name_of(archive: &Path) -> Option<String> {
    let raw = crate::archive::read_pkginfo(archive).ok()?;
    let info = crate::archive::parse_pkginfo(&raw).ok()?;
    info.get("pkgname").cloned()
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
    op: &mut transaction::OperationHandle,
    extras_why: plan::Why,
) -> Result<()> {
    let total = plan.items.len() + extras.len();
    if total == 0 {
        crate::nawa_info!("nothing to do");
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
        crate::nawa_info!(
            "{total} package(s) to install into {} ({from_repo} from the repository, {} to build)",
            ctx.root.display(),
            total - from_repo
        );
        // one row per package: dnf-style table with sizes
        struct Row {
            name: String,
            ver: String,
            tag: &'static str,
            dl: u64,
            inst: u64,
            note: String,
        }
        let mut rows: Vec<Row> = Vec::with_capacity(total);
        for (i, it) in plan.items.iter().enumerate() {
            let (tag, dl, inst) = match &sources[i] {
                Source::Repo(re) => ("repo", re.size, re.installed_size),
                Source::Build => ("build", 0, 0),
            };
            let mut note = it.why.label().to_string();
            if let Some(old) = &it.replaces {
                note.push_str(&format!(" (replaces {old})"));
            }
            rows.push(Row {
                name: it.rec.package.name.clone(),
                ver: format!("{}-{}", it.rec.package.version, it.rec.package.rel),
                tag,
                dl,
                inst,
                note,
            });
        }
        for re in extras {
            rows.push(Row {
                name: re.name.clone(),
                ver: format!("{}-{}", re.version, re.rel),
                tag: "repo",
                dl: re.size,
                inst: re.installed_size,
                note: "explicit (no recipe: dependencies not resolved)".to_string(),
            });
        }
        let size_cell = |n: u64| if n == 0 { "-".to_string() } else { format_size(n) };
        let wn = rows.iter().map(|r| r.name.len()).max().unwrap_or(0).max("Package".len());
        let wv = rows.iter().map(|r| r.ver.len()).max().unwrap_or(0).max("Version".len());
        let wd = rows.iter().map(|r| size_cell(r.dl).len()).max().unwrap_or(0).max("Download".len());
        let wi = rows.iter().map(|r| size_cell(r.inst).len()).max().unwrap_or(0).max("Installed".len());
        println!();
        println!(
            "{}",
            pal.header(&format!(
                "  {:>3}  {:<wn$}  {:<wv$}  {:<6}  {:>wd$}  {:>wi$}  Reason",
                "#", "Package", "Version", "Source", "Download", "Installed"
            ))
        );
        for (i, r) in rows.iter().enumerate() {
            let (dl, inst) = (size_cell(r.dl), size_cell(r.inst));
            println!(
                "  {:>3}  {}  {}  {}  {dl:>wd$}  {inst:>wi$}  {}",
                i + 1,
                pal.name(&format!("{:<wn$}", r.name)),
                pal.ver(&format!("{:<wv$}", r.ver)),
                pal.dim(&format!("{:<6}", r.tag)),
                pal.dim(&r.note),
            );
        }
        let dl_sum: u64 = rows.iter().map(|r| r.dl).sum();
        let inst_sum: u64 = rows.iter().map(|r| r.inst).sum();
        let unknown = rows.iter().filter(|r| r.inst == 0).count();
        println!();
        let mut line = format!(
            "nawa: download size {}, installed size {}",
            format_size(dl_sum),
            format_size(inst_sum)
        );
        if unknown > 0 {
            line.push_str(&format!(" (not counting {unknown} package(s) of unknown size)"));
        }
        println!("{line}");
    };
    print_plan(sources.as_slice());
    if flags.dry_run {
        crate::nawa_info!("dry run — nothing was built or installed");
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
            crate::nawa_info!("building all {total} package(s) from source");
        }
    }
    confirm::confirm(&format!("nawa: install {total} package(s)?"), flags.yes)?;

    let started = std::time::Instant::now();
    for (i, it) in plan.items.iter().enumerate() {
        // Ctrl+C during the previous install was deferred until that package
        // was complete; now that the system is consistent, stop here
        crate::interrupt::check()?;
        let n = i + 1;
        let id = it.rec.id();
        let mut step = || -> Result<()> {
            let (archive, index_sha256, signed_index): (PathBuf, Option<String>, bool) = match &sources[i] {
                Source::Build => (build_one(ctx, it, flags, n, total)?, None, false),
                Source::Repo(re) => {
                    crate::nawa_info!("[{n}/{total}] fetching {id} from the repository");
                    let open = sourcing
                        .open
                        .as_ref()
                        .context("internal: repository source without an open repository")?;
                    match upgrade::fetch_entry(&ctx.root, open, re, flags.allow_unsigned) {
                        Ok(a) => (
                            a,
                            Some(re.sha256.clone()),
                            open.archive_auth == upgrade::ArchiveAuth::SignedIndexSha256,
                        ),
                        // strict repo mode never builds behind the user's back
                        Err(e) if sourcing.mode == InstallMode::Repo => {
                            return Err(e)
                                .with_context(|| format!("[{n}/{total}] repository fetch of {id} failed"));
                        }
                        // auto: the recipe pins its sources, so a build is a safe fallback
                        Err(e) => {
                            crate::nawa_warn!(
                                "cannot use the repository copy of {id} ({e:#}) — building from source"
                            );
                            (build_one(ctx, it, flags, n, total)?, None, false)
                        }
                    }
                }
            };
            let source = match &sources[i] {
                Source::Repo(_) => "repository",
                Source::Build => "local-build",
            };
            install_one(
                ctx,
                &archive,
                flags,
                InstallRequest {
                    reason: it.why.db_reason(),
                    what: &id,
                    number: n,
                    total,
                    index_sha256: index_sha256.as_deref(),
                    signed_index,
                    source,
                },
                sourcing.open.as_ref(),
                op,
            )
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
        crate::interrupt::check()?;
        let n = plan.items.len() + j + 1;
        let what = format!("{}-{}-{}", re.name, re.version, re.rel);
        let mut step = || -> Result<()> {
            crate::nawa_info!("[{n}/{total}] fetching {what} from the repository");
            let open = sourcing
                .open
                .as_ref()
                .context("internal: repository package without an open repository")?;
            let archive = upgrade::fetch_entry(&ctx.root, open, re, flags.allow_unsigned)
                .with_context(|| format!("[{n}/{total}] repository fetch of {what} failed"))?;
            let signed_index = open.archive_auth == upgrade::ArchiveAuth::SignedIndexSha256;
            install_one(
                ctx,
                &archive,
                flags,
                InstallRequest {
                    reason: extras_why.db_reason(),
                    what: &what,
                    number: n,
                    total,
                    index_sha256: Some(re.sha256.as_str()),
                    signed_index,
                    source: "repository",
                },
                sourcing.open.as_ref(),
                op,
            )
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
    crate::nawa_success!(
        "done — {total} package(s) installed in {}",
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
        crate::nawa_info!("everything is up to date ({} installed)", installed.len());
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
    crate::nawa_info!("{} package(s) can be updated — run `nawa update`", list.len());
    Ok(())
}

pub(crate) fn cmd_update(
    ctx: &Ctx,
    names: &[String],
    flags: &PlanFlags,
    mode: Option<InstallMode>,
    repo: Option<String>,
) -> Result<()> {
    let mode = mode.unwrap_or_else(|| {
        if repo.is_some() { InstallMode::Repo } else { InstallMode::Build }
    });

    let known = recipe::load_all(&ctx.recipes)?;
    let installed = installed_map(ctx)?;
    for n in names {
        if !installed.contains_key(n) {
            bail!("`{n}` is not installed (use `nawa install {n}`)");
        }
    }
    let list: Vec<db::Entry> = installed.values().cloned().collect();
    let mut targets: Vec<String> = plan::outdated(&list, &known)
        .into_iter()
        .map(|(e, _)| e.name.clone())
        .filter(|n| names.is_empty() || names.contains(n))
        .collect();
    targets.sort(); // deterministic update plan even though installed_map is a HashMap
    if targets.is_empty() {
        crate::nawa_info!("everything is up to date");
        return Ok(());
    }
    let plan = plan::install_plan(&targets, &installed, &known, plan::Why::Update, true)?;
    let repo_url = repo.or_else(|| ctx.repo.clone());
    let sourcing = open_sourcing(ctx, mode, repo_url.as_deref(), flags)?;

    // In strict repository mode, every update target must be available at
    // least at the version requested by its recipe. Never silently downgrade
    // or report a stale repository package as a completed update.
    if mode == InstallMode::Repo {
        for item in &plan.items {
            let name = &item.rec.package.name;
            let re = sourcing.entry(name).ok_or_else(|| {
                anyhow::anyhow!("{name} has no package for this architecture in the repository")
            })?;
            let available = format!("{}-{}", re.version, re.rel);
            let wanted = format!("{}-{}", item.rec.package.version, item.rec.package.rel);
            if version::vercmp(&available, &wanted) == std::cmp::Ordering::Less {
                bail!(
                    "repository has {name} {available}, but the recipe requires {wanted}; publish the newer package first, or use --from auto / --build"
                );
            }
        }
    }

    let requested = targets.clone();
    let repo_ids: Vec<String> = sourcing
        .open
        .as_ref()
        .and_then(|o| o.base_url.clone())
        .into_iter()
        .collect();
    let mut op = {
        let _lock = lock::acquire(&ctx.root, lock::Mode::Exclusive)?;
        transaction::begin_operation(
            &ctx.root,
            transaction::OperationCommand::Update,
            &requested,
            &repo_ids,
        )?
    };
    let result = run_plan(ctx, &plan, flags, &sourcing, &[], &mut op, plan::Why::Update);
    let err = result.err();
    {
        let _lock = lock::acquire(&ctx.root, lock::Mode::Exclusive)?;
        op.finish(err.as_ref())?;
    }
    // also after a partial run: whatever was installed is worth checking
    let changed: Vec<String> = plan.items.iter().map(|it| it.rec.package.name.clone()).collect();
    warn_revdep(ctx, &changed);
    match err {
        Some(e) => Err(e),
        None => Ok(()),
    }
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
    // Membership files are the authoritative roots for package groups. Do
    // not let autoremove discard a direct group member merely because it was
    // originally pulled in as a dependency before the group was installed.
    let group_owned = groups::membership::group_owned_packages(&ctx.root, None)?;
    let mut orphan_view = installed.clone();
    for entry in &mut orphan_view {
        if group_owned.contains(&entry.name) && entry.reason == "dependency" {
            entry.reason = "group".to_string();
        }
    }
    let orphans = plan::orphans(&orphan_view, &known, build_deps);
    if orphans.is_empty() {
        crate::nawa_info!("nothing to remove");
        return Ok(());
    }
    let order = plan::removal_order(&orphans, &installed);
    let pal = Palette::auto(Stream::Stdout);
    crate::nawa_info!("{} unneeded dependency package(s):", order.len());
    for n in &order {
        println!("  {}", pal.name(n));
    }
    if dry_run {
        crate::nawa_info!("dry run — nothing was removed");
        return Ok(());
    }
    confirm::confirm(&format!("nawa: remove {} package(s)?", order.len()), yes)?;
    // the command holds the exclusive db lock for its whole duration
    // (see cli::lock_mode), so no per-step locking happens here
    let mut op = transaction::begin_operation(
        &ctx.root,
        transaction::OperationCommand::Autoremove,
        &[],
        &[],
    )?;
    for (i, n) in order.iter().enumerate() {
        crate::interrupt::check()?;
        crate::nawa_info!("[{}/{}] removing {n}", i + 1, order.len());
        let db = db::Db::new(&ctx.root);
        let before = db.entry(n)?;
        let mut recorder = op.recorder();
        let step_no = recorder.step_intent(
            n,
            transaction::history::StepAction::Remove,
            before.as_ref(),
            None,
            "",
        )?;
        match install::remove_with(n, &ctx.root, false) {
            Ok(()) => {
                recorder.step_result(
                    step_no, n, transaction::history::StepAction::Remove,
                    transaction::history::StepStatus::Committed,
                    before.as_ref(), None, "autoremove", None, None,
                )?;
            }
            Err(e) => {
                let after = db.entry(n)?;
                let committed_hook_failed = before.is_some() && after.is_none();
                recorder.step_result(
                    step_no, n, transaction::history::StepAction::Remove,
                    if committed_hook_failed {
                        transaction::history::StepStatus::CommittedHookFailed
                    } else {
                        transaction::history::StepStatus::Failed
                    },
                    before.as_ref(), after.as_ref(), "autoremove", Some(format!("{e:#}")),
                    committed_hook_failed.then(|| "failed".to_string()),
                )?;
                let err = e;
                op.finish(Some(&err))?;
                return Err(err);
            }
        }
    }
    op.finish(None)?;
    crate::nawa_success!("done — {} package(s) removed", order.len());
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
    // the caller (cmd_install) holds the exclusive lock for archive installs
    let db = db::Db::new(&ctx.root);
    let pkg_name = archive_name_of(file)
        .ok_or_else(|| anyhow::anyhow!("cannot read package name from {}", file.display()))?;
    let before = db.entry(&pkg_name)?;
    let target = install::inspect_archive_state(file, "explicit", before.as_ref())?;
    let action = match &before {
        None => transaction::history::StepAction::Install,
        Some(old) if old.version != target.version.clone().unwrap_or_default()
            || Some(old.rel) != target.rel => transaction::history::StepAction::Upgrade,
        Some(_) => transaction::history::StepAction::Reinstall,
    };
    let digest = target.artifact_sha256.clone();
    let mut op = transaction::begin_operation(
        &ctx.root,
        transaction::OperationCommand::ArchiveInstall,
        std::slice::from_ref(&pkg_name),
        &[],
    )?;
    let mut recorder = op.recorder();
    let step_no = recorder.step_intent_state(
        &pkg_name, action, before.as_ref(), Some(&target), &digest,
    )?;
    let result = install::install_archive_pinned(
        file, &ctx.root, force, allow_unsigned, key.as_deref(), "explicit", Some(&digest),
    );
    match &result {
        Ok(entry) => {
            recorder.step_result(
                step_no, &entry.name, action,
                transaction::history::StepStatus::Committed,
                before.as_ref(), Some(entry), "archive", None, None,
            )?;
        }
        Err(error) => {
            let current = db.entry(&pkg_name)?;
            let target_is_current = current.as_ref().is_some_and(|entry| target.matches_entry(entry));
            let retry_hook = install::hook_needs_retry(&ctx.root, &pkg_name).unwrap_or(false);
            let committed_hook_failed = target_is_current
                && (before.as_ref().is_none_or(|old| !target.matches_entry(old)) || retry_hook);
            recorder.step_result(
                step_no, &pkg_name, action,
                if committed_hook_failed { transaction::history::StepStatus::CommittedHookFailed }
                else { transaction::history::StepStatus::Failed },
                before.as_ref(),
                if committed_hook_failed { current.as_ref() } else { None },
                "archive", Some(format!("{error:#}")),
                committed_hook_failed.then(|| "failed".to_string()),
            )?;
        }
    }
    op.finish(result.as_ref().err())?;
    result?;
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
    crate::nawa_info!("{} {} ({} file(s))", pal.warn("removing"), pal.name(name), nfiles);
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
    let db = db::Db::new(&ctx.root);
    let before = db.entry(name)?;
    let mut op = transaction::begin_operation(
        &ctx.root,
        transaction::OperationCommand::Remove,
        &[name.to_string()],
        &[],
    )?;
    let mut recorder = op.recorder();
    let step_no = recorder.step_intent(
        name,
        transaction::history::StepAction::Remove,
        before.as_ref(),
        None,
        before.as_ref().map(|b| b.artifact_sha256.as_str()).unwrap_or(""),
    )?;
    let result = install::remove_with(name, &ctx.root, force);
    match &result {
        Ok(()) => {
            recorder.step_result(
                step_no, name, transaction::history::StepAction::Remove,
                transaction::history::StepStatus::Committed,
                before.as_ref(), None, "remove", None, None,
            )?;
        }
        Err(e) => {
            let after = db.entry(name)?;
            let committed_hook_failed = before.is_some() && after.is_none();
            recorder.step_result(
                step_no, name, transaction::history::StepAction::Remove,
                if committed_hook_failed {
                    transaction::history::StepStatus::CommittedHookFailed
                } else {
                    transaction::history::StepStatus::Failed
                },
                before.as_ref(), after.as_ref(), "remove", Some(format!("{e:#}")),
                committed_hook_failed.then(|| "failed".to_string()),
            )?;
        }
    }
    op.finish(result.as_ref().err())?;
    result?;
    // the removed package is gone: this checks what depended on it
    warn_revdep(ctx, &[name.to_string()]);
    Ok(())
}
