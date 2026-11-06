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
pub const MANIFEST_NAME: &str = "nawa-plugin.toml";

/// Built-in, system-wide discovery directories.
pub fn system_dirs() -> Vec<PathBuf> {
    vec![
        PathBuf::from("/usr/lib/nawa/plugins"),
        PathBuf::from("/usr/local/lib/nawa/plugins"),
    ]
}

/// Additional directories from `$NAWA_PLUGIN_PATH` (colon-separated).
pub fn env_dirs() -> Vec<PathBuf> {
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
pub fn all_dirs() -> Vec<PathBuf> {
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
pub struct Discovered {
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
pub fn discover(root_user: bool) -> Result<Vec<Discovered>> {
    let mut out: Vec<Discovered> = Vec::new();
    let mut seen_ids: BTreeMap<String, PathBuf> = BTreeMap::new();
    let env = env_dirs();
    for dir in all_dirs() {
        let approved_for_root = system_dirs().contains(&dir);
        if root_user && !approved_for_root && !dir_is_root_safe(&dir) {
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
    }
}

/// A user-provided directory is only usable by root when it (and its
/// parents) are root-owned and not group/world-writable — the same rule the
/// shell applies to PATH lookups of privileged binaries.
fn dir_is_root_safe(dir: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let mut cur = Some(dir);
    while let Some(d) = cur {
        if let Ok(md) = std::fs::symlink_metadata(d) {
            if md.file_type().is_symlink() {
                return false;
            }
            if md.uid() != 0 || md.mode() & 0o022 != 0 {
                return false;
            }
        } else {
            return false;
        }
        cur = d.parent();
    }
    true
}

/// Persisted enable/disable state (the `[plugins]` table of nawa.toml).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PluginState {
    #[serde(default)]
    pub enabled: Vec<String>,
    #[serde(default)]
    pub disabled: Vec<String>,
}

impl PluginState {
    pub fn load(cfg_file: Option<&Path>) -> Self {
        let Some(path) = cfg_file else { return Self::default() };
        let Ok(raw) = std::fs::read_to_string(path) else { return Self::default() };
        Self::from_config_text(&raw)
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

/// Replace or append the `[plugins]` table in a TOML document, keeping every
/// other line byte-for-byte identical.
fn replace_plugins_table(raw: &str, state: &PluginState) -> Result<String> {
    let body = toml::to_string_pretty(state)?;
    let table = format!("[plugins]\n{body}");
    let mut out = String::new();
    let mut in_plugins = false;
    let mut replaced = false;
    for line in raw.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with('[') {
            if in_plugins {
                in_plugins = false;
                if !replaced {
                    out.push_str(&table);
                    out.push('\n');
                    replaced = true;
                }
            }
            if trimmed == "[plugins]" {
                in_plugins = true;
                continue; // drop the old table header and body
            }
        }
        if !in_plugins {
            out.push_str(line);
            out.push('\n');
        }
    }
    if in_plugins && !replaced {
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
        let d = std::env::temp_dir().join(format!("nawa-plugin-disc-{}-{tag}", std::process::id()));
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
    }

    #[test]
    fn plugins_table_replacement_preserves_the_rest() {
        let raw = "jobs = 4\n\n[plugins]\nenabled = [\"old\"]\n\n[other]\nkey = 1\n";
        let st = PluginState { enabled: vec!["a".into()], disabled: vec![] };
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
        // a config without [plugins] yields the default state
        let parsed = PluginState::from_config_text("jobs = 4\n");
        assert!(parsed.enabled.is_empty());
    }
}
