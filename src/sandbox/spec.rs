//! The sandbox spec: a TOML file describing root mode, build paths, the
//! environment whitelist, the script and the degradation policy. This module
//! is pure data + validation — it never touches the kernel, so all of it is
//! unit-testable without namespaces.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// Everything the sandbox needs, serialised as TOML next to the build logs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct SandboxSpec {
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
    /// Optional uid/gid to drop to after the filesystem has been assembled.
    #[serde(default)]
    pub run_uid: Option<u32>,
    #[serde(default)]
    pub run_gid: Option<u32>,
    /// Optional hard limits; zero keeps the existing build behavior.
    #[serde(default)]
    pub memory_limit_bytes: u64,
    #[serde(default)]
    pub process_limit: u64,
    #[serde(default)]
    pub file_size_limit_bytes: u64,
    #[serde(default)]
    pub open_files_limit: u64,
    /// Existing bind mounts to mark read-only before the root is locked down.
    #[serde(default)]
    pub readonly_paths: Vec<String>,
    /// Extra directory/file binds the build needs (§5.3): the assets
    /// snapshot at `<build_inside>/assets`, later the fetch views. `host`
    /// is opened as an O_PATH handle before any cover (with the staging
    /// handles), bound at `inside` and, when `readonly`, remounted
    /// read-only at once. `serde(default)` keeps older specs parsing.
    /// Protected host paths are refused; see [`ExtraBind`].
    #[serde(default)]
    pub extra_binds: Vec<ExtraBind>,
    /// Make every mount below / read-only except writable_paths.
    #[serde(default)]
    pub strict_readonly_root: bool,
    /// Explicit private writable mounts for strict plugin sandboxes.
    #[serde(default)]
    pub writable_paths: Vec<String>,
}

/// One extra bind mount, raw data of the spec (§5.3).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ExtraBind {
    /// Host path of the bind source. It must be absolute, free of `..` and
    /// outside the host root, host configuration/kernel filesystems and
    /// Nawa's state trees (`etc/nawa`, `var/lib/nawa`, `var/cache/nawa`,
    /// also below a custom `--root` prefix). Build assets, fetch views,
    /// resolver copies and sandboxed fetch cache entries remain allowed.
    pub host: String,
    /// Absolute path *inside* the sandbox, under `build_inside` (validated
    /// like `readonly_paths`: absolute, no `..`).
    pub inside: String,
    /// Remount the bind read-only right after mounting it. A *writable*
    /// extra bind must additionally be listed in `writable_paths` when
    /// `strict_readonly_root` is set.
    #[serde(default)]
    pub readonly: bool,
    /// `dir` (bind a directory) or `file` (bind a single file, whose
    /// placeholder is created first).
    pub kind: String,
}

