//! Plugin manifests (roadmap §6.4).
//!
//! ```toml
//! [plugin]
//! id = "notify"
//! name = "Nawa notification plugin"
//! version = "1.2.0"
//! api_version = "1"
//! executable = "/usr/lib/nawa/plugins/nawa-plugin-notify"
//! description = "Send desktop notifications after package operations"
//!
//! [commands]
//! names = ["notify-test"]
//!
//! [capabilities]
//! requested = ["history.read", "transaction.observe"]
//! ```
//!
//! Unknown fields are REJECTED (deny_unknown_fields): a manifest that this
//! host cannot fully understand must fail validation, not half-work. The
//! manifest never grants anything by listing it — requested capabilities are
//! a wish, not a permission.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Named, narrow permissions a plugin may request (roadmap §6.8). Broad
/// capabilities (root, filesystem.write, package.install) do not exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Capability {
    #[serde(rename = "history.read")]
    HistoryRead,
    #[serde(rename = "repository.metadata.read")]
    RepositoryMetadataRead,
    #[serde(rename = "transaction.observe")]
    TransactionObserve,
    #[serde(rename = "command.register")]
    CommandRegister,
    #[serde(rename = "notification.send")]
    NotificationSend,
    #[serde(rename = "package.plan.inspect")]
    PackagePlanInspect,
}

impl Capability {
    pub fn as_str(&self) -> &'static str {
        match self {
            Capability::HistoryRead => "history.read",
            Capability::RepositoryMetadataRead => "repository.metadata.read",
            Capability::TransactionObserve => "transaction.observe",
            Capability::CommandRegister => "command.register",
            Capability::NotificationSend => "notification.send",
            Capability::PackagePlanInspect => "package.plan.inspect",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "history.read" => Self::HistoryRead,
            "repository.metadata.read" => Self::RepositoryMetadataRead,
            "transaction.observe" => Self::TransactionObserve,
            "command.register" => Self::CommandRegister,
            "notification.send" => Self::NotificationSend,
            "package.plan.inspect" => Self::PackagePlanInspect,
            _ => return None,
        })
    }
}

