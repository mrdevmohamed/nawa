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

/// Current profile schema. Schema 1 remains readable for compatibility.
pub(crate) const SCHEMA: u32 = 2;
const LEGACY_SCHEMA: u32 = 1;

/// A validated system profile (§7). `providers` maps a capability role to
/// the provider identity the profile selects (e.g. `init-system` →
/// `systemd`); the map is ordered for deterministic merges and reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SystemProfile {
    pub(crate) schema: u32,
    pub(crate) distribution: Option<String>,
    pub(crate) profile: Option<String>,
    pub(crate) architecture: Option<String>,
    /// Projection of exactly-one required provider policies.
    pub(crate) providers: BTreeMap<String, String>,
    /// Full constraints, grouped by normalized capability domain.
    pub(crate) policies: BTreeMap<String, ProviderPolicy>,
}

/// A normalized provider policy. Preferred identities retain declaration
/// order; forbidden identities never imply a replacement provider.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct ProviderPolicy {
    pub(crate) required: Vec<String>,
    pub(crate) preferred: Vec<String>,
    pub(crate) forbidden: Vec<String>,
}

/// Raw values from one [policy.<role>] profile table.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct ProviderPolicyDoc {
    pub(crate) required: Vec<String>,
    pub(crate) preferred: Vec<String>,
    pub(crate) forbidden: Vec<String>,
}

/// A present role table replaces that role's complete distribution policy.
/// An empty table explicitly clears inherited constraints.
pub(crate) type ProviderPoliciesDoc = Option<BTreeMap<String, ProviderPolicyDoc>>;

/// Legacy [providers] values are interpreted as required-provider shorthand.
pub(crate) type ProvidersDoc = Option<BTreeMap<String, String>>;

impl SystemProfile {
    /// Validate a legacy schema-1 profile or schema-2 policy profile.
    /// Kept as a shorthand for domain tests that supply only [providers].
    #[cfg(test)]
    pub(crate) fn from_doc(
        scalars: &BTreeMap<String, String>,
        providers: &ProvidersDoc,
    ) -> Result<SystemProfile, String> {
        Self::from_doc_with_policies(scalars, providers, &None)
    }

    pub(crate) fn from_doc_with_policies(
        scalars: &BTreeMap<String, String>,
        providers: &ProvidersDoc,
        policies: &ProviderPoliciesDoc,
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
            .map_err(|_| format!("profile schema is not a number: {schema_raw:?}"))?;
        if schema != LEGACY_SCHEMA && schema != SCHEMA {
            return Err(format!(
                "unknown profile schema {schema}: this nawa understands schemas {LEGACY_SCHEMA} and {SCHEMA}"
            ));
        }
        if schema == LEGACY_SCHEMA && policies.is_some() {
            return Err(format!(
                "profile schema {LEGACY_SCHEMA} does not support provider policy tables; use schema {SCHEMA}"
            ));
        }
        let optional = |key: &str| -> Option<String> {
            scalars
                .get(key)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };

        let mut normalized = BTreeMap::new();
        if let Some(section) = policies {
            for (raw_role, raw_policy) in section {
                let role = normalize_role(raw_role)?;
                let policy = normalize_policy(&role, raw_policy)?;
                if normalized.insert(role.clone(), policy).is_some() {
                    return Err(format!("provider policy domain {role:?} is declared more than once after normalization"));
                }
            }
        }
        if let Some(section) = providers {
            for (raw_role, raw_identity) in section {
                let role = normalize_role(raw_role)?;
                if normalized.contains_key(&role) {
                    return Err(format!(
                        "profile domain {role:?} is declared in both providers and policy"
                    ));
                }
                let identity = normalize_identity(&role, raw_identity, "required")?;
                normalized.insert(
                    role,
                    ProviderPolicy {
                        required: vec![identity],
                        preferred: Vec::new(),
                        forbidden: Vec::new(),
                    },
                );
            }
        }

        let provider_map = providers_from_policies(&normalized);
        Ok(SystemProfile {
            schema,
            distribution: optional("distribution"),
            profile: optional("profile"),
            architecture: optional("architecture"),
            providers: provider_map,
            policies: normalized,
        })
    }

    /// The identity the profile selects for a capability role, if any.
    pub(crate) fn provider(&self, role: &str) -> Option<&str> {
        self.providers.get(role).map(|s| s.as_str())
    }

