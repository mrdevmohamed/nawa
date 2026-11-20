//! Capability identities and normalized provider declarations (design §6).
//!
//! Package names are implementation details; system requirements are
//! expressed as normalized capabilities (§6). The model accepts arbitrary
//! domains so the shared planner can reason about declarations generically.
//! The first actionable provider domain is `init-system`, with the exact
//! supported identities `runit` and `systemd`. Other domains remain
//! descriptive metadata until their policy, compatibility and execution
//! contracts are separately specified.
//!
//! For critical roles such as init-system, a migration must require
//! explicit, *authenticated* provider metadata (§6.3): a local unsigned
//! archive or sidecar must not provide trusted init capability metadata.
//! This module models the declaration itself; authentication is enforced
//! where archives and index entries are verified.
//!
//! The metadata chain is wired: recipe `[capabilities]` declarations are
//! normalized into archive `.PKGINFO` and authenticated in signed repository
//! index schema v4. The resolver keeps legacy v1-v3 metadata explicitly
//! unknown; it never infers capabilities from package names or file presence.
//!
//! Recipe spelling:
//!
//! ```toml
//! [capabilities]
//! provides = ["init-system=runit"]
//! ```

/// One normalized capability identity: `domain=identity` (§6.1, e.g.
/// `init-system=runit`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Capability {
    /// the capability domain, e.g. `init-system`
    pub(crate) domain: String,
    /// the identity within the domain, e.g. `runit`
    pub(crate) identity: String,
}

impl Capability {
    /// Parse one `domain=identity` declaration. Normalization: surrounding
    /// whitespace is trimmed, domain and identity are lowercased, and
    /// exactly one `=` separates two non-empty parts with no internal
    /// whitespace. Unknown domains parse — the resolver decides which
    /// domains are in scope (§6: only `init-system` for now).
    pub(crate) fn parse(raw: &str) -> Result<Capability, String> {
        let s = raw.trim();
        let (domain, identity) = s
            .split_once('=')
            .ok_or_else(|| format!("capability {raw:?} is not `domain=identity`"))?;
        if domain.is_empty() || identity.is_empty() {
            return Err(format!(
                "capability {raw:?} has an empty domain or identity"
            ));
        }
        let (domain, identity) = (domain.trim().to_lowercase(), identity.trim().to_lowercase());
        if domain.split_whitespace().count() > 1 {
            return Err(format!(
                "capability {raw:?} contains whitespace in the domain"
            ));
        }
        if identity.split_whitespace().count() > 1 {
            return Err(format!(
                "capability {raw:?} contains whitespace in the identity"
            ));
        }
        if identity.contains('=') {
            return Err(format!("capability {raw:?} has more than one `=`"));
        }
        Ok(Capability { domain, identity })
    }

    /// The normalized string form.
    pub(crate) fn to_key(&self) -> String {
        format!("{}={}", self.domain, self.identity)
    }

    /// True for the first actionable provider domain (§6).
    pub(crate) fn is_init_system(&self) -> bool {
        self.domain == "init-system"
    }
}

/// The provider declaration as carried by metadata (§6.3(5)): a missing
/// field in legacy metadata is [`Provides::Unknown`], **not** an empty
/// list — the normalized model preserves the distinction, so a solver can
/// report `unknown` instead of claiming "declares no capabilities".
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Provides {
    /// the field is present: possibly empty; entries normalized in
    /// declaration order
    Declared(Vec<Capability>),
    /// legacy metadata without the field. Capabilities are never inferred
    /// from package names or file presence (§6.3(4)).
    Unknown,
}

impl Provides {
    /// Parse already-split raw `provides` entries (e.g. a TOML string list).
    /// Strict: the first malformed entry is an error, not a silent skip.
    pub(crate) fn parse_all(raw: &[String]) -> Result<Provides, String> {
        let mut caps = Vec::with_capacity(raw.len());
        for entry in raw {
            caps.push(Capability::parse(entry)?);
        }
        Ok(Provides::Declared(caps))
    }

    /// Every declared `init-system` capability, in declaration order. Empty
    /// for [`Provides::Unknown`] — and distinct from it (match on the
    /// variant to tell "declares none" from "unknown").
    pub(crate) fn init_identities(&self) -> Vec<&Capability> {
        match self {
            Provides::Declared(caps) => caps.iter().filter(|c| c.is_init_system()).collect(),
            Provides::Unknown => Vec::new(),
        }
    }

