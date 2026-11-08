//! `nawa group` command handlers (roadmap Phase 5). Groups are metadata
//! that resolve to ordinary package names; installs go through the normal
//! planner (cmd_install), removals recompute reachability from the
//! remaining explicit/group roots.

use crate::*;
use anyhow::bail;

pub(crate) fn cmd_group_list(ctx: &Ctx) -> Result<()> {
    let installed = groups::membership::load_all(&ctx.root)?;
    if installed.is_empty() {
        crate::nawa_info!("no groups installed");
        return Ok(());
    }
    println!("{:<14}  {:<10}  Packages", "ID", "Installed");
    println!("{}", "-".repeat(52));
    for g in &installed {
        println!(
            "{:<14}  {:<10}  {}",
            g.group_id,
            crate::humanize::format_date(g.installed_at),
            g.packages.join(" ")
        );
    }
    Ok(())
}

pub(crate) fn cmd_group_info(ctx: &Ctx, id: &str) -> Result<()> {
    groups::check_group_id(id)?;
    let Some(g) = groups::membership::load(&ctx.root, id)? else {
        bail!("group {id} is not installed (install it with `nawa group install {id}`)");
    };
    println!("Group:     {} ({})", g.group_id, g.packages.len());
    println!("Installed: {}", crate::humanize::format_date(g.installed_at));
    if !g.source_repository.is_empty() {
        println!("Source:    {}", g.source_repository);
    }
    let db = db::Db::new(&ctx.root);
    println!();
    for p in &g.packages {
        let state = match db.entry(p)? {
            Some(e) => format!("{} ({})", e.version_full(), e.reason),
            None => "MISSING".to_string(),
        };
        println!("  {:<24} {}", p, state);
    }
    Ok(())
}

pub(crate) fn cmd_group_install(
    ctx: &Ctx,
    id: &str,
    flags: &PlanFlags,
    with_optional: bool,
) -> Result<()> {
    groups::check_group_id(id)?;
    let repo_url = ctx.repo.as_deref().filter(|url| !url.is_empty()).with_context(|| {
        "no repository configured; set repo in /etc/nawa/nawa.toml"
    })?;
    let repo_ref = fetch::RepoRef::parse(repo_url)?;
    let open = upgrade::open_repo(&ctx.root, &repo_ref, flags.allow_unsigned, false)
        .with_context(|| format!("cannot read group metadata from repository {repo_url}"))?;
    if open.signer_public.is_none() {
        bail!("package groups require a repository index signed by a key in the configured trusted-key store");
    }
    let catalog = open.groups.as_ref().with_context(|| {
        format!("repository {repo_url} does not publish groups.toml in its signed index")
    })?;
    let definition = catalog.find(id).cloned().with_context(|| {
        format!("repository {repo_url} has no group named {id}")
    })?;
    let packages = definition.effective_packages(with_optional);
    if packages.is_empty() {
        bail!("group {id} has no packages selected for installation");
    }
    if let Some(existing) = groups::membership::load(&ctx.root, id)? {
        if existing.packages == packages {
            crate::nawa_info!("group {id} is already installed");
            return Ok(());
        }
        bail!(
            "group {id} is already installed with a different package selection; \
             remove it first with `nawa group remove {id}`, then install it again"
        );
    }
    let definition_revision = open.index.groups.as_ref()
        .map(|entry| entry.sha256.clone()).unwrap_or_default();
    let source_repository = open.base_url.clone()
        .unwrap_or_else(|| open.dir.display().to_string());

    crate::nawa_info!(
        "installing group {} — {} package(s){}",
        definition.name,
        packages.len(),
        if with_optional { " (including optional)" } else { "" }
    );
    crate::commands::installing::cmd_install_with_why(
        ctx, &packages, flags, false, None, None, None, plan::Why::Group, Some(id),
    )?;
    if !flags.dry_run {
        let _lock = lock::acquire(&ctx.root, lock::Mode::Exclusive)?;
        let installed_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_secs())
            .unwrap_or(0);
        groups::membership::save(&ctx.root, &groups::membership::MembershipRecord {
            schema: 1,
            group_id: id.to_string(),
            definition_revision,
            installed_at,
            source_repository,
            packages,
        })?;
        crate::nawa_success!("group {id} installed");
    }
    Ok(())
}

pub(crate) fn cmd_group_remove(ctx: &Ctx, id: &str, yes: bool) -> Result<()> {
    groups::check_group_id(id)?;
    let Some(record) = groups::membership::load(&ctx.root, id)? else {
        bail!("group {id} is not installed");
    };
    let db = db::Db::new(&ctx.root);
    let installed = db.installed()?;
    let installed_names: std::collections::BTreeSet<String> =
        installed.iter().map(|entry| entry.name.clone()).collect();
    let other_groups = groups::membership::group_owned_packages(&ctx.root, Some(id))?;
    let explicit: std::collections::BTreeSet<&str> = installed.iter()
        .filter(|entry| entry.reason == "explicit")
        .map(|entry| entry.name.as_str()).collect();
    let mut candidates: Vec<String> = record.packages.iter()
        .filter(|name| installed_names.contains(*name))
        .filter(|name| !other_groups.contains(*name) && !explicit.contains(name.as_str()))
        .cloned().collect();
    candidates = plan::removal_order(&candidates, &installed);
    let candidate_set: std::collections::BTreeSet<String> = candidates.iter().cloned().collect();
    candidates.retain(|package| installed.iter()
        .filter(|entry| !candidate_set.contains(&entry.name))
        .all(|entry| !entry.run.iter().any(|dependency| dependency == package)));

    crate::nawa_info!("removing group {} (membership record)", id);
    for package in &candidates { println!("  remove {package}"); }
    if !candidates.is_empty() {
        confirm::confirm(&format!("nawa: remove {} package(s) of group {id}?", candidates.len()), yes)?;
    }

    let mut op = transaction::begin_operation(
        &ctx.root, transaction::OperationCommand::GroupRemove, &[id.to_string()], &[],
    )?;
    op.record_group(id);
    let mut failure: Option<anyhow::Error> = None;
    for package in &candidates {
        let before = db.entry(package)?;
        let step_no = {
            let mut recorder = op.recorder();
            recorder.step_intent(
                package, transaction::history::StepAction::Remove,
                before.as_ref(), None,
                before.as_ref().map(|entry| entry.artifact_sha256.as_str()).unwrap_or(""),
            )?
        };
        let result = install::remove_with(package, &ctx.root, false);
        let after = db.entry(package)?;
        let committed_hook_failed = result.is_err() && before.is_some() && after.is_none();
        {
            let mut recorder = op.recorder();
            recorder.step_result(
                step_no, package, transaction::history::StepAction::Remove,
                if result.is_ok() { transaction::history::StepStatus::Committed }
                else if committed_hook_failed { transaction::history::StepStatus::CommittedHookFailed }
                else { transaction::history::StepStatus::Failed },
                before.as_ref(),
                after.as_ref(),
                "group-remove",
                result.as_ref().err().map(|error| format!("{error:#}")),
                committed_hook_failed.then(|| "failed".to_string()),
            )?;
        }
        if let Err(error) = result { failure = Some(error); break; }
    }
    match failure {
        Some(error) => {
            op.finish(Some(&error))?;
            Err(error)
        }
        None => {
            // Persist history first. A retry can safely clean up membership if
            // interruption happens between the two durable operations.
            op.finish(None)?;
            groups::membership::remove(&ctx.root, id)?;
            crate::nawa_info!("group {id} removed ({} package(s))", candidates.len());
            Ok(())
        }
    }
}
