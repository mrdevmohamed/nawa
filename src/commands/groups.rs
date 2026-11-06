//! `nawa group` command handlers (roadmap Phase 5). Groups are metadata
//! that resolve to ordinary package names; installs go through the normal
//! planner (cmd_install), removals recompute reachability from the
//! remaining explicit/group roots.

use crate::*;
use anyhow::bail;

pub(crate) fn cmd_group_list(ctx: &Ctx) -> Result<()> {
    let installed = groups::membership::load_all(&ctx.root)?;
    if installed.is_empty() {
        println!("nawa: no groups installed");
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

pub(crate) fn cmd_group_install(_ctx: &Ctx, id: &str) -> Result<()> {
    groups::check_group_id(id)?;
    // group definitions come from authenticated repository metadata; without
    // a repository this first implementation refuses rather than guessing
    bail!(
        "group {id} has no published definition in the configured repositories — \
         publishing group metadata (groups.toml referenced by the signed index) \
         is required before `nawa group install` can resolve it"
    );
}

pub(crate) fn cmd_group_remove(ctx: &Ctx, id: &str, yes: bool) -> Result<()> {
    groups::check_group_id(id)?;
    let Some(record) = groups::membership::load(&ctx.root, id)? else {
        bail!("group {id} is not installed");
    };
    let db = db::Db::new(&ctx.root);
    let installed = db.installed()?;
    // packages still owned by another group or explicit stay
    let other_groups = groups::membership::group_owned_packages(&ctx.root, Some(id))?;
    let explicit: std::collections::BTreeSet<&str> = installed
        .iter()
        .filter(|e| e.reason == "explicit")
        .map(|e| e.name.as_str())
        .collect();
    let mut candidates: Vec<String> = record
        .packages
        .iter()
        .filter(|p| !other_groups.contains(*p) && !explicit.contains(p.as_str()))
        .cloned()
        .collect();
    // dependent-first removal order, rechecking live reverse dependencies
    candidates = plan::removal_order(&candidates, &installed);
    // a package required by something outside the removal set stays
    let cand_set: std::collections::BTreeSet<String> = candidates.iter().cloned().collect();
    candidates.retain(|p| {
        installed
            .iter()
            .filter(|e| !cand_set.contains(&e.name))
            .all(|e| !e.run.iter().any(|r| r == p))
    });

    println!("nawa: removing group {} (membership record)", id);
    if candidates.is_empty() {
        println!("nawa: no packages to remove (all still owned by groups or explicit requests)");
        groups::membership::remove(&ctx.root, id)?;
        return Ok(());
    }
    for p in &candidates {
        println!("  remove {p}");
    }
    confirm::confirm(&format!("nawa: remove {} package(s) of group {id}?", candidates.len()), yes)?;
    let mut op = transaction::begin_operation(
        &ctx.root,
        transaction::OperationCommand::GroupRemove,
        &[id.to_string()],
        &[],
    )?;
    op.record_group(id);
    let mut err: Option<anyhow::Error> = None;
    for p in &candidates {
        if let Err(e) = install::remove_with(p, &ctx.root, false) {
            err = Some(e);
            break;
        }
    }
    // the membership drops only with a recorded transaction result
    if err.is_none() {
        groups::membership::remove(&ctx.root, id)?;
    }
    op.finish(err.as_ref())?;
    match err {
        Some(e) => Err(e),
        None => {
            println!("nawa: group {id} removed ({} package(s))", candidates.len());
            Ok(())
        }
    }
}
