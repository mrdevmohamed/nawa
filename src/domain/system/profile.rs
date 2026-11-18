//! The system profile (design §7): a distribution's *intended composition*
//! — never a snapshot of what is installed, and never the build profile of
//! `src/profile.rs` (`--profiles`, compiler flags for `profiles/<arch>.toml`;
//! separate concepts, separate models and separate paths by design).
//!
//! The design distinguishes four things (§7): distribution defaults shipped
//! and versioned by the distribution, local administrator overrides stored
//! separately (never silently overwritten by a package update), the
//! *effective target profile* produced by merging defaults with valid
//! overrides, and the observed runtime state reported independently by the
//! detector (the `application/features/system/detect.rs` use case). This module
//! owns the profile values and the merge; the loader lives with the
//! application layer (`features/system/profile.rs`) because the exact file
//! locations are the open Faseela contract (blocker B4) and are injected.
//!
//! Fail-closed rules (§7): an invalid override fails closed — it is an
//! error, never silently ignored in favor of the defaults — and an unknown
//! schema is an error, never a guess. Strictness follows the B3 reasoning
//! (`domain/package/metadata.rs`): unknown fields are a parse error, so a
//! newer profile document is loudly rejected instead of silently misread.
//!
//! Illustrative profile (§7 — a discussion example, not a promise that any
//! listed provider is currently supported):
//!
//! ```toml
//! schema = 1
//! distribution = "faseela"
//! profile = "desktop"
//! architecture = "x86_64"
//!
//! [providers]
//! init-system = "systemd"
//! ```
//!
//! The normalized input arrives as already-parsed maps (the application
//! layer converts the file text); this module validates and normalizes. No
//! I/O, no adapter types.


use std::collections::BTreeMap;

use crate::domain::system::capability::Capability;

/// The one schema version this nawa understands (§7's example).
pub(crate) const SCHEMA: u32 = 1;

/// A validated system profile (§7). `providers` maps a capability role to
/// the provider identity the profile selects (e.g. `init-system` →
/// `systemd`); the map is ordered for deterministic merges and reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SystemProfile {
    pub(crate) schema: u32,
    pub(crate) distribution: Option<String>,
    pub(crate) profile: Option<String>,
    pub(crate) architecture: Option<String>,
    pub(crate) providers: BTreeMap<String, String>,
}

/// The `[providers]` section as the loader saw it: absent, or present
/// (possibly empty). The distinction matters for the merge — an empty
/// section overrides nothing, an absent section contributes nothing.
pub(crate) type ProvidersDoc = Option<BTreeMap<String, String>>;

impl SystemProfile {
    /// Validate and normalize one profile document. `scalars` holds every
    /// top-level scalar key of the document (`schema`, `distribution`,
    /// `profile`, `architecture` — anything else is rejected), `providers`
    /// the optional `[providers]` section.
    ///
    /// Normalization: scalar values and provider roles/identities are
    /// trimmed; roles and identities are lowercased (§6.1 normalization, as
    /// [`Capability::parse`] applies). An optional scalar left empty
    /// becomes absent. Provider values (identities) must be non-empty and
    /// whitespace-free.
    pub(crate) fn from_doc(
        scalars: &BTreeMap<String, String>,
        providers: &ProvidersDoc,
    ) -> Result<SystemProfile, String> {
        for key in scalars.keys() {
            if !matches!(
                key.as_str(),
                "schema" | "distribution" | "profile" | "architecture"
            ) {
                return Err(format!("unknown profile key `{key}`"));
            }
        }
        let schema_raw = scalars
            .get("schema")
            .ok_or("profile has no `schema` key: the schema version must be declared")?;
        let schema: u32 = schema_raw
            .trim()
            .parse()
            .map_err(|_| format!("profile `schema` is not a number: {schema_raw:?}"))?;
        if schema != SCHEMA {
            return Err(format!(
                "unknown profile schema {schema}: this nawa understands schema {SCHEMA}"
            ));
        }
        let optional = |key: &str| -> Option<String> {
            scalars
                .get(key)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };
        let mut provider_map = BTreeMap::new();
        if let Some(section) = providers {
            for (role, identity) in section {
                let role = normalize_role(role)?;
                let identity = identity.trim().to_lowercase();
                if identity.is_empty() || identity.split_whitespace().count() > 1 {
                    return Err(format!(
                        "profile provider `{role}` has an empty or whitespace identity"
                    ));
                }
                provider_map.insert(role, identity);
            }
        }
        Ok(SystemProfile {
            schema,
            distribution: optional("distribution"),
            profile: optional("profile"),
            architecture: optional("architecture"),
            providers: provider_map,
        })
    }

