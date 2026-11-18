//! `system/profile`: read the *desired state* (§5.3, §7) — the effective
//! target profile merged from the distribution-owned defaults and the
//! local administrator override.
//!
//! Recommended layout (§7), with the exact filename pending the Faseela
//! agreement (blocker B4) — paths are injected below so the contract can
//! change without touching this logic:
//!
//! * distribution defaults: `<root>/usr/share/nawa/system-profile.toml`
//! * administrator override: `<root>/etc/nawa/system-profile.toml`
//!
//! Semantics (§7): both files missing → no desired state is declared
//! (`Ok(None)` — an absent profile is not read as a decision, and nothing
//! is guessed). An administrator override may exist without distribution
//! defaults. An invalid file — unknown key, wrong or unknown schema, bad
//! provider declaration — fails closed: it is an error naming the file,
//! never silently ignored in favor of the other side, and never a partial
//! trust. Overrides take precedence over defaults; the merge rules live in
//! the domain ([`crate::domain::system::profile::SystemProfile::merge`]).
//!
//! Read-only (§5.4): this module writes nothing. In particular a package
//! update never touches these files — local policy is never silently
//! overwritten (§7); only the administrator edits an override. This is the
//! system profile, not the build profile of `src/profile.rs`
//! (`--profiles`): separate models, separate paths (§7).

use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::path::Path;

use crate::domain::system::profile::{ProvidersDoc, SystemProfile};

/// The effective target profile together with the file that supplied each
/// declared provider role (§5.1: an observation must include its source —
/// when the merge takes the override's `init-system`, the source is the
/// override file, otherwise the defaults file).
#[derive(Debug)]
pub(crate) struct EffectiveProfile {
    pub(crate) profile: Option<SystemProfile>,
    pub(crate) role_sources: BTreeMap<String, std::path::PathBuf>,
}

/// The effective target profile for `root`: merge the recommended
/// distribution defaults and administrator override (§7's recommended
/// layout; the exact filename is the open B4 contract).
pub(crate) fn load_effective(root: &Path) -> Result<EffectiveProfile> {
    load_effective_at(
        &root.join("usr/share/nawa/system-profile.toml"),
        &root.join("etc/nawa/system-profile.toml"),
    )
}

/// The effective target profile from two explicit files (injectable so the
/// B4 location contract can change and the rules stay unit-testable).
pub(crate) fn load_effective_at(defaults: &Path, overrides: &Path) -> Result<EffectiveProfile> {
    let defaults_profile = read_one(defaults)?;
    let overrides_profile = read_one(overrides)?;
    let profile = SystemProfile::merge(defaults_profile.as_ref(), overrides_profile.as_ref());
    let mut role_sources = BTreeMap::new();
    if let Some(p) = &profile {
        for role in p.providers.keys() {
            let source = if overrides_profile
                .as_ref()
                .is_some_and(|o| o.providers.contains_key(role))
            {
                overrides
            } else {
                defaults
            };
            role_sources.insert(role.clone(), source.to_path_buf());
        }
    }
    Ok(EffectiveProfile {
        profile,
        role_sources,
    })
}

/// Read one profile file: `Ok(None)` when it does not exist (absence is
/// not an error and not a decision), a validated [`SystemProfile`] when it
/// does, a named error when it is invalid (fail closed, §7).
fn read_one(path: &Path) -> Result<Option<SystemProfile>> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(e).with_context(|| format!("system profile {}", path.display()));
        }
    };
    let (scalars, providers) =
        parse_doc(&text).with_context(|| format!("system profile {}", path.display()))?;
    SystemProfile::from_doc(&scalars, &providers)
        .map_err(anyhow::Error::msg)
        .map(Some)
        .with_context(|| format!("system profile {}", path.display()))
}