/// Event names supported by plugin API v1.
pub const SUPPORTED_EVENTS: &[&str] = &[
    "transaction.completed",
    "transaction.failed",
    "transaction.partial",
    "package.installed",
    "package.upgraded",
    "package.removed",
];

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawManifest {
    plugin: RawPlugin,
    #[serde(default)]
    commands: RawCommands,
    #[serde(default)]
    capabilities: RawCapabilities,
    #[serde(default)]
    events: RawEvents,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPlugin {
    id: String,
    name: String,
    version: String,
    api_version: String,
    executable: String,
    #[serde(default)]
    description: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCommands {
    #[serde(default)]
    names: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCapabilities {
    #[serde(default)]
    requested: Vec<Capability>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawEvents {
    #[serde(default)]
    subscribe: Vec<String>,
}

/// A validated plugin manifest.
#[derive(Debug, Clone)]
pub struct PluginManifest {
    pub id: String,
    pub name: String,
    pub version: String,
    pub api_version: u32,
    pub executable: PathBuf,
    pub description: String,
    pub commands: Vec<String>,
    pub requested_capabilities: Vec<Capability>,
    /// Events requested by this plugin; delivery also requires an admin grant.
    pub events: Vec<String>,
}

/// The plugin API version this host implements (roadmap §6.12). Independent
/// of the Nawa application version.
pub const HOST_API_VERSION: u32 = 1;

impl PluginManifest {
    /// Parse and fully validate a manifest. `approved_dir` is the discovery
    /// directory the manifest came from: a system plugin's executable must
    /// resolve inside its approved directory (symlinks out are refused) and
    /// must not be writable by unprivileged users.
    pub fn parse(raw: &str, approved_dir: &Path) -> Result<Self> {
        let m: RawManifest =
            toml::from_str(raw).context("manifest is not valid TOML or uses unknown fields")?;
        Self::validate_raw(m, approved_dir)
    }

    fn validate_raw(m: RawManifest, approved_dir: &Path) -> Result<Self> {
        let p = m.plugin;
        if !crate::db::valid_name(&p.id) {
            bail!(
                "plugin id {:?} is invalid (allowed: letters, digits and + _ . -)",
                p.id
            );
        }
        if p.name.trim().is_empty() {
            bail!("plugin {} has an empty name", p.id);
        }
        if !valid_semver_ish(&p.version) {
            bail!("plugin {} has a non-numeric version {:?}", p.id, p.version);
        }
        let api_version: u32 = p
            .api_version
            .parse()
            .with_context(|| format!("plugin {} has a non-numeric api_version", p.id))?;
        if api_version == 0 {
            bail!("plugin {} has api_version 0", p.id);
        }
        if api_version != HOST_API_VERSION {
            bail!(
                "plugin {} implements API version {api_version}; this nawa supports {HOST_API_VERSION} — refusing to execute it",
                p.id
            );
        }
        // A privileged host trusts neither the plugin directory nor the
        // executable unless every lexical parent is root-owned, non-symlinked,
        // and not writable by group/other users. Checking only the file mode
        // would allow replacement through a writable parent directory.
        if running_as_root() && !plugin_directory_is_root_safe(approved_dir) {
            bail!(
                "plugin {} directory {} is not root-owned and protected from unprivileged writes",
                p.id,
                approved_dir.display()
            );
        }

        // executable: absolute, a regular file, inside the approved dir
        let exe = PathBuf::from(&p.executable);
        if !exe.is_absolute() {
            bail!(
                "plugin {} has a relative executable path {:?}",
                p.id,
                p.executable
            );
        }
        let meta = std::fs::symlink_metadata(&exe)
            .with_context(|| format!("plugin {} executable {} is missing", p.id, exe.display()))?;
        if meta.file_type().is_symlink() {
            bail!(
                "plugin {} executable {} is a symlink — refusing (an approved plugin \
                 directory cannot point outside itself)",
                p.id,
                exe.display()
            );
        }
        if !meta.is_file() {
            bail!("plugin {} executable {} is not a regular file", p.id, exe.display());
        }
        // canonicalize both and require containment
        let real_exe = std::fs::canonicalize(&exe)
            .with_context(|| format!("resolve plugin executable {}", exe.display()))?;
        let real_dir = std::fs::canonicalize(approved_dir)
            .with_context(|| format!("resolve plugin directory {}", approved_dir.display()))?;
        if !real_exe.starts_with(&real_dir) {
            bail!(
                "plugin {} executable {} escapes its approved directory {}",
                p.id,
                exe.display(),
                approved_dir.display()
            );
        }
        // unprivileged-writable check (roadmap §6.4): a plugin approved for
        // privileged use must not be modifiable by non-root users
        if running_as_root() {
            use std::os::unix::fs::MetadataExt;
            let mode = meta.mode();
            if mode & 0o022 != 0 {
                bail!(
                    "plugin {} executable {} is group- or world-writable (mode {:o}) — refusing",
                    p.id,
                    exe.display(),
                    mode & 0o777
                );
            }
        }
        let mut commands = m.commands.names;
        commands.sort();
        if let Some(duplicate) = commands.windows(2).find(|pair| pair[0] == pair[1]) {
            bail!("plugin {} declares duplicate command name {:?}", p.id, duplicate[0]);
        }
        for c in &commands {
            if !crate::db::valid_name(c) {
                bail!("plugin {} registers an invalid command name {c:?}", p.id);
            }
            if crate::cli::RESERVED_COMMANDS.contains(&c.as_str()) {
                bail!(
                    "plugin command {c:?} shadows a built-in nawa command — built-ins always win"
                );
            }
        }
        let mut caps = m.capabilities.requested;
        caps.sort();
        caps.dedup();

        let mut events = m.events.subscribe;
        events.sort();
        events.dedup();
        for event in &events {
            if !SUPPORTED_EVENTS.contains(&event.as_str()) {
                bail!("plugin {} subscribes to unsupported event {event:?}", p.id);
            }
        }
        if !events.is_empty() && !caps.contains(&Capability::TransactionObserve) {
            bail!("plugin {} subscribes to events but did not request transaction.observe", p.id);
        }

        Ok(PluginManifest {
            id: p.id,
            name: p.name,
            version: p.version,
            api_version,
            executable: real_exe,
            description: p.description,
            commands,
            requested_capabilities: caps,
            events,
        })
    }
}

/// Conservative "looks like a semver": MAJOR[.MINOR[.PATCH]][-suffix].
fn valid_semver_ish(v: &str) -> bool {
    let core = v.split('-').next().unwrap_or("");
    let mut parts = core.split('.');
    let first = match parts.next() {
        Some(p) if !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) => p,
        _ => return false,
    };
    if first.len() > 1 && first.starts_with('0') {
        return false;
    }
    for p in parts {
        if p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()) {
            return false;
        }
    }
    !v.trim().is_empty() && v.len() <= 32
}

fn plugin_directory_is_root_safe(dir: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let start = if dir.is_absolute() {
        dir.to_path_buf()
    } else {
        let Ok(cwd) = std::env::current_dir() else { return false };
        cwd.join(dir)
    };
    for ancestor in start.ancestors() {
        let Ok(meta) = std::fs::symlink_metadata(ancestor) else { return false };
        if meta.file_type().is_symlink() || meta.uid() != 0 || meta.mode() & 0o022 != 0 {
            return false;
        }
    }
    true
}

fn running_as_root() -> bool {
    rustix::process::geteuid().is_root()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(tag: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let parent = if running_as_root() {
            PathBuf::from("/root")
        } else {
            std::env::temp_dir()
        };
        let base = parent.join(format!("nawa-plugin-manifest-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let plugins = base.join("plugins");
        std::fs::create_dir_all(&plugins).unwrap();
        (base, plugins)
    }

    fn write_exe(plugins: &Path, name: &str) -> std::path::PathBuf {
        let exe = plugins.join(name);
        std::fs::write(&exe, b"#!/bin/sh\nexit 0\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        exe
    }

    fn manifest(plugins: &Path, id: &str) -> String {
        let exe = write_exe(plugins, "nawa-plugin-demo");
        format!(
            r#"[plugin]
id = "{id}"
name = "Demo Plugin"
version = "1.2.0"
api_version = "1"
executable = "{}"
description = "demo"

[commands]
names = ["demo-test"]

[capabilities]
requested = ["history.read", "transaction.observe"]

[events]
subscribe = ["transaction.completed"]
"#,
            exe.display()
        )
    }

    #[test]
    fn valid_manifest_parses_and_normalizes() {
        let (_base, plugins) = dir("ok");
        let m = PluginManifest::parse(&manifest(&plugins, "demo"), &plugins).unwrap();
        assert_eq!(m.id, "demo");
        assert_eq!(m.api_version, 1);
        assert_eq!(m.commands, vec!["demo-test".to_string()]);
        assert_eq!(m.requested_capabilities.len(), 2);
        assert!(m.requested_capabilities.contains(&Capability::HistoryRead));
        assert_eq!(m.events, vec!["transaction.completed".to_string()]);
        let _ = std::fs::remove_dir_all(&_base);
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let (_base, plugins) = dir("unknown");
        let raw = manifest(&plugins, "demo").replace("[commands]", "[extra]\nfoo = 1\n\n[commands]");
        assert!(PluginManifest::parse(&raw, &plugins).is_err());
        let raw = manifest(&plugins, "demo").replace("description = \"demo\"", "surprise = true");
        assert!(PluginManifest::parse(&raw, &plugins).is_err());
        let _ = std::fs::remove_dir_all(&_base);
    }

    #[test]
    fn event_subscriptions_require_transaction_observe_and_known_events() {
        let (_base, plugins) = dir("events");
        let raw = manifest(&plugins, "demo")
            .replace("requested = [\"history.read\", \"transaction.observe\"]", "requested = [\"history.read\"]");
        let error = PluginManifest::parse(&raw, &plugins).unwrap_err();
        assert!(format!("{error:#}").contains("transaction.observe"), "{error:#}");

        let raw = manifest(&plugins, "demo")
            .replace("transaction.completed", "transaction.unknown");
        let error = PluginManifest::parse(&raw, &plugins).unwrap_err();
        assert!(format!("{error:#}").contains("unsupported event"), "{error:#}");
        let _ = std::fs::remove_dir_all(&_base);
    }

    #[test]
    fn incompatible_api_version_is_refused() {
        let (_base, plugins) = dir("api");
        let raw = manifest(&plugins, "demo").replace("api_version = \"1\"", "api_version = \"2\"");
        let err = PluginManifest::parse(&raw, &plugins).unwrap_err();
        assert!(format!("{err:#}").contains("API version"), "{err:#}");
        let _ = std::fs::remove_dir_all(&_base);
    }

    #[test]
    fn symlinks_and_escapes_are_refused() {
        let (base, plugins) = dir("escape");
        let outside = base.join("outside.sh");
        std::fs::write(&outside, b"#!/bin/sh\n").unwrap();
        let link = plugins.join("nawa-plugin-link");
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        let raw = manifest(&plugins, "demo").replace(
            &format!("{}", plugins.join("nawa-plugin-demo").display()),
            &format!("{}", link.display()),
        );
        let err = PluginManifest::parse(&raw, &plugins).unwrap_err();
        assert!(format!("{err:#}").contains("symlink"), "{err:#}");

        // an executable outside the approved directory is refused
        let raw = manifest(&plugins, "demo").replace(
            &format!("{}", plugins.join("nawa-plugin-demo").display()),
            &format!("{}", outside.display()),
        );
        assert!(PluginManifest::parse(&raw, &plugins).is_err());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn duplicate_command_names_are_rejected() {
        let (_base, plugins) = dir("duplicate-command");
        let raw = manifest(&plugins, "demo")
            .replace("names = [\"demo-test\"]", "names = [\"demo-test\", \"demo-test\"]");
        let error = PluginManifest::parse(&raw, &plugins).unwrap_err();
        assert!(format!("{error:#}").contains("duplicate command name"), "{error:#}");
        let _ = std::fs::remove_dir_all(&_base);
    }

    #[test]
    fn builtin_command_shadowing_is_refused() {
        let (_base, plugins) = dir("shadow");
        let raw = manifest(&plugins, "demo").replace("names = [\"demo-test\"]", "names = [\"install\"]");
        let err = PluginManifest::parse(&raw, &plugins).unwrap_err();
        assert!(format!("{err:#}").contains("built-in"), "{err:#}");
        let _ = std::fs::remove_dir_all(&_base);
    }

    #[test]
    fn bad_ids_and_versions_are_rejected() {
        let (_base, plugins) = dir("bad");
        assert!(PluginManifest::parse(&manifest(&plugins, "../evil"), &plugins).is_err());
        assert!(PluginManifest::parse(&manifest(&plugins, "a b"), &plugins).is_err());
        let raw = manifest(&plugins, "demo").replace("version = \"1.2.0\"", "version = ");
        assert!(PluginManifest::parse(&raw, &plugins).is_err());
        let raw = manifest(&plugins, "demo").replace("version = \"1.2.0\"", "version = \"one\"");
        assert!(PluginManifest::parse(&raw, &plugins).is_err());
        let _ = std::fs::remove_dir_all(&_base);
    }

    #[test]
    fn writable_plugin_directory_is_refused_when_root() {
        let (base, plugins) = dir("writable-dir");
        let raw = manifest(&plugins, "demo");
        if running_as_root() {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&plugins, std::fs::Permissions::from_mode(0o777)).unwrap();
            let error = PluginManifest::parse(&raw, &plugins).unwrap_err();
            assert!(format!("{error:#}").contains("directory"), "{error:#}");
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn world_writable_executable_is_refused_when_root() {
        let (base, plugins) = dir("ww");
        let raw = manifest(&plugins, "demo");
        if running_as_root() {
            use std::os::unix::fs::PermissionsExt;
            let exe = plugins.join("nawa-plugin-demo");
            std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o777)).unwrap();
            let err = PluginManifest::parse(&raw, &plugins).unwrap_err();
            assert!(format!("{err:#}").contains("world-writable"), "{err:#}");
        }
        let _ = std::fs::remove_dir_all(&base);
    }
}