    /// The declared provider for the exclusive init-system role (§6.2):
    /// exactly one declaration → [`InitProvider::Single`]; several →
    /// [`InitProvider::Ambiguous`], which must be reported, never resolved
    /// arbitrarily; none (or unknown metadata) → [`InitProvider::Absent`].
    pub(crate) fn init_provider(&self) -> InitProvider<'_> {
        let caps = self.init_identities();
        match caps.len() {
            0 => InitProvider::Absent,
            1 => InitProvider::Single(caps[0]),
            _ => InitProvider::Ambiguous(caps),
        }
    }
}

/// The declared init-system provider for the exclusive role (§6.2). A
/// [`Capability`] here is a *candidate* — it is not the active provider,
/// and the caller still checks the identity against this migration's two
/// exact identities (`InitKind::from_capability`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum InitProvider<'a> {
    Single(&'a Capability),
    /// several init-system identities are declared: report, do not choose
    Ambiguous(Vec<&'a Capability>),
    /// none declared, or the metadata has no capability field
    Absent,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_normalizes_case_and_surrounding_whitespace() {
        let cap = Capability::parse("  init-system=Runit ").unwrap();
        assert_eq!(cap.domain, "init-system");
        assert_eq!(cap.identity, "runit");
        assert_eq!(cap.to_key(), "init-system=runit");
        assert!(cap.is_init_system());
    }

    #[test]
    fn parse_rejects_malformed_declarations() {
        for bad in [
            "noequals",
            "=runit",
            "init-system=",
            "",
            "   ",
            "init system=runit",
            "init-system=run it",
            "a=b=c",
        ] {
            assert!(Capability::parse(bad).is_err(), "{bad:?} should not parse");
        }
    }

    #[test]
    fn unknown_domains_parse_and_are_not_init_system() {
        let cap = Capability::parse("session-manager=foo").unwrap();
        assert_eq!(cap.to_key(), "session-manager=foo");
        assert!(!cap.is_init_system());
    }

    #[test]
    fn parse_all_normalizes_and_preserves_declaration_order() {
        let raw = vec!["init-system=runit".to_string(), "Session=Foo".to_string()];
        let caps = match Provides::parse_all(&raw).unwrap() {
            Provides::Declared(caps) => caps,
            other => panic!("expected Declared, got {other:?}"),
        };
        assert_eq!(caps[0].to_key(), "init-system=runit");
        assert_eq!(caps[1].to_key(), "session=foo");
    }

    #[test]
    fn parse_all_is_strict_about_the_first_malformed_entry() {
        let raw = vec!["init-system=runit".to_string(), "broken".to_string()];
        assert!(Provides::parse_all(&raw).is_err());
    }

    #[test]
    fn unknown_is_distinct_from_an_empty_declaration() {
        // §6.3(5): a missing field is unknown, not "declares nothing"
        assert_ne!(Provides::Unknown, Provides::Declared(Vec::new()));
        assert!(Provides::Unknown.init_identities().is_empty());
        assert!(Provides::Declared(Vec::new()).init_identities().is_empty());
    }

    #[test]
    fn init_identities_filter_the_declared_init_system_entries() {
        let caps = Provides::parse_all(&[
            "session=foo".to_string(),
            "init-system=runit".to_string(),
            "init-system=systemd".to_string(),
        ])
        .unwrap();
        let inits = caps.init_identities();
        assert_eq!(inits.len(), 2);
        assert_eq!(inits[0].identity, "runit");
        assert_eq!(inits[1].identity, "systemd");
    }

    #[test]
    fn init_provider_selection_reports_ambiguity_instead_of_choosing() {
        let single = Provides::parse_all(&["init-system=runit".to_string()]).unwrap();
        assert_eq!(
            single.init_provider(),
            InitProvider::Single(&Capability {
                domain: "init-system".to_string(),
                identity: "runit".to_string(),
            })
        );

        // two providers of an exclusive role: reported, never chosen (§6.2)
        let both = Provides::parse_all(&[
            "init-system=runit".to_string(),
            "init-system=systemd".to_string(),
        ])
        .unwrap();
        match both.init_provider() {
            InitProvider::Ambiguous(caps) => assert_eq!(caps.len(), 2),
            other => panic!("expected Ambiguous, got {other:?}"),
        }

        for none in [
            Provides::Unknown,
            Provides::Declared(Vec::new()),
            Provides::parse_all(&["session=foo".to_string()]).unwrap(),
        ] {
            assert_eq!(none.init_provider(), InitProvider::Absent);
        }
    }
}
