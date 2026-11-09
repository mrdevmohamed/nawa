//! Plugin discovery and enable/disable state (roadmap §6.3, §6.7).
//!
//! Locations, in deterministic order:
//!   1. `/usr/lib/nawa/plugins`        — distribution-managed
//!   2. `/usr/local/lib/nawa/plugins`  — locally administered
//!   3. `$NAWA_PLUGIN_PATH` entries    — testing / per-user override
//!
//! A root nawa only ever executes plugins from 1/2. Entries from 3 are
//! usable when unprivileged (or when the path equals an approved system dir),
//! never as root with a user-writable directory.
//!
//! Enable/disable state lives in the nawa configuration file:
//!
//! ```toml
//! [plugins]
//! enabled = ["notify"]
//! disabled = ["experimental-repo"]
//! ```

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::manifest::{PluginManifest, HOST_API_VERSION};

/// The manifest file name inside a plugin directory.
pub(crate) const MANIFEST_NAME: &str = "nawa-plugin.toml";

/// Built-in, system-wide discovery directories.
pub(crate) fn system_dirs() -> Vec<PathBuf> {
    vec![
        PathBuf::from("/usr/lib/nawa/plugins"),
        PathBuf::from("/usr/local/lib/nawa/plugins"),
    ]
}

/// Additional directories from `$NAWA_PLUGIN_PATH` (colon-separated).
pub(crate) fn env_dirs() -> Vec<PathBuf> {
    std::env::var("NAWA_PLUGIN_PATH")
        .ok()
        .map(|v| {
            v.split(':')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(PathBuf::from)
                .collect()
        })
        .unwrap_or_default()
}

/// All discovery directories, approved-for-root first, env dirs last.
pub(crate) fn all_dirs() -> Vec<PathBuf> {
    let mut v = system_dirs();
    for d in env_dirs() {
        if !v.contains(&d) {
            v.push(d);
        }
    }
    v
}

/// One discovered plugin.
#[derive(Debug, Clone)]
pub(crate) struct Discovered {
    pub manifest: PluginManifest,
    pub dir: PathBuf,
    /// true when the discovery directory is approved for privileged use.
    pub approved_for_root: bool,
    /// Why the plugin is not usable (recorded, not silently dropped).
    pub error: Option<String>,
}

/// Discover every plugin in every directory. Deterministic order: by
/// directory order, then by manifest path. A duplicate id across directories
/// is an error for whichever comes later in the order.
pub(crate) fn discover(root_user: bool) -> Result<Vec<Discovered>> {
    let mut out: Vec<Discovered> = Vec::new();
    let mut seen_ids: BTreeMap<String, PathBuf> = BTreeMap::new();
    let env = env_dirs();
    for dir in all_dirs() {
        let approved_for_root = system_dirs().contains(&dir);
        if root_user && !approved_for_root && !secure_admin_config(&dir) {
            // a root nawa must not execute a user-writable plugin just
            // because it is discoverable (roadmap §6.3)
            continue;
        }
        let rd = match std::fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(_) => continue,
        };
        let mut entries: Vec<PathBuf> = rd
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.join(MANIFEST_NAME).is_file())
            .collect();
        entries.sort();
        for entry in entries {
            let raw = match std::fs::read_to_string(entry.join(MANIFEST_NAME)) {
                Ok(r) => r,
                Err(e) => {
                    out.push(Discovered {
                        manifest: broken(entry.clone()),
                        dir: entry,
                        approved_for_root,
                        error: Some(format!("unreadable manifest: {e}")),
                    });
                    continue;
                }
            };
            match PluginManifest::parse(&raw, &entry) {
                Ok(m) => {
                    if let Some(first) = seen_ids.get(&m.id) {
                        out.push(Discovered {
                            manifest: m,
                            dir: entry,
                            approved_for_root,
                            error: Some(format!(
                                "duplicate plugin id: already discovered at {}",
                                first.display()
                            )),
                        });
                    } else {
                        seen_ids.insert(m.id.clone(), entry.clone());
                        out.push(Discovered {
                            manifest: m,
                            dir: entry,
                            approved_for_root,
                            error: None,
                        });
                    }
                }
                Err(e) => {
                    out.push(Discovered {
                        manifest: broken(entry.clone()),
                        dir: entry,
                        approved_for_root,
                        error: Some(format!("{e:#}")),
                    });
                }
            }
        }
    }
    let _ = &env;
    Ok(out)
}

