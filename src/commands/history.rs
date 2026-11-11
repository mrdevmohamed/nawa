//! `nawa history` presentation: the transaction table and the details of one
//! transaction. Pure output over `transaction::history`; applying an undo
//! lives in `undo`.

use anyhow::Result;
use std::path::Path;

use crate::transaction::history::PackageChange;

/// The `nawa history` table (roadmap §5.8): newest first, stable ids.
pub(crate) fn print_history(root: &Path, limit: usize) -> Result<()> {
    let records = crate::transaction::history::list_records(root)?;
    if records.is_empty() {
        crate::nawa_info!("no transaction history yet");
        return Ok(());
    }
    println!(
        "{:<6}  {:<16}  {:<13}  {:<9}  Result",
        "ID", "Date", "Command", "Packages"
    );
    println!("{}", "-".repeat(60));
    for r in records.iter().rev().take(limit) {
        println!(
            "{:<6}  {:<16}  {:<13}  {:<9}  {}",
            r.id,
            crate::humanize::format_datetime(r.started_at),
            r.command,
            r.package_changes.len(),
            r.status,
        );
    }
    Ok(())
}

/// `nawa history info <ID>`: the full story of one transaction.
pub(crate) fn print_info(root: &Path, id: u64) -> Result<()> {
    let r = crate::transaction::history::load_record(root, id)?;
    println!("Transaction {}", r.id);
    println!("  Command:   {} {}", r.command, r.requested_packages.join(" "));
    println!("  Status:    {}", r.status);
    println!("  Started:   {}", crate::humanize::format_datetime(r.started_at));
    if let Some(f) = r.finished_at {
        println!("  Finished:  {}", crate::humanize::format_datetime(f));
    }
    if let Some(d) = r.duration_seconds {
        println!("  Duration:  {}", crate::humanize::format_duration(d));
    }
    if !r.initiated_by.is_empty() {
        println!("  Initiated: {}", r.initiated_by);
    }
    if !r.repository_ids.is_empty() {
        println!("  Repos:     {}", r.repository_ids.join(", "));
    }
    if let Some(u) = r.undone_by {
        println!("  Undone by: transaction {u}");
    }
    if let Some(u) = r.undoes {
        println!("  Undoes:    transaction {u}");
    }
    if r.package_changes.is_empty() {
        println!("  No package changes");
    }
    for c in &r.package_changes {
        println!();
        println!("  {}: {} ({})", c.name, c.action, c.status);
        if let Some(b) = &c.before {
            if let Some(v) = &b.version {
                println!("    before: {}-{}", v, b.rel.unwrap_or(1));
            }
        }
        if let Some(a) = &c.after {
            if let Some(v) = &a.version {
                println!("    after:  {}-{}", v, a.rel.unwrap_or(1));
            }
        }
        if !artifact_of_after(c).is_empty() {
            println!("    artifact: {}", artifact_of_after(c));
        }
        if !c.source.is_empty() {
            println!("    source:   {}", c.source);
        }
        if let Some(f) = &c.failure {
            println!("    failure:  {f}");
        }
        if let Some(h) = &c.hook {
            println!("    hook:     {h}");
        }
    }
    Ok(())
}

fn artifact_of_after(change: &PackageChange) -> &str {
    change.after.as_ref().map(|a| a.artifact_sha256.as_str()).unwrap_or("")
}