    /// Policy for a capability domain, when declared.
    pub(crate) fn provider_policy(&self, role: &str) -> Option<&ProviderPolicy> {
        self.policies.get(role)
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
                schema: d.schema.max(o.schema),
                distribution: o.distribution.clone().or_else(|| d.distribution.clone()),
                profile: o.profile.clone().or_else(|| d.profile.clone()),
                architecture: o.architecture.clone().or_else(|| d.architecture.clone()),
                policies: {
                    let mut merged = d.policies.clone();
                    // Administrator policy replaces the whole domain. An
                    // empty table intentionally clears distribution rules.
                    merged.extend(o.policies.iter().map(|(r, p)| (r.clone(), p.clone())));
                    merged
                },
                providers: providers_from_policies(&{
                    let mut merged = d.policies.clone();
                    merged.extend(o.policies.iter().map(|(r, p)| (r.clone(), p.clone())));
                    merged
                }),
            }),
        }
    }
}

fn normalize_policy(role: &str, raw: &ProviderPolicyDoc) -> Result<ProviderPolicy, String> {
    let required = normalize_identities(role, &raw.required, "required")?;
    let preferred = normalize_identities(role, &raw.preferred, "preferred")?;
    let forbidden = normalize_identities(role, &raw.forbidden, "forbidden")?;
    let mut classes = BTreeMap::<String, &'static str>::new();
    for (kind, identities) in [
        ("required", &required),
        ("preferred", &preferred),
        ("forbidden", &forbidden),
    ] {
        for identity in identities {
            if let Some(previous) = classes.insert(identity.clone(), kind) {
                return Err(format!(
                    "provider policy for {role}={identity} is contradictory: both {previous} and {kind}"
                ));
            }
        }
    }
    if role == "init-system" && required.len() > 1 {
        return Err(
            "the exclusive init-system role may have at most one required provider".to_string(),
        );
    }
    if !required.is_empty() && !preferred.is_empty() {
        return Err(format!(
            "provider policy for {role} cannot combine required and preferred providers"
        ));
    }
    Ok(ProviderPolicy {
        required,
        preferred,
        forbidden,
    })
}

fn normalize_identities(role: &str, raw: &[String], kind: &str) -> Result<Vec<String>, String> {
    let mut values = Vec::with_capacity(raw.len());
    let mut seen = std::collections::BTreeSet::new();
    for identity in raw {
        let normalized = normalize_identity(role, identity, kind)?;
        if !seen.insert(normalized.clone()) {
            return Err(format!(
                "provider policy for {role} repeats {kind} identity {normalized}"
            ));
        }
        values.push(normalized);
    }
    Ok(values)
}

fn normalize_identity(role: &str, identity: &str, kind: &str) -> Result<String, String> {
    Capability::parse(&format!("{role}={identity}"))
        .map(|capability| capability.identity)
        .map_err(|error| format!("invalid {kind} provider for {role}: {error}"))
}