fn broken(dir: PathBuf) -> PluginManifest {
    PluginManifest {
        id: dir
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default(),
        name: String::new(),
        version: String::new(),
        api_version: HOST_API_VERSION,
        executable: dir.join(MANIFEST_NAME),
        description: String::new(),
        commands: Vec::new(),
        requested_capabilities: Vec::new(),
        events: Vec::new(),
    }
}

/// Persisted enable/disable state (the `[plugins]` table of nawa.toml).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct PluginState {
    #[serde(default)]
    pub enabled: Vec<String>,
    #[serde(default)]
    pub disabled: Vec<String>,
    /// Administrator grants keyed by plugin id. Project files never grant.
    #[serde(default)]
    pub grants: BTreeMap<String, Vec<String>>,
}

impl PluginState {
    pub fn load(cfg_file: Option<&Path>) -> Self {
        let Some(path) = cfg_file else { return Self::default() };
        let Ok(raw) = std::fs::read_to_string(path) else { return Self::default() };
        Self::from_config_text(&raw)
    }

    /// Merge project enable/disable preferences with administrator policy.
    /// Only a root-owned, non-writable system config can supply grants.
    pub fn load_effective(
        project_file: Option<&Path>,
        system_file: Option<&Path>,
    ) -> Self {
        let project = Self::load(project_file);
        let system = if system_file.is_some_and(secure_admin_config) {
            Self::load(system_file)
        } else {
            Self::default()
        };
        // A non-empty system enabled list is an administrator allow-list.
        // Project preferences cannot add entries or replace that policy.
        let enabled = if system.enabled.is_empty() {
            project.enabled
        } else {
            system.enabled.clone()
        };
        let mut disabled = system.disabled.clone();
        for id in project.disabled {
            if !disabled.iter().any(|known| known == &id) {
                disabled.push(id);
            }
        }
        Self { enabled, disabled, grants: system.grants }
    }

    pub fn has_grant(&self, id: &str, capability: super::manifest::Capability) -> bool {
        // notification.send is reserved until API v1 has a real host-side
        // notification broker. A hand-edited TOML file cannot activate it.
        if capability == super::manifest::Capability::NotificationSend {
            return false;
        }
        self.grants.get(id)
            .is_some_and(|items| items.iter().any(|item| item == capability.as_str()))
    }

    pub fn granted_capabilities(
        &self,
        id: &str,
        requested: &[super::manifest::Capability],
    ) -> Vec<super::manifest::Capability> {
        requested.iter().copied().filter(|cap| self.has_grant(id, *cap)).collect()
    }

    pub fn grant(&mut self, id: &str, capability: super::manifest::Capability) {
        let items = self.grants.entry(id.to_string()).or_default();
        if !items.iter().any(|item| item == capability.as_str()) {
            items.push(capability.as_str().to_string());
            items.sort();
        }
    }

    pub fn revoke(&mut self, id: &str, capability: super::manifest::Capability) {
        if let Some(items) = self.grants.get_mut(id) {
            items.retain(|item| item != capability.as_str());
            if items.is_empty() {
                self.grants.remove(id);
            }
        }
    }

    /// Extract the `[plugins]` table from a full nawa.toml document. A file
    /// without one (or with a malformed one) yields the default state.
    pub fn from_config_text(raw: &str) -> Self {
        let doc: toml::Table = match toml::from_str(raw) {
            Ok(t) => t,
            Err(_) => return Self::default(),
        };
        doc.get("plugins")
            .and_then(|v| v.as_table())
            .and_then(|t| t.clone().try_into::<Self>().ok())
            .unwrap_or_default()
    }

    pub fn save(&self, cfg_file: Option<&Path>) -> Result<()> {
        let Some(path) = cfg_file else {
            anyhow::bail!("no configuration file to store plugin state in — create one with `nawa init`");
        };
        let existing = std::fs::read_to_string(path).unwrap_or_default();
        let updated = replace_plugins_table(&existing, self)
            .context("could not update the [plugins] table")?;
        std::fs::write(path, updated)
            .with_context(|| format!("write {}", path.display()))?;
        Ok(())
    }

