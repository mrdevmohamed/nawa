//! Build profiles: `profiles/<arch>.toml`.
//!
//! A profile carries the compiler/linker flags (and default job count) that
//! every package built for that architecture should get, so the whole
//! distribution is compiled consistently. The build engine exports them to
//! the recipe's build sections as `CFLAGS`, `CXXFLAGS` and `LDFLAGS`.
//!
//! * A missing profile file is not an error — builds then use the toolchain
//!   defaults, exactly as before profiles existed.
//! * Flags already present in nawa's own environment win over the profile, so
//!   `CFLAGS="-O3" nawa build foo` still works for one-off experiments.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Clone, Default, Deserialize)]
struct ProfileFile {
    #[serde(default)]
    profile: Profile,
    #[serde(default)]
    build: BuildProfile,
}

/// The `[build]` table: isolation defaults for every build
/// (docs/design/build-isolation.md §8).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct BuildProfile {
    /// `none` | `env` | `sandbox` | `closure`; `nawa build --isolation` wins.
    /// Stored as a raw string because the default (`""`) must mean `none`
    /// without making every older profile invalid.
    #[serde(default)]
    pub isolation: Option<String>,
    /// Per-section wall-clock timeout in seconds (sandbox levels only).
    /// Default: 4 h, as in the design.
    #[serde(default)]
    pub timeout: Option<u64>,
    /// `slim = false` turns off the post-install slimming of every package
    /// (strip, cleanup, `-doc`/`-lang` split). Default: on. The environment
    /// variable `NAWA_SLIM=0|1` wins over this.
    #[serde(default)]
    pub slim: Option<bool>,
}

/// The `[profile]` table. Sub-tables (`[profile.toolchain]`,
/// `[profile.bootstrap]`) are informational for now and ignored here.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Profile {
    #[serde(default)]
    pub arch: String,
    #[serde(default)]
    pub cflags: String,
    #[serde(default)]
    pub cxxflags: String,
    #[serde(default)]
    pub ldflags: String,
    /// Default parallel jobs; `--jobs` on the command line overrides it.
    #[serde(default)]
    pub jobs: Option<u32>,
}

impl Profile {
    pub fn parse(raw: &str) -> Result<Self> {
        let f: ProfileFile = toml::from_str(raw).context("bad profile")?;
        Ok(f.profile)
    }

    /// Parse the `[build]` table of a profile.
    pub fn parse_build(raw: &str) -> Result<BuildProfile> {
        let f: ProfileFile = toml::from_str(raw).context("bad profile")?;
        Ok(f.build)
    }

    /// Load `<dir>/<arch>.toml`. `Ok(None)` when the file does not exist.
    pub fn load(dir: &Path, arch: &str) -> Result<Option<Self>> {
        if arch.is_empty() || arch.contains('/') || arch.contains("..") {
            bail!("invalid architecture name `{arch}`");
        }
        let path = dir.join(format!("{arch}.toml"));
        if !path.is_file() {
            return Ok(None);
        }
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("cannot read profile {}", path.display()))?;
        let p = Self::parse(&raw).with_context(|| format!("in {}", path.display()))?;
        if !p.arch.is_empty() && p.arch != arch {
            bail!(
                "profile {} declares arch `{}` but is used for `{}`",
                path.display(),
                p.arch,
                arch
            );
        }
        Ok(Some(p))
    }

    /// Environment variables to export to build sections. Variables already
    /// set in nawa's own environment are left alone (explicit beats profile).
    pub fn env(&self) -> Vec<(String, String)> {
        self.env_with(|k| std::env::var_os(k).is_some())
    }

    pub fn env_with(&self, already_set: impl Fn(&str) -> bool) -> Vec<(String, String)> {
        [
            ("CFLAGS", &self.cflags),
            ("CXXFLAGS", &self.cxxflags),
            ("LDFLAGS", &self.ldflags),
        ]
        .into_iter()
        .filter(|(k, v)| !v.trim().is_empty() && !already_set(k))
        .map(|(k, v)| (k.to_string(), v.trim().to_string()))
        .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHIPPED: &str = r#"
[profile]
arch = "x86_64"
cflags = "-O2 -pipe"
cxxflags = "-O2 -pipe"
ldflags = ""
jobs = 4

[profile.toolchain]
compiler = "gcc"
libc = "glibc"

[profile.bootstrap]
min_kernel = "5.4"
"#;

    #[test]
    fn parses_shipped_layout_with_subtables() {
        let p = Profile::parse(SHIPPED).unwrap();
        assert_eq!(p.arch, "x86_64");
        assert_eq!(p.cflags, "-O2 -pipe");
        assert_eq!(p.jobs, Some(4));
    }

    #[test]
    fn parses_build_table_and_missing_means_none() {
        let b = Profile::parse_build(SHIPPED).unwrap();
        assert_eq!(b.isolation, None);
        assert_eq!(b.timeout, None);
        assert_eq!(b.slim, None);
        let with_build = SHIPPED.to_string()
            + "\n[build]\nisolation = \"sandbox\"\ntimeout = 600\nslim = false\n";
        let b = Profile::parse_build(&with_build).unwrap();
        assert_eq!(b.isolation.as_deref(), Some("sandbox"));
        assert_eq!(b.timeout, Some(600));
        assert_eq!(b.slim, Some(false));
    }

    #[test]
    fn empty_flags_are_not_exported() {
        let p = Profile::parse(SHIPPED).unwrap();
        let env = p.env_with(|_| false);
        let keys: Vec<&str> = env.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, ["CFLAGS", "CXXFLAGS"]); // ldflags is empty
    }

    #[test]
    fn existing_environment_wins() {
        let p = Profile::parse(SHIPPED).unwrap();
        let env = p.env_with(|k| k == "CFLAGS");
        assert_eq!(env, vec![("CXXFLAGS".to_string(), "-O2 -pipe".to_string())]);
    }

    #[test]
    fn missing_file_is_none_and_bad_arch_is_rejected() {
        let d = std::env::temp_dir().join(format!("nawa-profile-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        assert!(Profile::load(&d, "x86_64").unwrap().is_none());
        assert!(Profile::load(&d, "../etc").is_err());
        std::fs::write(d.join("aarch64.toml"), "[profile]\narch = \"x86_64\"\n").unwrap();
        assert!(Profile::load(&d, "aarch64").is_err());
        std::fs::write(d.join("x86_64.toml"), SHIPPED).unwrap();
        assert!(Profile::load(&d, "x86_64").unwrap().is_some());
        let _ = std::fs::remove_dir_all(&d);
    }
}
