//! `nawa plugin` command handlers (roadmap §6.10).

use crate::*;
use anyhow::bail;

/// `nawa plugin list`: every discovered plugin and its state. Discovery
/// never executes anything.
pub(crate) fn cmd_plugin_list(ctx: &Ctx) -> Result<()> {
    let state = plugins::discovery::PluginState::load(ctx.cfg.file.as_deref());
    let found = plugins::discovery::discover(running_as_root())?;
    if found.is_empty() {
        println!("nawa: no plugins discovered");
        return Ok(());
    }
    let pal = Palette::auto(Stream::Stdout);
    println!("{:<14}  {:<8}  {:<9}  {:<7}  Source", "ID", "Version", "State", "API");
    println!("{}", "-".repeat(64));
    for p in &found {
        let (id, ver, api) = if p.error.is_some() {
            (p.manifest.id.clone(), "-".to_string(), "-".to_string())
        } else {
            (
                p.manifest.id.clone(),
                p.manifest.version.clone(),
                p.manifest.api_version.to_string(),
            )
        };
        let enabled = p.error.is_none() && state.is_enabled(&p.manifest.id);
        let shown_state = match &p.error {
            Some(_) => "invalid".to_string(),
            None if enabled => "enabled".to_string(),
            None => "disabled".to_string(),
        };
        println!(
            "{:<14}  {:<8}  {}  {:<7}  {}",
            pal.name(&id),
            pal.ver(&ver),
            if shown_state == "enabled" {
                pal.ok(&shown_state)
            } else if shown_state == "invalid" {
                pal.warn(&shown_state)
            } else {
                pal.dim(&shown_state)
            },
            api,
            pal.dim(&p.dir.display().to_string()),
        );
    }
    Ok(())
}

/// `nawa plugin info <ID>`
pub(crate) fn cmd_plugin_info(ctx: &Ctx, id: &str) -> Result<()> {
    let state = plugins::discovery::PluginState::load(ctx.cfg.file.as_deref());
    let found = plugins::discovery::discover(running_as_root())?
        .into_iter()
        .find(|p| p.manifest.id == id)
        .ok_or_else(|| anyhow::anyhow!("no plugin {id:?} discovered"))?;
    if let Some(err) = &found.error {
        bail!("plugin {id} is not usable: {err}");
    }
    let m = &found.manifest;
    println!("Plugin:      {} ({})", m.name, m.id);
    println!("Version:     {} (API {})", m.version, m.api_version);
    println!("Executable:  {}", m.executable.display());
    println!("Directory:   {}", found.dir.display());
    if !m.description.is_empty() {
        println!("Description: {}", m.description);
    }
    println!(
        "State:       {}",
        if state.is_enabled(&m.id) { "enabled" } else { "disabled" }
    );
    if m.commands.is_empty() {
        println!("Commands:    (none)");
    } else {
        println!("Commands:    {}", m.commands.join(", "));
    }
    if m.requested_capabilities.is_empty() {
        println!("Capabilities: (none requested)");
    } else {
        let caps: Vec<&str> = m.requested_capabilities.iter().map(|c| c.as_str()).collect();
        println!("Capabilities: {} (requested; not granted by the manifest)", caps.join(", "));
    }
    if !found.approved_for_root {
        println!("Note:        discovered in a non-system directory; never executed by a privileged nawa");
    }
    Ok(())
}

/// `nawa plugin enable/disable <ID>`: persist into the config's [plugins].
pub(crate) fn cmd_plugin_enable(ctx: &Ctx, id: &str, enable: bool) -> Result<()> {
    let found = plugins::discovery::discover(running_as_root())?
        .into_iter()
        .find(|p| p.manifest.id == id && p.error.is_none())
        .ok_or_else(|| anyhow::anyhow!("no usable plugin {id:?} discovered"))?;
    let _ = &found;
    let mut state = plugins::discovery::PluginState::load(ctx.cfg.file.as_deref());
    if enable {
        state.disabled.retain(|d| d != id);
        if !state.enabled.iter().any(|e| e == id) {
            state.enabled.push(id.to_string());
        }
    } else {
        state.enabled.retain(|e| e != id);
        if !state.disabled.iter().any(|d| d == id) {
            state.disabled.push(id.to_string());
        }
    }
    state.save(ctx.cfg.file.as_deref())?;
    println!(
        "nawa: plugin {id} {} (recorded in {})",
        if enable { "enabled" } else { "disabled" },
        ctx.cfg.file.as_deref().map(|p| p.display().to_string()).unwrap_or_else(|| "(no config)".into()),
    );
    Ok(())
}

/// `nawa plugin run <ID> <COMMAND> [ARGS...]`: one-shot JSONL execution with
/// a bounded timeout. Arguments are passed as an argv array — never
/// concatenated into a shell line.
pub(crate) fn cmd_plugin_run(ctx: &Ctx, id: &str, command: &str, args: &[String]) -> Result<()> {
    let state = plugins::discovery::PluginState::load(ctx.cfg.file.as_deref());
    let found = plugins::discovery::discover(running_as_root())?
        .into_iter()
        .find(|p| p.manifest.id == id)
        .ok_or_else(|| anyhow::anyhow!("no plugin {id:?} discovered"))?;
    if let Some(err) = &found.error {
        bail!("plugin {id} is not usable: {err}");
    }
    if !state.is_enabled(id) {
        bail!("plugin {id} is disabled — enable it with `nawa plugin enable {id}`");
    }
    if !found.manifest.commands.iter().any(|c| c == command) {
        bail!(
            "plugin {id} does not register command {command:?} (registered: {})",
            if found.manifest.commands.is_empty() {
                "(none)".to_string()
            } else {
                found.manifest.commands.join(", ")
            }
        );
    }
    if running_as_root() && !found.approved_for_root {
        bail!(
            "refusing to run plugin {id} as root: it was discovered in a non-system \
             directory ({})",
            found.dir.display()
        );
    }
    let mut process = plugins::runner::PluginProcess::start(
        &found.manifest,
        plugins::runner::DEFAULT_TIMEOUT,
    )?;
    let result = process.run(command, args, plugins::runner::DEFAULT_TIMEOUT)?;
    // the plugin's result is data, not instructions: print it as JSON
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}

fn running_as_root() -> bool {
    rustix::process::geteuid().is_root()
}