    /// A plugin runs only when it is not disabled AND (it is listed in
    /// enabled OR no enable-list exists at all — off by default only when
    /// any plugin has been configured explicitly).
    pub fn is_enabled(&self, id: &str) -> bool {
        if self.disabled.iter().any(|d| d == id) {
            return false;
        }
        if self.enabled.is_empty() {
            return true; // nothing configured: every usable plugin is on
        }
        self.enabled.iter().any(|e| e == id)
    }
}

/// Administrator files and every parent must be root-owned, non-symlinked,
/// and not writable by group or other users.
pub(crate) fn secure_admin_config(path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let mut current = Some(path);
    while let Some(entry) = current {
        let Ok(meta) = std::fs::symlink_metadata(entry) else { return false };
        if meta.file_type().is_symlink() || meta.uid() != 0 || meta.mode() & 0o022 != 0 {
            return false;
        }
        current = entry.parent();
    }
    true
}

#[derive(Serialize)]
struct PluginsRoot<'a> {
    plugins: &'a PluginState,
}

/// Replace the plugin table and all nested plugin tables while preserving
/// unrelated top-level configuration lines.
fn replace_plugins_table(raw: &str, state: &PluginState) -> Result<String> {
    let table = toml::to_string(&PluginsRoot { plugins: state })?;
    let mut out = String::new();
    let mut in_plugins = false;
    let mut replaced = false;
    for line in raw.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with('[') {
            let is_plugin_table = trimmed == "[plugins]"
                || trimmed.starts_with("[plugins.")
                || trimmed.starts_with("[[plugins.");
            if is_plugin_table {
                in_plugins = true;
                continue;
            }
            if in_plugins {
                out.push_str(&table);
                out.push('\n');
                in_plugins = false;
                replaced = true;
            }
        }
        if !in_plugins {
            out.push_str(line);
            out.push('\n');
        }
    }
    if in_plugins {
        out.push_str(&table);
        out.push('\n');
        replaced = true;
    }
    if !replaced {
        if !out.trim_end().is_empty() {
            out.push('\n');
        }
        out.push_str(&table);
    }
    Ok(out.trim_end_matches('\n').to_string() + "\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        // Root discovery correctly rejects fixtures under world-writable
        // /tmp. Put root-run tests under /root so they exercise the privileged
        // path without weakening the production directory policy.
        let base = if crate::diag::is_root() {
            PathBuf::from("/root")
        } else {
            std::env::temp_dir()
        };
        let d = base.join(format!("nawa-plugin-disc-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn make_plugin(dir: &Path, id: &str, cmd: Option<&str>) -> PathBuf {
        let pdir = dir.join(id);
        std::fs::create_dir_all(&pdir).unwrap();
        let exe = pdir.join("nawa-plugin-x");
        std::fs::write(&exe, b"#!/bin/sh\nexit 0\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        let commands = match cmd {
            Some(c) => format!("\n[commands]\nnames = [\"{c}\"]\n"),
            None => String::new(),
        };
        std::fs::write(
            pdir.join(MANIFEST_NAME),
            format!(
                "[plugin]\nid = \"{id}\"\nname = \"T\"\nversion = \"1.0.0\"\napi_version = \"1\"\nexecutable = \"{}\"\ndescription = \"t\"{commands}",
                exe.display()
            ),
        )
        .unwrap();
        pdir
    }

    #[test]
    fn discovery_reports_valid_invalid_and_duplicates() {
        let base = tmpdir("discover");
        let d1 = base.join("dir1");
        let d2 = base.join("dir2");
        std::fs::create_dir_all(&d1).unwrap();
        std::fs::create_dir_all(&d2).unwrap();
        make_plugin(&d1, "alpha", Some("alpha-run"));
        make_plugin(&d2, "alpha", None); // duplicate id
        make_plugin(&d2, "beta", Some("beta-run"));
        // a broken manifest
        let broken_dir = d2.join("broken");
        std::fs::create_dir_all(&broken_dir).unwrap();
        std::fs::write(broken_dir.join(MANIFEST_NAME), "not toml [[").unwrap();

        std::env::set_var("NAWA_PLUGIN_PATH", format!("{}:{}", d1.display(), d2.display()));
        let found = discover(false).unwrap();
        std::env::remove_var("NAWA_PLUGIN_PATH");

        assert_eq!(found.len(), 4);
        let alpha_first = found.iter().find(|p| p.manifest.id == "alpha" && p.error.is_none());
        assert!(alpha_first.is_some(), "first alpha is usable");
        let alpha_dup = found.iter().find(|p| p.manifest.id == "alpha" && p.error.is_some());
        assert!(alpha_dup.is_some(), "second alpha is a duplicate error");
        let broken = found.iter().find(|p| p.dir.ends_with("broken"));
        assert!(broken.is_some() && broken.unwrap().error.is_some());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn enabled_state_logic() {
        let mut st = PluginState::default();
        assert!(st.is_enabled("x"), "nothing configured: everything on");
        st.disabled.push("x".to_string());
        assert!(!st.is_enabled("x"));
        st.enabled.push("y".to_string());
        assert!(!st.is_enabled("z"), "once an enable-list exists, unknown ids are off");
        assert!(st.is_enabled("y"));
        assert!(!st.is_enabled("x"), "disabled wins over enabled");
        st.grant("x", super::super::manifest::Capability::TransactionObserve);
        assert!(st.has_grant("x", super::super::manifest::Capability::TransactionObserve));
        st.revoke("x", super::super::manifest::Capability::TransactionObserve);
        assert!(!st.has_grant("x", super::super::manifest::Capability::TransactionObserve));
        st.grant("x", super::super::manifest::Capability::NotificationSend);
        assert!(
            !st.has_grant("x", super::super::manifest::Capability::NotificationSend),
            "reserved capabilities are never effective even if present in TOML"
        );
    }

    #[test]
    fn project_configuration_cannot_grant_capabilities() {
        let base = tmpdir("policy");
        let project = base.join("project.toml");
        let system = base.join("system.toml");
        std::fs::write(
            &project,
            "[plugins]\nenabled = [\"audit\"]\n\n[plugins.grants]\naudit = [\"command.register\"]\n",
        ).unwrap();
        // A temp directory is not an administrator-owned path. This remains
        // true for root and unprivileged CI jobs alike.
        std::fs::write(
            &system,
            "[plugins]\nenabled = [\"audit\"]\n\n[plugins.grants]\naudit = [\"transaction.observe\"]\n",
        ).unwrap();
        // Even if a root-run test creates this file under /root with uid 0,
        // group/world-writable mode makes it ineligible as administrator
        // policy. This keeps the assertion independent of CI privilege.
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&system, std::fs::Permissions::from_mode(0o666)).unwrap();

        let effective = PluginState::load_effective(Some(&project), Some(&system));
        assert!(effective.is_enabled("audit"));
        assert!(!effective.has_grant("audit", super::super::manifest::Capability::CommandRegister));
        assert!(!effective.has_grant("audit", super::super::manifest::Capability::TransactionObserve));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn plugins_table_replacement_preserves_the_rest() {
        let raw = "jobs = 4\n\n[plugins]\nenabled = [\"old\"]\n\n[other]\nkey = 1\n";
        let mut st = PluginState { enabled: vec!["a".into()], disabled: vec![], grants: BTreeMap::new() };
        st.grant("audit", super::super::manifest::Capability::TransactionObserve);
        let out = replace_plugins_table(raw, &st).unwrap();
        assert!(out.contains("jobs = 4"));
        assert!(out.contains("[other]"));
        assert!(out.contains("enabled = [\"a\"]"));
        assert!(!out.contains("\"old\""));
        // append when absent
        let out = replace_plugins_table("jobs = 4\n", &st).unwrap();
        assert!(out.contains("[plugins]"));
        assert!(out.starts_with("jobs = 4"));
        // round trip through the [plugins]-table extractor
        let parsed = PluginState::from_config_text(&out);
        assert_eq!(parsed.enabled, vec!["a".to_string()]);
        assert!(parsed.disabled.is_empty());
        assert!(parsed.has_grant("audit", super::super::manifest::Capability::TransactionObserve));
        // a config without [plugins] yields the default state
        let parsed = PluginState::from_config_text("jobs = 4\n");
        assert!(parsed.enabled.is_empty());
    }
}
