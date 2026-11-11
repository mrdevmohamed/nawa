//! Plugin command handlers: management, capability policy and direct dispatch.

use anyhow::{Result};
use crate::ctx::Ctx;
use crate::color::Palette;
use std::path::PathBuf;
use crate::color::Stream;
use crate::config;
use crate::plugins;

use anyhow::bail;

fn effective_state(ctx: &Ctx) -> plugins::discovery::PluginState {
    plugins::discovery::PluginState::load_effective(
        ctx.cfg.file.as_deref(),
        ctx.cfg.system_file.as_deref(),
    )
}

fn protected_paths(ctx: &Ctx) -> Vec<PathBuf> {
    let mut paths = vec![ctx.root.clone()];
    for path in [ctx.cfg.file.as_deref(), ctx.cfg.system_file.as_deref()]
        .into_iter()
        .flatten()
    {
        if let Some(parent) = path.parent() {
            paths.push(parent.to_path_buf());
        }
    }
    paths
}

/// List every discovered plugin without executing it.
pub(crate) fn cmd_plugin_list(ctx: &Ctx) -> Result<()> {
    let state = effective_state(ctx);
    let found = plugins::discovery::discover(crate::diag::is_root())?;
    if found.is_empty() {
        crate::nawa_info!("no plugins discovered");
        return Ok(());
    }
    let pal = Palette::auto(Stream::Stdout);
    println!("{:<14}  {:<8}  {:<9}  {:<7}  Source", "ID", "Version", "State", "API");
    println!("{}", "-".repeat(64));
    for plugin in &found {
        let (id, version, api) = if plugin.error.is_some() {
            (plugin.manifest.id.clone(), "-".to_string(), "-".to_string())
        } else {
            (
                plugin.manifest.id.clone(),
                plugin.manifest.version.clone(),
                plugin.manifest.api_version.to_string(),
            )
        };
        let enabled = plugin.error.is_none() && state.is_enabled(&plugin.manifest.id);
        let shown_state = match &plugin.error {
            Some(_) => "invalid".to_string(),
            None if enabled => "enabled".to_string(),
            None => "disabled".to_string(),
        };
        println!(
            "{:<14}  {:<8}  {}  {:<7}  {}",
            pal.name(&id),
            pal.ver(&version),
            if shown_state == "enabled" { pal.ok(&shown_state) }
            else if shown_state == "invalid" { pal.warn(&shown_state) }
            else { pal.dim(&shown_state) },
            api,
            pal.dim(&plugin.dir.display().to_string()),
        );
    }
    Ok(())
}

/// Show a plugin manifest and the effective administrator grants.
pub(crate) fn cmd_plugin_info(ctx: &Ctx, id: &str) -> Result<()> {
    let state = effective_state(ctx);
    let found = plugins::discovery::discover(crate::diag::is_root())?
        .into_iter()
        .find(|plugin| plugin.manifest.id == id)
        .ok_or_else(|| anyhow::anyhow!("no plugin {id:?} discovered"))?;
    if let Some(error) = &found.error {
        bail!("plugin {id} is not usable: {error}");
    }
    let manifest = &found.manifest;
    println!("Plugin:      {} ({})", manifest.name, manifest.id);
    println!("Version:     {} (API {})", manifest.version, manifest.api_version);
    println!("Executable:  {}", manifest.executable.display());
    println!("Directory:   {}", found.dir.display());
    if !manifest.description.is_empty() {
        println!("Description: {}", manifest.description);
    }
    println!("State:       {}", if state.is_enabled(id) { "enabled" } else { "disabled" });
    println!("Commands:    {}", if manifest.commands.is_empty() { "(none)".into() } else { manifest.commands.join(", ") });
    println!("Events:      {}", if manifest.events.is_empty() { "(none)".into() } else { manifest.events.join(", ") });
    let requested: Vec<&str> = manifest.requested_capabilities.iter().map(|cap| cap.as_str()).collect();
    let granted = state.granted_capabilities(id, &manifest.requested_capabilities);
    let granted_names: Vec<&str> = granted.iter().map(|cap| cap.as_str()).collect();
    println!("Requested:   {}", if requested.is_empty() { "(none)".into() } else { requested.join(", ") });
    println!("Granted:     {}", if granted_names.is_empty() { "(none)".into() } else { granted_names.join(", ") });
    if !found.approved_for_root {
        println!("Note:        non-system plugin; root execution requires root-owned, non-writable paths");
    }
    Ok(())
}

/// Enable/disable is a project preference. A system-wide disable wins.
pub(crate) fn cmd_plugin_enable(ctx: &Ctx, id: &str, enable: bool) -> Result<()> {
    let found = plugins::discovery::discover(crate::diag::is_root())?
        .into_iter()
        .find(|plugin| plugin.manifest.id == id && plugin.error.is_none())
        .ok_or_else(|| anyhow::anyhow!("no usable plugin {id:?} discovered"))?;
    let _ = &found;
    let mut state = plugins::discovery::PluginState::load(ctx.cfg.file.as_deref());
    if enable {
        state.disabled.retain(|entry| entry != id);
        if !state.enabled.iter().any(|entry| entry == id) {
            state.enabled.push(id.to_string());
        }
    } else {
        state.enabled.retain(|entry| entry != id);
        if !state.disabled.iter().any(|entry| entry == id) {
            state.disabled.push(id.to_string());
        }
    }
    state.save(ctx.cfg.file.as_deref())?;
    crate::nawa_info!(
        "plugin {id} {} (recorded in {})",
        if enable { "enabled" } else { "disabled" },
        ctx.cfg.file.as_deref().map(|path| path.display().to_string())
            .unwrap_or_else(|| "(no project config)".into()),
    );
    Ok(())
}