fn default_build_inside() -> String {
    "/build".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RootSpec {
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
pub(crate) struct BuildPaths {
    /// Host path of the source staging dir (bound at /build/src).
    pub src_host: String,
    /// Host path of the DESTDIR staging dir (bound at /build/pkg).
    pub pkg_host: String,
    /// Host path of an empty dir (bound at /build/home).
    pub home_host: String,
    /// Host dir for the sandbox's own scratch (newroot, home, …).
    pub scratch: String,
}

/// Host-path policy for an extra bind (§5.3 + the P0-3 audit). Legitimate
/// bind sources are the per-build assets/fetch/resolver paths and the
/// sandboxed fetch cache entries. Everything else is not a build input, so
/// the policy refuses the host root, host configuration/kernel filesystems
/// and Nawa's own state trees — including when `--root` places them below an
/// arbitrary prefix. Custom `$NAWA_SRCCACHE` and temporary Cargo cwd paths
/// stay allowed.
fn validate_extra_bind_host(host: &str, inside: &str) -> Result<()> {
    let host = Path::new(host);
    if !host.is_absolute() {
        bail!("sandbox extra bind host must be absolute: {host:?} (inside {inside:?})");
    }
    if host
        .components()
        .any(|part| matches!(part, std::path::Component::ParentDir))
    {
        bail!("sandbox extra bind host must not contain '..': {host:?} (inside {inside:?})");
    }
    // `etc/nawa` is the trust store and configuration, `var/lib/nawa` the
    // package db/journal/lock and `var/cache/nawa` the repository/archive
    // caches. Matching the component suffix covers `--root <prefix>` too.
    let nawa_state = host.ancestors().any(|path| {
        path.ends_with("etc/nawa")
            || path.ends_with("var/lib/nawa")
            || path.ends_with("var/cache/nawa")
    });
    let kernel_interface = host.starts_with("/etc")
        || host.starts_with("/proc")
        || host.starts_with("/sys")
        || host.starts_with("/boot");
    let host_root =
        host == Path::new("/") || host == Path::new("/dev") || host == Path::new("/run");
    if nawa_state || kernel_interface || host_root {
        bail!(
            "sandbox extra bind host {} is a protected host path and cannot be \
             exposed to the sandbox (inside {inside:?})",
            host.display()
        );
    }
    Ok(())
}

impl SandboxSpec {
    pub fn parse(text: &str) -> Result<Self> {
        let spec: SandboxSpec = toml::from_str(text).context("bad sandbox spec")?;
        if spec.version != 1 {
            bail!("unsupported sandbox spec version {}", spec.version);
        }
        if spec.run_uid.is_some() != spec.run_gid.is_some() {
            bail!("sandbox run_uid and run_gid must be specified together");
        }
        for path in spec.readonly_paths.iter().chain(spec.writable_paths.iter()) {
            let parsed = Path::new(path);
            if !parsed.is_absolute()
                || parsed.components().any(|part| matches!(part, std::path::Component::ParentDir))
            {
                bail!("sandbox mount path must be absolute and must not contain '..': {path:?}");
            }
        }
        for b in &spec.extra_binds {
            let inside = Path::new(&b.inside);
            if !inside.is_absolute()
                || inside
                    .components()
                    .any(|part| matches!(part, std::path::Component::ParentDir))
            {
                bail!(
                    "sandbox extra bind inside must be absolute and must not contain '..': {:?}",
                    b.inside
                );
            }
            if b.host.trim().is_empty() {
                bail!("sandbox extra bind for {:?} has no host path", b.inside);
            }
            validate_extra_bind_host(&b.host, &b.inside)?;
            match b.kind.as_str() {
                "dir" | "file" => {}
                other => bail!(
                    "sandbox extra bind kind must be \"dir\" or \"file\", got {other:?} (inside {:?})",
                    b.inside
                ),
            }
        }
        if spec.strict_readonly_root && spec.writable_paths.is_empty() {
            bail!("strict read-only sandbox must explicitly list its writable mounts");
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
    fn plugin_sandbox_fields_are_validated() {
        let base = r#"
version = 1
script = "true"
run_uid = 65534
run_gid = 65534
strict_readonly_root = true
writable_paths = ["/tmp", "/build/home"]
readonly_paths = ["/build/src"]
[build]
src_host = "/w/src"
pkg_host = "/w/pkg"
home_host = "/w/home"
scratch = "/w/sb"
"#;
        let spec = SandboxSpec::parse(base).unwrap();
        assert_eq!(spec.run_uid, Some(65534));
        assert!(spec.strict_readonly_root);
        assert_eq!(spec.writable_paths, vec!["/tmp", "/build/home"]);

        assert!(SandboxSpec::parse(&base.replace("run_gid = 65534\n", "")).is_err());
        assert!(SandboxSpec::parse(&base.replace("/build/src", "../escape")).is_err());
        assert!(SandboxSpec::parse(&base.replace("writable_paths = [\"/tmp\", \"/build/home\"]", "writable_paths = []")).is_err());
    }

    /// §5.3: extra binds are raw data of the spec, `serde(default)` so older
    /// specs keep parsing; `inside` is validated like `readonly_paths`.
    #[test]
    fn extra_binds_parse_and_validate() {
        let base = r#"
version = 1
script = "true"
[build]
src_host = "/w/src"
pkg_host = "/w/pkg"
home_host = "/w/home"
scratch = "/w/sb"

[[extra_binds]]
host = "/s/build/boot/linux/assets"
inside = "/build/assets"
readonly = true
kind = "dir"

[[extra_binds]]
host = "/s/.conf"
inside = "/build/fetch/seeds.conf"
readonly = false
kind = "file"
"#;
        let spec = SandboxSpec::parse(base).unwrap();
        assert_eq!(spec.extra_binds.len(), 2);
        assert_eq!(spec.extra_binds[0].inside, "/build/assets");
        assert!(spec.extra_binds[0].readonly);
        assert_eq!(spec.extra_binds[1].kind, "file");

        // serde default: a spec without extra_binds (older revision) parses
        let legacy =
            SandboxSpec::parse(base.split("[[extra_binds]]").next().unwrap()).unwrap();
        assert!(legacy.extra_binds.is_empty());

        assert!(SandboxSpec::parse(&base.replace("/build/assets", "assets")).is_err());
        assert!(SandboxSpec::parse(&base.replace("/build/assets", "/build/../assets")).is_err());
        assert!(
            SandboxSpec::parse(&base.replace("kind = \"file\"", "kind = \"fyle\"")).is_err(),
            "unknown file kind is refused"
        );
        // an empty host path is refused
        assert!(
            SandboxSpec::parse(&base.replace("host = \"/s/.conf\"", "host = \"\"")).is_err(),
            "missing host path is refused"
        );
    }

    /// The P0-3 policy: extra binds must not expose Nawa's state trees
    /// (trust store, package db, caches), the host root or kernel/config
    /// filesystems, while per-build inputs and sandboxed fetch cache entries
    /// stay allowed.
    #[test]
    fn extra_binds_refuse_protected_host_paths() {
        let base = r#"
version = 1
script = "true"
[build]
src_host = "/w/src"
pkg_host = "/w/pkg"
home_host = "/w/home"
scratch = "/w/sb"

[[extra_binds]]
host = "HOST"
inside = "/build/input"
kind = "dir"
"#;
        let parse = |host: &str| SandboxSpec::parse(&base.replace("HOST", host));

        // assets, fetch views, resolver copies and sandboxed fetch entries
        parse("/w/assets").unwrap();
        parse("/w/fetch/go").unwrap();
        parse("/w/fetch/etc/resolv.conf").unwrap();
        parse("/w").unwrap();
        parse("/home/builder/build/distfiles/fetch/go/entry.tmp").unwrap();
        parse("/tmp/nawa-cargo-cwd-1234").unwrap();
        parse("/dev/shm/nawa-build/assets").unwrap();
        parse("/run/user/1000/nawa-build/fetch").unwrap();

        for host in [
            "/",
            "/etc",
            "/etc/shadow",
            "/etc/ssl/certs",
            "/boot",
            "/etc/nawa",
            "/etc/nawa/trusted-keys",
            "/var/lib/nawa",
            "/var/lib/nawa/local/demo",
            "/var/cache/nawa",
            "/var/cache/nawa/layers/abc",
            // custom --root prefixes carry the same state trees
            "/tmp/testroot/etc/nawa/trusted-keys",
            "/tmp/testroot/var/lib/nawa",
            "/tmp/testroot/var/cache/nawa/remote",
            "/proc",
            "/proc/self",
            "/sys",
            "/sys/class/net",
            "/dev",
            "/run",
            "/w/../etc/nawa",
            "w/assets",
        ] {
            assert!(
                parse(host).is_err(),
                "extra bind source {host:?} must be refused"
            );
        }
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