    /// The identity the profile selects for a capability role, if any.
    pub(crate) fn provider(&self, role: &str) -> Option<&str> {
        self.providers.get(role).map(|s| s.as_str())
    }

    /// The selected init provider as a normalized capability (§6.1), the
    /// value the solver compares against candidate declarations
    /// ([`crate::domain::system::capability::InitProvider`]). The caller
    /// still checks the identity against the migration's exact identities
    /// ([`crate::domain::system::identity::InitKind::from_capability`]).
    pub(crate) fn init_capability(&self) -> Option<Capability> {
        self.provider("init-system").map(|identity| Capability {
            domain: "init-system".to_string(),
            identity: identity.to_string(),
        })
    }

    /// The effective target profile (§7): defaults merged with valid
    /// overrides. Administrator overrides take precedence; provider roles
    /// are overlaid per role (an override replaces or adds a role; roles
    /// only present in the defaults survive — an override cannot remove a
    /// role). Either side may be absent; an override without defaults is a
    /// valid profile of its own.
    pub(crate) fn merge(
        defaults: Option<&SystemProfile>,
        overrides: Option<&SystemProfile>,
    ) -> Option<SystemProfile> {
        match (defaults, overrides) {
            (None, None) => None,
            (Some(d), None) => Some(d.clone()),
            (None, Some(o)) => Some(o.clone()),
            (Some(d), Some(o)) => Some(SystemProfile {
                // both sides were validated against the same schema version
                schema: d.schema,
                distribution: o.distribution.clone().or_else(|| d.distribution.clone()),
                profile: o.profile.clone().or_else(|| d.profile.clone()),
                architecture: o.architecture.clone().or_else(|| d.architecture.clone()),
                providers: {
                    let mut merged = d.providers.clone();
                    merged.extend(o.providers.iter().map(|(r, i)| (r.clone(), i.clone())));
                    merged
                },
            }),
        }
    }
}