fn providers_from_policies(
    policies: &BTreeMap<String, ProviderPolicy>,
) -> BTreeMap<String, String> {
    policies
        .iter()
        .filter(|(_, policy)| policy.required.len() == 1)
        .map(|(role, policy)| (role.clone(), policy.required[0].clone()))
        .collect()
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
        for bad in ["0", "3", "x"] {
            let e = SystemProfile::from_doc(&doc(&[("schema", bad)]), &None).unwrap_err();
            assert!(e.contains("schema"), "schema {bad:?}: {e}");
        }
        let legacy = SystemProfile::from_doc(&doc(&[("schema", "1")]), &None).unwrap();
        assert_eq!(legacy.schema, 1);
        let current = SystemProfile::from_doc(&doc(&[("schema", "2")]), &None).unwrap();
        assert_eq!(current.schema, SCHEMA);
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
    fn schema_two_accepts_ordered_preferred_and_forbidden_provider_policy() {
        let policy = ProviderPolicyDoc {
            required: Vec::new(),
            preferred: vec!["systemd".into(), "runit".into()],
            forbidden: vec!["busybox".into()],
        };
        let document = BTreeMap::from([("init-system".to_string(), policy)]);
        let profile =
            SystemProfile::from_doc_with_policies(&doc(&[("schema", "2")]), &None, &Some(document))
                .unwrap();
        assert_eq!(profile.provider("init-system"), None);
        let policy = profile.provider_policy("init-system").unwrap();
        assert_eq!(
            policy.preferred,
            vec!["systemd".to_string(), "runit".to_string()]
        );
        assert_eq!(policy.forbidden, vec!["busybox".to_string()]);
        assert!(profile.init_capability().is_none());
    }

    #[test]
    fn legacy_provider_shorthand_becomes_a_required_policy() {
        let profile = SystemProfile::from_doc(
            &doc(&[("schema", "1")]),
            &providers(&[("INIT-System", "SystemD")]),
        )
        .unwrap();
        assert_eq!(profile.provider("init-system"), Some("systemd"));
        assert_eq!(
            profile.provider_policy("init-system").unwrap().required,
            vec!["systemd".to_string()]
        );
    }

    #[test]
    fn provider_policy_rejects_duplicates_and_contradictory_rules() {
        let bad = [
            ProviderPolicyDoc {
                required: vec!["systemd".into(), "SYSTEMD".into()],
                ..ProviderPolicyDoc::default()
            },
            ProviderPolicyDoc {
                required: vec!["systemd".into()],
                forbidden: vec!["SYSTEMD".into()],
                ..ProviderPolicyDoc::default()
            },
            ProviderPolicyDoc {
                preferred: vec!["runit".into()],
                forbidden: vec!["runit".into()],
                ..ProviderPolicyDoc::default()
            },
            ProviderPolicyDoc {
                required: vec!["systemd".into()],
                preferred: vec!["runit".into()],
                ..ProviderPolicyDoc::default()
            },
            ProviderPolicyDoc {
                required: vec!["systemd".into(), "runit".into()],
                ..ProviderPolicyDoc::default()
            },
        ];
        for policy in bad {
            let document = BTreeMap::from([("init-system".to_string(), policy)]);
            let error = SystemProfile::from_doc_with_policies(
                &doc(&[("schema", "2")]),
                &None,
                &Some(document),
            )
            .unwrap_err();
            assert!(!error.is_empty());
        }
    }

    #[test]
    fn schema_one_rejects_policy_tables_but_remains_backward_compatible() {
        let document = BTreeMap::from([(
            "init-system".to_string(),
            ProviderPolicyDoc {
                required: vec!["systemd".into()],
                ..ProviderPolicyDoc::default()
            },
        )]);
        let error =
            SystemProfile::from_doc_with_policies(&doc(&[("schema", "1")]), &None, &Some(document))
                .unwrap_err();
        assert!(error.contains("does not support provider policy tables"));
        let legacy = SystemProfile::from_doc(
            &doc(&[("schema", "1")]),
            &providers(&[("init-system", "systemd")]),
        )
        .unwrap();
        assert_eq!(legacy.provider("init-system"), Some("systemd"));
    }

    #[test]
    fn administrator_policy_replaces_one_domain_and_empty_tables_clear_it() {
        let defaults = SystemProfile::from_doc_with_policies(
            &doc(&[("schema", "2")]),
            &None,
            &Some(BTreeMap::from([
                (
                    "init-system".to_string(),
                    ProviderPolicyDoc {
                        required: vec!["systemd".into()],
                        forbidden: vec!["runit".into()],
                        ..ProviderPolicyDoc::default()
                    },
                ),
                (
                    "network-stack".to_string(),
                    ProviderPolicyDoc {
                        preferred: vec!["networkmanager".into()],
                        ..ProviderPolicyDoc::default()
                    },
                ),
            ])),
        )
        .unwrap();
        let admin_override = SystemProfile::from_doc_with_policies(
            &doc(&[("schema", "2")]),
            &None,
            &Some(BTreeMap::from([(
                "init-system".to_string(),
                ProviderPolicyDoc {
                    preferred: vec!["runit".into()],
                    ..ProviderPolicyDoc::default()
                },
            )])),
        )
        .unwrap();
        let effective = SystemProfile::merge(Some(&defaults), Some(&admin_override)).unwrap();
        assert_eq!(effective.provider("init-system"), None);
        assert_eq!(
            effective.provider_policy("init-system").unwrap().preferred,
            vec!["runit".to_string()]
        );
        assert_eq!(
            effective
                .provider_policy("network-stack")
                .unwrap()
                .preferred,
            vec!["networkmanager".to_string()]
        );

        let clear = SystemProfile::from_doc_with_policies(
            &doc(&[("schema", "2")]),
            &None,
            &Some(BTreeMap::from([(
                "init-system".to_string(),
                ProviderPolicyDoc::default(),
            )])),
        )
        .unwrap();
        let cleared = SystemProfile::merge(Some(&defaults), Some(&clear)).unwrap();
        let policy = cleared.provider_policy("init-system").unwrap();
        assert!(
            policy.required.is_empty()
                && policy.preferred.is_empty()
                && policy.forbidden.is_empty()
        );
        assert_eq!(cleared.provider("init-system"), None);
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