/// Grant/revoke only on the root-owned system config. A capability must also
/// be declared in the plugin manifest; manifests cannot grant themselves.
pub(crate) fn cmd_plugin_grant(
    ctx: &Ctx,
    id: &str,
    capability_name: &str,
    grant: bool,
) -> Result<()> {
    if !crate::diag::is_root() {
        bail!("plugin capabilities are administrator policy; run this command as root");
    }
    let capability = plugins::manifest::Capability::parse(capability_name)
        .ok_or_else(|| anyhow::anyhow!("unknown capability {capability_name:?}"))?;
    if capability == plugins::manifest::Capability::NotificationSend {
        bail!("notification.send is reserved; API v1 has no notification broker to enforce it");
    }
    let plugin = plugins::discovery::discover(true)?
        .into_iter()
        .find(|plugin| plugin.manifest.id == id && plugin.error.is_none() && plugin.approved_for_root)
        .ok_or_else(|| anyhow::anyhow!("no usable system plugin {id:?} discovered"))?;
    if !plugin.manifest.requested_capabilities.contains(&capability) {
        bail!("plugin {id} did not request {}", capability.as_str());
    }

    let system_file = ctx.cfg.system_file.clone().unwrap_or_else(config::system_file);
    if !plugins::discovery::secure_admin_config(&system_file) {
        bail!(
            "system config {} is missing or insecure; create it with nawa init --system and keep it root-owned",
            system_file.display()
        );
    }
    let mut state = plugins::discovery::PluginState::load(Some(&system_file));
    if grant {
        state.grant(id, capability);
    } else {
        state.revoke(id, capability);
    }
    state.save(Some(&system_file))?;
    crate::nawa_info!(
        "{} {} for plugin {id}",
        if grant { "granted" } else { "revoked" },
        capability.as_str()
    );
    Ok(())
}

/// Explicit namespaced execution does not require command.register: the user
/// selected the plugin and command. The plugin still gets only granted APIs.
pub(crate) fn cmd_plugin_run(ctx: &Ctx, id: &str, command: &str, args: &[String]) -> Result<()> {
    let state = effective_state(ctx);
    let found = plugins::discovery::discover(crate::diag::is_root())?
        .into_iter()
        .find(|plugin| plugin.manifest.id == id)
        .ok_or_else(|| anyhow::anyhow!("no plugin {id:?} discovered"))?;
    if let Some(error) = &found.error {
        bail!("plugin {id} is not usable: {error}");
    }
    if !state.is_enabled(id) {
        bail!("plugin {id} is disabled; enable it with nawa plugin enable {id}");
    }
    if !found.manifest.commands.iter().any(|known| known == command) {
        bail!("plugin {id} does not register command {command:?}");
    }
    let granted = state.granted_capabilities(id, &found.manifest.requested_capabilities);
    let mut process = plugins::runner::PluginProcess::start_with_policy(
        &found.manifest,
        &granted,
        &protected_paths(ctx),
        plugins::runner::DEFAULT_TIMEOUT,
    )?;
    let result = process.run(command, args, plugins::runner::DEFAULT_TIMEOUT)?;
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}

/// Dispatch an unknown top-level command only when exactly one enabled plugin
/// registered it and the administrator granted command.register.
pub(crate) fn cmd_plugin_dispatch(ctx: &Ctx, command: &str, args: &[String]) -> Result<()> {
    let state = effective_state(ctx);
    let found = plugins::discovery::discover(crate::diag::is_root())?;
    let mut candidates: Vec<_> = found.into_iter().filter(|plugin| {
        plugin.error.is_none()
            && state.is_enabled(&plugin.manifest.id)
            && plugin.manifest.requested_capabilities.contains(
                &plugins::manifest::Capability::CommandRegister,
            )
            && state.has_grant(
                &plugin.manifest.id,
                plugins::manifest::Capability::CommandRegister,
            )
            && plugin.manifest.commands.iter().any(|name| name == command)
    }).collect();

    if candidates.is_empty() {
        bail!("unknown command {command:?}; no enabled plugin has an administrator command.register grant");
    }
    if candidates.len() > 1 {
        let ids = candidates.iter().map(|plugin| plugin.manifest.id.as_str()).collect::<Vec<_>>().join(", ");
        bail!("plugin command {command:?} is ambiguous; registered by: {ids}");
    }
    let plugin = candidates.remove(0);
    let granted = state.granted_capabilities(
        &plugin.manifest.id,
        &plugin.manifest.requested_capabilities,
    );
    let mut process = plugins::runner::PluginProcess::start_with_policy(
        &plugin.manifest,
        &granted,
        &protected_paths(ctx),
        plugins::runner::DEFAULT_TIMEOUT,
    )?;
    let result = process.run(command, args, plugins::runner::DEFAULT_TIMEOUT)?;
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}