/// Normalize a provider role like [`Capability::parse`] normalizes a
/// capability domain: trimmed, lowercased, non-empty, no internal
/// whitespace.
fn normalize_role(role: &str) -> Result<String, String> {
    let role = role.trim().to_lowercase();
    if role.is_empty() {
        return Err("profile has an empty provider role".to_string());
    }
    if role.split_whitespace().count() > 1 {
        return Err(format!(
            "profile provider role {role:?} contains whitespace"
        ));
    }
    Ok(role)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn providers(pairs: &[(&str, &str)]) -> ProvidersDoc {
        Some(doc(pairs))
    }

    /// The §7 illustrative profile, normalized.
    fn desktop() -> SystemProfile {
        SystemProfile::from_doc(
            &doc(&[
                ("schema", "1"),
                ("distribution", "faseela"),
                ("profile", "desktop"),
                ("architecture", "x86_64"),
            ]),
            &providers(&[("init-system", "systemd")]),
        )
        .unwrap()
    }

    #[test]
    fn the_illustrative_profile_parses() {
        let p = desktop();
        assert_eq!(p.schema, 1);
        assert_eq!(p.distribution.as_deref(), Some("faseela"));
        assert_eq!(p.profile.as_deref(), Some("desktop"));
        assert_eq!(p.architecture.as_deref(), Some("x86_64"));
        assert_eq!(p.provider("init-system"), Some("systemd"));
    }

    #[test]
    fn init_capability_is_the_normalized_selector() {
        let c = desktop().init_capability().unwrap();
        assert_eq!(c.to_key(), "init-system=systemd");
        assert!(c.is_init_system());
        let no_init = SystemProfile::from_doc(&doc(&[("schema", "1")]), &None).unwrap();
        assert!(no_init.init_capability().is_none());
    }

    #[test]
    fn schema_is_required_and_pinned_to_one() {
        assert!(SystemProfile::from_doc(&doc(&[]), &None)
            .unwrap_err()
            .contains("no `schema` key"));
        for bad in ["2", "0", "x"] {
            let e = SystemProfile::from_doc(&doc(&[("schema", bad)]), &None).unwrap_err();
            assert!(e.contains("schema"), "schema {bad:?}: {e}");
        }
        let ok = SystemProfile::from_doc(&doc(&[("schema", "1")]), &None).unwrap();
        assert_eq!(ok.schema, 1);
    }

    #[test]
    fn unknown_keys_are_rejected_not_ignored() {
        let e = SystemProfile::from_doc(&doc(&[("schema", "1"), ("maintainer", "someone")]), &None)
            .unwrap_err();
        assert_eq!(e, "unknown profile key `maintainer`");
    }

    #[test]
    fn scalars_and_roles_normalize_case_whitespace_and_emptiness() {
        let p = SystemProfile::from_doc(
            &doc(&[
                ("schema", " 1 "),
                ("distribution", " Faseela "),
                ("profile", ""),
            ]),
            &providers(&[("  INIT-System ", " SystemD ")]),
        )
        .unwrap();
        assert_eq!(p.schema, 1);
        assert_eq!(p.distribution.as_deref(), Some("Faseela"));
        assert_eq!(p.profile, None, "an empty optional scalar is absent");
        assert_eq!(p.provider("init-system"), Some("systemd"));
    }

    #[test]
    fn provider_declarations_are_strict() {
        for (role, identity) in [
            ("", "systemd"),
            ("init system", "systemd"),
            ("init-system", ""),
            ("init-system", "sys temd"),
        ] {
            let e =
                SystemProfile::from_doc(&doc(&[("schema", "1")]), &providers(&[(role, identity)]))
                    .unwrap_err();
            assert!(!e.is_empty(), "{role:?}={identity:?} should not parse");
        }
    }

    #[test]
    fn an_empty_providers_section_is_present_but_overrides_nothing() {
        let p = SystemProfile::from_doc(&doc(&[("schema", "1")]), &Some(BTreeMap::new())).unwrap();
        assert!(p.providers.is_empty());
        assert!(p.init_capability().is_none());
    }

    #[test]
    fn merge_absent_inputs_yield_no_effective_profile() {
        assert_eq!(SystemProfile::merge(None, None), None);
    }

    #[test]
    fn a_single_side_is_the_effective_profile() {
        let d = desktop();
        assert_eq!(SystemProfile::merge(Some(&d), None), Some(d.clone()));
        assert_eq!(SystemProfile::merge(None, Some(&d)), Some(d));
    }

    #[test]
    fn overrides_take_precedence_field_wise_and_roles_overlay() {
        let base = SystemProfile::from_doc(
            &doc(&[
                ("schema", "1"),
                ("distribution", "faseela"),
                ("profile", "server"),
                ("architecture", "x86_64"),
            ]),
            &providers(&[("init-system", "runit"), ("session-manager", "elogind")]),
        )
        .unwrap();
        let over = SystemProfile::from_doc(
            &doc(&[("schema", "1"), ("profile", "desktop")]),
            &providers(&[("init-system", "systemd")]),
        )
        .unwrap();
        let eff = SystemProfile::merge(Some(&base), Some(&over)).unwrap();
        // override wins where it speaks
        assert_eq!(eff.profile.as_deref(), Some("desktop"));
        assert_eq!(eff.provider("init-system"), Some("systemd"));
        // defaults survive where the override is silent
        assert_eq!(eff.distribution.as_deref(), Some("faseela"));
        assert_eq!(eff.architecture.as_deref(), Some("x86_64"));
        assert_eq!(eff.provider("session-manager"), Some("elogind"));
    }

    #[test]
    fn merges_are_deterministic_for_identical_inputs() {
        let a = SystemProfile::merge(Some(&desktop()), Some(&desktop()));
        let b = SystemProfile::merge(Some(&desktop()), Some(&desktop()));
        assert_eq!(a, b);
    }
}
