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

use crate::domain::system::profile::{
    ProviderPoliciesDoc, ProviderPolicyDoc, ProvidersDoc, SystemProfile,
};

/// The effective target profile together with the file that supplied each
/// declared provider role (§5.1: an observation must include its source —
/// when the merge takes the override's `init-system`, the source is the
/// override file, otherwise the defaults file).
#[derive(Debug)]
pub(crate) struct EffectiveProfile {
    pub(crate) profile: Option<SystemProfile>,
    /// Source of each required provider shorthand/selection.
    pub(crate) role_sources: BTreeMap<String, std::path::PathBuf>,
    /// Source of the complete policy declared for each capability domain.
    pub(crate) policy_sources: BTreeMap<String, std::path::PathBuf>,
}

/// The effective target profile for `root`: merge the recommended
/// distribution defaults and administrator override (§7's recommended
/// layout; the exact filename is the open B4 contract).
pub(crate) fn load_effective(root: &Path) -> Result<EffectiveProfile> {
    let defaults = target_profile_path(root, Path::new("usr/share/nawa/system-profile.toml"))?;
    let overrides = target_profile_path(root, Path::new("etc/nawa/system-profile.toml"))?;
    load_effective_at(&defaults, &overrides)
}

/// Resolve a profile path while ensuring that an existing file or its
/// nearest existing ancestor stays inside the selected target root.
///
/// Canonicalizing the complete path also rejects a final profile-file
/// symlink that points outside the target. When the profile or some parent
/// directory is absent, the nearest existing ancestor is checked and the
/// unresolved target path is returned so the normal NotFound behavior is
/// preserved. This is a containment check, not a race-free openat walk.
fn target_profile_path(root: &Path, relative: &Path) -> Result<std::path::PathBuf> {
    if relative.is_absolute()
        || relative
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        anyhow::bail!(
            "invalid system profile path beneath target root: {}",
            relative.display()
        );
    }

    let canonical_root = std::fs::canonicalize(root)
        .with_context(|| format!("canonicalize system target root {}", root.display()))?;
    let candidate = canonical_root.join(relative);

    match std::fs::canonicalize(&candidate) {
        Ok(resolved) => {
            if !resolved.starts_with(&canonical_root) {
                anyhow::bail!(
                    "system profile {} resolves outside selected target root {}",
                    candidate.display(),
                    canonical_root.display()
                );
            }
            Ok(resolved)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            // The file may be absent, but do not let an already-present
            // ancestor symlink redirect even the missing path outside root.
            let mut ancestor = candidate.parent();
            while let Some(path) = ancestor {
                match std::fs::canonicalize(path) {
                    Ok(resolved) => {
                        if !resolved.starts_with(&canonical_root) {
                            anyhow::bail!(
                                "system profile path {} has an ancestor outside selected target root {}",
                                candidate.display(),
                                canonical_root.display()
                            );
                        }
                        break;
                    }
                    Err(parent_error) if parent_error.kind() == std::io::ErrorKind::NotFound => {
                        ancestor = path.parent();
                    }
                    Err(parent_error) => {
                        return Err(parent_error).with_context(|| {
                            format!("inspect system profile path ancestor {}", path.display())
                        });
                    }
                }
            }
            Ok(candidate)
        }
        Err(error) => {
            Err(error).with_context(|| format!("resolve system profile {}", candidate.display()))
        }
    }
}

