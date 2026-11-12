//! `nawa sandbox-probe`: report which sandbox features this kernel offers.

use anyhow::{Context, Result};

use crate::sandbox;

pub(crate) fn cmd_sandbox_probe(op: Option<String>) -> Result<()> {
    // a single probe runs in this process; the full mode spawns one
    // process per op, because each probe creates namespaces and
    // nesting them in one process corrupts the results
    match op {
        Some(o) => {
            let ok = match sandbox::probe(&o) {
                Ok(true) => {
                    println!("PASS  {o}");
                    true
                }
                Ok(false) => {
                    println!("FAIL  {o}");
                    false
                }
                Err(e) => {
                    println!("ERROR {o}: {e:#}");
                    false
                }
            };
            std::process::exit(if ok { 0 } else { 1 });
        }
        None => {
            let exe = std::env::current_exe().context("locate the nawa binary")?;
            let mut all_ok = true;
            for o in sandbox::PROBES {
                let out = std::process::Command::new(&exe)
                    .arg("sandbox-probe")
                    .arg(o)
                    .output()
                    .with_context(|| format!("run probe {o}"))?;
                let text = String::from_utf8_lossy(&out.stdout);
                let one_line = text
                    .lines()
                    .find(|l| l.starts_with("PASS") || l.starts_with("FAIL"))
                    .unwrap_or_default();
                println!("{one_line}");
                if !out.status.success() {
                    all_ok = false;
                }
            }
            if all_ok {
                println!("sandbox probe: OK — all features available");
            } else {
                println!("sandbox probe: some features are missing (restricted kernel/container?)");
                println!(
                    "nawa build refuses to weaken isolation silently; pass \
                     --allow-degraded to continue"
                );
            }
            std::process::exit(if all_ok { 0 } else { 1 });
        }
    }

}
