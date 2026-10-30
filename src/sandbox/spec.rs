//! The sandbox spec: a TOML file describing root mode, build paths, the
//! environment whitelist, the script and the degradation policy. This module
//! is pure data + validation — it never touches the kernel, so all of it is
//! unit-testable without namespaces.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// Everything the sandbox needs, serialised as TOML next to the build logs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxSpec {
    pub version: u32,
    #[serde(default)]
    pub root: RootSpec,
    pub build: BuildPaths,
    /// The exact environment the build section receives (§6 whitelist).
    #[serde(default)]
    pub env: std::collections::BTreeMap<String, String>,
    /// The (already expanded) script, run as `bash -e -c <script>`.
    pub script: String,
    /// Per-section wall-clock limit in seconds (0 = none).
    #[serde(default)]
    pub timeout_secs: u64,
    /// Escape hatch (§4.4): only effective when the user passed
    /// `--allow-network`; skips the network namespace entirely.
    #[serde(default)]
    pub network: bool,
    /// Mount /sys read-only instead of hiding it.
    #[serde(default)]
    pub sysfs: bool,
    /// Continue (with a loud warning) when the host kernel refuses an
    /// isolation feature instead of failing the build (§9).
    #[serde(default)]
    pub allow_degraded: bool,
    /// Host paths to hide behind an empty tmpfs in `live` mode: the build
    /// directory, the repo copy, the bootstrap seed, package caches…
    #[serde(default)]
    pub mask: Vec<String>,
    /// Where `/build` lives *inside* the sandbox. The design pins `/build`
    /// (it always exists in the stage chroots); an unprivileged host that
    /// cannot create `/build` uses a base on the fresh `/tmp` tmpfs instead
    /// (groundwork for rootless builds, §10 Phase 7). Recipes see it
    /// through `%{srcdir}` / `%{pkgdir}`, never as a hardcoded path.
    #[serde(default = "default_build_inside")]
    pub build_inside: String,
    /// Working directory of the build sections *inside* the sandbox: the
    /// unpacked source dir (e.g. `/build/src/m4-1.4.19`). Empty = `<build_inside>/src`.
    #[serde(default)]
    pub workdir: String,
}

fn default_build_inside() -> String {
    "/build".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RootSpec {
    /// `live` = the running system (sandbox level),
    /// `dir` = an assembled build root (closure level).
    #[serde(default = "default_root_mode")]
    pub mode: String,
    /// Host path of the assembled build root when `mode = "dir"`.
    #[serde(default)]
    pub dir: String,
    /// Phase 5 (§5.3 v2): host paths of extracted package layers, first
    /// entry bottom-most. When set, `dir` is mounted as an **overlayfs**
    /// (lowerdir = these layers, throw-away upper/work under the sandbox
    /// scratch) instead of being bound — assembly cost drops from
    /// "extract every archive" to "mount what is already on disk".
    /// Requires overlayfs inside a user namespace (kernel ≥ 5.11); the
    /// builder only fills this after `nawa sandbox-probe overlay` passed.
    #[serde(default)]
    pub layers: Vec<String>,
}

impl Default for RootSpec {
    fn default() -> Self {
        RootSpec { mode: default_root_mode(), dir: String::new(), layers: Vec::new() }
    }
}

fn default_root_mode() -> String {
    "live".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildPaths {
    /// Host path of the source staging dir (bound at /build/src).
    pub src_host: String,
    /// Host path of the DESTDIR staging dir (bound at /build/pkg).
    pub pkg_host: String,
    /// Host path of an empty dir (bound at /build/home).
    pub home_host: String,
    /// Host dir for the sandbox's own scratch (newroot, home, …).
    pub scratch: String,
}

impl SandboxSpec {
    pub fn parse(text: &str) -> Result<Self> {
        let spec: SandboxSpec = toml::from_str(text).context("bad sandbox spec")?;
        if spec.version != 1 {
            bail!("unsupported sandbox spec version {}", spec.version);
        }
        match spec.root.mode.as_str() {
            "live" => {}
            "dir" => {
                if spec.root.dir.trim().is_empty() {
                    bail!("sandbox spec root.mode = \"dir\" needs root.dir");
                }
                for l in &spec.root.layers {
                    if l.trim().is_empty() {
                        bail!("sandbox spec root.layers entries must be non-empty");
                    }
                }
            }
            other => bail!("unknown sandbox root mode `{other}`"),
        }
        Ok(spec)
    }

    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("read sandbox spec {}", path.display()))?;
        Self::parse(&text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_parses_layers_for_dir_roots() {
        let spec = SandboxSpec::parse(
            r#"
version = 1
script = "true"

[root]
mode = "dir"
dir = "/buildroots/x"
layers = ["/var/cache/nawa/layers/aaa", "/var/cache/nawa/layers/bbb"]

[build]
src_host = "/w/src"
pkg_host = "/w/pkg"
home_host = "/w/home"
scratch = "/w/sb"
"#,
        )
        .unwrap();
        assert_eq!(spec.root.layers.len(), 2);
        assert_eq!(spec.root.layers[0], "/var/cache/nawa/layers/aaa");
        // live roots never carry layers
        let live = SandboxSpec::parse(
            r#"
version = 1
script = "true"
[build]
src_host = "/w/src"
pkg_host = "/w/pkg"
home_host = "/w/home"
scratch = "/w/sb"
"#,
        )
        .unwrap();
        assert!(live.root.layers.is_empty());
    }

    #[test]
    fn layers_default_to_empty_for_older_specs() {
        // a spec written before Phase 5 (no [root.layers]) must keep parsing
        let spec = SandboxSpec::parse(
            r#"
version = 1
script = "true"
[root]
mode = "dir"
dir = "/r"
[build]
src_host = "/w/src"
pkg_host = "/w/pkg"
home_host = "/w/home"
scratch = "/w/sb"
"#,
        )
        .unwrap();
        assert!(spec.root.layers.is_empty());
    }
}