/// Convert one profile document's text into the normalized maps the domain
/// consumes. Strict: every top-level key must be a scalar (the optional
/// `[providers]` section is the only table), and the domain rejects keys
/// it does not know — so a newer profile document fails loudly instead of
/// being silently misread.
fn parse_doc(text: &str) -> Result<(BTreeMap<String, String>, ProvidersDoc)> {
    let value: toml::Value = toml::from_str(text).context("profile is not valid TOML")?;
    let table = value
        .as_table()
        .ok_or_else(|| anyhow::anyhow!("profile is not a TOML table"))?;
    let mut scalars = BTreeMap::new();
    let mut providers: ProvidersDoc = None;
    for (key, value) in table {
        if key == "providers" {
            let section = value
                .as_table()
                .ok_or_else(|| anyhow::anyhow!("profile key `providers` must be a table"))?;
            let mut map = BTreeMap::new();
            for (role, identity) in section {
                let identity = scalar_str(identity, &format!("provider `{role}`"))?;
                map.insert(role.clone(), identity);
            }
            providers = Some(map);
            continue;
        }
        let s = scalar_str(value, &format!("profile key `{key}`"))?;
        scalars.insert(key.clone(), s);
    }
    Ok((scalars, providers))
}

/// One scalar as a string: strings pass, integers are stringified
/// (`schema = 1`), everything else is rejected.
fn scalar_str(value: &toml::Value, what: &str) -> Result<String> {
    match value {
        toml::Value::String(s) => Ok(s.clone()),
        toml::Value::Integer(i) => Ok(i.to_string()),
        other => Err(anyhow::anyhow!(
            "{what} must be a scalar value (got {other:?})"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DESKTOP: &str = "\
schema = 1
distribution = \"faseela\"
profile = \"desktop\"

[providers]
init-system = \"systemd\"
";

    fn write(path: &Path, text: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, text).unwrap();
    }

    struct Fixture {
        dir: std::path::PathBuf,
        defaults: std::path::PathBuf,
        overrides: std::path::PathBuf,
    }

    impl Fixture {
        fn new(tag: &str) -> Fixture {
            let dir =
                std::env::temp_dir().join(format!("nawa-profile-{}-{}", tag, std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Fixture {
                defaults: dir.join("defaults.toml"),
                overrides: dir.join("overrides.toml"),
                dir,
            }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn both_files_missing_means_no_declared_desired_state() {
        let f = Fixture::new("absent");
        let eff = load_effective_at(&f.defaults, &f.overrides).unwrap();
        assert_eq!(eff.profile, None);
    }

    #[test]
    fn the_illustrative_profile_loads() {
        let f = Fixture::new("defaults");
        write(&f.defaults, DESKTOP);
        let eff = load_effective_at(&f.defaults, &f.overrides)
            .unwrap()
            .profile
            .unwrap();
        assert_eq!(eff.distribution.as_deref(), Some("faseela"));
        assert_eq!(eff.provider("init-system"), Some("systemd"));
    }

    #[test]
    fn an_override_without_defaults_is_a_valid_profile() {
        let f = Fixture::new("override-only");
        write(
            &f.overrides,
            "schema = 1\n\n[providers]\ninit-system = \"systemd\"\n",
        );
        let eff = load_effective_at(&f.defaults, &f.overrides)
            .unwrap()
            .profile
            .unwrap();
        assert_eq!(eff.provider("init-system"), Some("systemd"));
    }

    #[test]
    fn overrides_take_precedence_over_defaults() {
        let f = Fixture::new("merge");
        write(&f.defaults, DESKTOP);
        write(
            &f.overrides,
            "schema = 1\nprofile = \"server\"\n\n[providers]\ninit-system = \"runit\"\n",
        );
        let eff = load_effective_at(&f.defaults, &f.overrides)
            .unwrap()
            .profile
            .unwrap();
        assert_eq!(eff.profile.as_deref(), Some("server"));
        assert_eq!(eff.provider("init-system"), Some("runit"));
        assert_eq!(eff.distribution.as_deref(), Some("faseela"));
    }

    #[test]
    fn an_invalid_override_fails_closed_naming_the_file() {
        // §7: an invalid override is an error, never silently ignored in
        // favor of the valid defaults
        let f = Fixture::new("bad-override");
        write(&f.defaults, DESKTOP);
        write(&f.overrides, "schema = 2\n");
        let e = load_effective_at(&f.defaults, &f.overrides).unwrap_err();
        let msg = format!("{e:#}");
        assert!(msg.contains("unknown profile schema 2"), "{msg}");
        assert!(msg.contains("overrides.toml"), "{msg}");
    }

    #[test]
    fn an_invalid_defaults_file_fails_closed_too() {
        let f = Fixture::new("bad-defaults");
        write(&f.defaults, "unknown-key = \"x\"\n");
        let e = load_effective_at(&f.defaults, &f.overrides).unwrap_err();
        assert!(format!("{e:#}").contains("unknown profile key"),);
    }

    #[test]
    fn a_table_where_a_scalar_is_expected_is_an_error() {
        let f = Fixture::new("table-key");
        write(&f.defaults, "[extra]\nfield = \"x\"\n");
        let e = load_effective_at(&f.defaults, &f.overrides).unwrap_err();
        assert!(format!("{e:#}").contains("`extra`"),);
    }

    #[test]
    fn an_empty_file_is_an_invalid_profile_not_an_absent_one() {
        let f = Fixture::new("empty");
        write(&f.defaults, "");
        let e = load_effective_at(&f.defaults, &f.overrides).unwrap_err();
        assert!(format!("{e:#}").contains("no `schema` key"),);
    }

    #[test]
    fn schema_may_be_written_as_an_integer() {
        let f = Fixture::new("int-schema");
        write(&f.defaults, "schema = 1\n");
        let eff = load_effective_at(&f.defaults, &f.overrides)
            .unwrap()
            .profile
            .unwrap();
        assert_eq!(eff.schema, 1);
    }

    #[test]
    fn declared_roles_carry_their_declaring_file_as_the_source() {
        // §5.1 attribution: the override declares init-system, so the
        // override file is that role's source; a role only the defaults
        // declare points at the defaults file
        let f = Fixture::new("sources");
        write(
            &f.defaults,
            "schema = 1\n\n[providers]\nsession-manager = \"elogind\"\n",
        );
        write(
            &f.overrides,
            "schema = 1\n\n[providers]\ninit-system = \"systemd\"\n",
        );
        let eff = load_effective_at(&f.defaults, &f.overrides).unwrap();
        assert_eq!(
            eff.role_sources.get("init-system").map(|p| p.as_path()),
            Some(f.overrides.as_path())
        );
        assert_eq!(
            eff.role_sources.get("session-manager").map(|p| p.as_path()),
            Some(f.defaults.as_path())
        );
    }

    #[test]
    fn load_effective_uses_the_faseela_distribution_and_admin_override_paths() {
        let f = Fixture::new("faseela-layout");
        let defaults = f.dir.join("usr/share/nawa/system-profile.toml");
        let overrides = f.dir.join("etc/nawa/system-profile.toml");
        write(
            &defaults,
            "schema = 1\ndistribution = \"faseela\"\narchitecture = \"x86_64\"\n\n[providers]\ninit-system = \"systemd\"\n",
        );

        // This is the exact path and schema emitted by Faseela's base-files
        // recipe. The production loader must discover it without injected paths.
        let distribution = load_effective(&f.dir).unwrap();
        let profile = distribution.profile.as_ref().unwrap();
        assert_eq!(profile.distribution.as_deref(), Some("faseela"));
        assert_eq!(profile.architecture.as_deref(), Some("x86_64"));
        assert_eq!(profile.provider("init-system"), Some("systemd"));
        assert_eq!(
            distribution.role_sources.get("init-system").map(|p| p.as_path()),
            Some(defaults.as_path())
        );

        write(
            &overrides,
            "schema = 1\nprofile = \"administrator-managed\"\n\n[providers]\ninit-system = \"runit\"\n",
        );
        let effective = load_effective(&f.dir).unwrap();
        let profile = effective.profile.as_ref().unwrap();
        assert_eq!(profile.distribution.as_deref(), Some("faseela"));
        assert_eq!(profile.profile.as_deref(), Some("administrator-managed"));
        assert_eq!(profile.provider("init-system"), Some("runit"));
        assert_eq!(
            effective.role_sources.get("init-system").map(|p| p.as_path()),
            Some(overrides.as_path())
        );
    }

}