/// The effective target profile from two explicit files (injectable so the
/// B4 location contract can change and the rules stay unit-testable).
pub(crate) fn load_effective_at(defaults: &Path, overrides: &Path) -> Result<EffectiveProfile> {
    let defaults_profile = read_one(defaults)?;
    let overrides_profile = read_one(overrides)?;
    let profile = SystemProfile::merge(defaults_profile.as_ref(), overrides_profile.as_ref());
    let mut role_sources = BTreeMap::new();
    let mut policy_sources = BTreeMap::new();
    if let Some(p) = &profile {
        for role in p.policies.keys() {
            let source = if overrides_profile
                .as_ref()
                .is_some_and(|o| o.policies.contains_key(role))
            {
                overrides
            } else {
                defaults
            };
            policy_sources.insert(role.clone(), source.to_path_buf());
            if p.provider(role).is_some() {
                role_sources.insert(role.clone(), source.to_path_buf());
            }
        }
    }
    Ok(EffectiveProfile {
        profile,
        role_sources,
        policy_sources,
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
    let (scalars, providers, policies) =
        parse_doc(&text).with_context(|| format!("system profile {}", path.display()))?;
    SystemProfile::from_doc_with_policies(&scalars, &providers, &policies)
        .map_err(anyhow::Error::msg)
        .map(Some)
        .with_context(|| format!("system profile {}", path.display()))
}

/// Convert one profile document's text into the normalized maps the domain
/// consumes. Strict: every top-level key must be a scalar (the optional
/// `[providers]` section is the only table), and the domain rejects keys
/// it does not know — so a newer profile document fails loudly instead of
/// being silently misread.
fn parse_doc(text: &str) -> Result<(BTreeMap<String, String>, ProvidersDoc, ProviderPoliciesDoc)> {
    let value: toml::Value = toml::from_str(text).context("profile is not valid TOML")?;
    let table = value
        .as_table()
        .ok_or_else(|| anyhow::anyhow!("profile is not a TOML table"))?;
    let mut scalars = BTreeMap::new();
    let mut providers: ProvidersDoc = None;
    let mut policies: ProviderPoliciesDoc = None;

    for (key, value) in table {
        if key == "providers" {
            let section = value
                .as_table()
                .ok_or_else(|| anyhow::anyhow!("profile key providers must be a table"))?;
            let mut map = BTreeMap::new();
            for (role, identity) in section {
                let identity = scalar_str(identity, &format!("provider {role:?}"))?;
                map.insert(role.clone(), identity);
            }
            providers = Some(map);
            continue;
        }

        if key == "policy" {
            let section = value
                .as_table()
                .ok_or_else(|| anyhow::anyhow!("profile key policy must be a table"))?;
            let mut domains = BTreeMap::new();
            for (role, value) in section {
                let table = value
                    .as_table()
                    .ok_or_else(|| anyhow::anyhow!("policy for {role:?} must be a table"))?;
                let mut policy = ProviderPolicyDoc::default();
                for (field, value) in table {
                    let values = string_array(value, &format!("policy {role:?}.{field}"))?;
                    match field.as_str() {
                        "required" => policy.required = values,
                        "preferred" => policy.preferred = values,
                        "forbidden" => policy.forbidden = values,
                        _ => anyhow::bail!(
                            "unknown provider policy field {field:?} for domain {role:?}"
                        ),
                    }
                }
                domains.insert(role.clone(), policy);
            }
            policies = Some(domains);
            continue;
        }

        let scalar = scalar_str(value, &format!("profile key `{key}`"))?;
        scalars.insert(key.clone(), scalar);
    }
    Ok((scalars, providers, policies))
}

/// Parse a policy identity list. Every element must be a TOML string; silently
/// skipping a malformed constraint would weaken the administrator's policy.
fn string_array(value: &toml::Value, what: &str) -> Result<Vec<String>> {
    let array = value
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("{what} must be an array of strings"))?;
    array
        .iter()
        .enumerate()
        .map(|(index, value)| {
            value
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| anyhow::anyhow!("{what}[{index}] must be a string"))
        })
        .collect()
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
    fn load_effective_rejects_a_profile_beneath_an_external_ancestor_symlink() {
        use std::os::unix::fs::symlink;

        let f = Fixture::new("external-ancestor");
        let outside_root = f.dir.with_file_name(format!(
            "{}-outside-root",
            f.dir.file_name().unwrap().to_string_lossy()
        ));
        let outside = outside_root.join("usr/share/nawa");
        std::fs::create_dir_all(&outside).unwrap();
        write(&outside.join("system-profile.toml"), DESKTOP);
        symlink(outside_root.join("usr"), f.dir.join("usr")).unwrap();

        let error = load_effective(&f.dir).unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains("outside selected target root"),
            "an external ancestor must be refused: {message}"
        );
        let _ = std::fs::remove_dir_all(outside_root);
    }

    #[test]
    fn load_effective_rejects_a_final_profile_symlink_outside_the_root() {
        use std::os::unix::fs::symlink;

        let f = Fixture::new("external-file");
        let outside = f.dir.with_file_name(format!(
            "{}-outside-profile.toml",
            f.dir.file_name().unwrap().to_string_lossy()
        ));
        write(&outside, DESKTOP);
        let defaults = f.dir.join("usr/share/nawa/system-profile.toml");
        std::fs::create_dir_all(defaults.parent().unwrap()).unwrap();
        symlink(&outside, &defaults).unwrap();

        let error = load_effective(&f.dir).unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains("outside selected target root"),
            "an external final symlink must be refused: {message}"
        );
        let _ = std::fs::remove_file(outside);
    }

    #[test]
    fn load_effective_accepts_an_in_root_profile_directory_symlink() {
        use std::os::unix::fs::symlink;

        let f = Fixture::new("internal-ancestor");
        let actual = f.dir.join("real/usr/share/nawa/system-profile.toml");
        write(&actual, DESKTOP);
        symlink(f.dir.join("real/usr"), f.dir.join("usr")).unwrap();

        let effective = load_effective(&f.dir).unwrap();
        assert_eq!(
            effective
                .profile
                .as_ref()
                .and_then(|profile| profile.provider("init-system")),
            Some("systemd")
        );
        assert!(
            effective.role_sources["init-system"].starts_with(&f.dir),
            "reported provenance must remain inside the selected root"
        );
    }

    #[test]
    fn schema_two_policy_tables_load_with_domain_source_provenance() {
        let f = Fixture::new("schema-two-policy");
        write(
            &f.defaults,
            "schema = 2\n\n[policy.init-system]\nrequired = [\"systemd\"]\nforbidden = [\"runit\"]\n\n[policy.network-stack]\npreferred = [\"networkmanager\", \"connman\"]\n",
        );
        write(
            &f.overrides,
            "schema = 2\n\n[policy.init-system]\npreferred = [\"runit\"]\n",
        );

        let effective = load_effective_at(&f.defaults, &f.overrides).unwrap();
        let profile = effective.profile.as_ref().unwrap();
        assert_eq!(profile.provider("init-system"), None);
        assert_eq!(
            profile.provider_policy("init-system").unwrap().preferred,
            vec!["runit".to_string()]
        );
        assert_eq!(
            profile.provider_policy("network-stack").unwrap().preferred,
            vec!["networkmanager".to_string(), "connman".to_string()]
        );
        assert_eq!(
            effective
                .policy_sources
                .get("init-system")
                .map(|p| p.as_path()),
            Some(f.overrides.as_path())
        );
        assert_eq!(
            effective
                .policy_sources
                .get("network-stack")
                .map(|p| p.as_path()),
            Some(f.defaults.as_path())
        );
    }

    #[test]
    fn an_empty_admin_policy_table_clears_distribution_constraints() {
        let f = Fixture::new("clear-policy");
        write(
            &f.defaults,
            "schema = 2\n\n[policy.init-system]\nrequired = [\"systemd\"]\n",
        );
        write(&f.overrides, "schema = 2\n\n[policy.init-system]\n");
        let effective = load_effective_at(&f.defaults, &f.overrides).unwrap();
        let policy = effective
            .profile
            .as_ref()
            .unwrap()
            .provider_policy("init-system")
            .unwrap();
        assert!(policy.required.is_empty());
        assert_eq!(effective.policy_sources["init-system"], f.overrides);
    }

    #[test]
    fn policy_lists_reject_non_string_values_and_unknown_fields() {
        let f = Fixture::new("bad-policy-fields");
        write(
            &f.defaults,
            "schema = 2\n\n[policy.init-system]\npreferred = [1, \"runit\"]\n",
        );
        let error = load_effective_at(&f.defaults, &f.overrides).unwrap_err();
        assert!(format!("{error:#}").contains("must be a string"));

        write(
            &f.defaults,
            "schema = 2\n\n[policy.init-system]\nchoose = [\"systemd\"]\n",
        );
        let error = load_effective_at(&f.defaults, &f.overrides).unwrap_err();
        assert!(format!("{error:#}").contains("unknown provider policy field"));
    }

    #[test]
    fn schema_one_rejects_policy_tables_and_old_provider_profiles_still_load() {
        let f = Fixture::new("schema-one-policy");
        write(
            &f.defaults,
            "schema = 1\n\n[policy.init-system]\nrequired = [\"systemd\"]\n",
        );
        let error = load_effective_at(&f.defaults, &f.overrides).unwrap_err();
        assert!(format!("{error:#}").contains("does not support provider policy tables"));

        write(&f.defaults, DESKTOP);
        let effective = load_effective_at(&f.defaults, &f.overrides).unwrap();
        assert_eq!(
            effective
                .profile
                .as_ref()
                .and_then(|p| p.provider("init-system")),
            Some("systemd")
        );
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
        write(&f.overrides, "schema = 3\n");
        let e = load_effective_at(&f.defaults, &f.overrides).unwrap_err();
        let msg = format!("{e:#}");
        assert!(msg.contains("unknown profile schema 3"), "{msg}");
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
            distribution
                .role_sources
                .get("init-system")
                .map(|p| p.as_path()),
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
            effective
                .role_sources
                .get("init-system")
                .map(|p| p.as_path()),
            Some(overrides.as_path())
        );
    }
}
