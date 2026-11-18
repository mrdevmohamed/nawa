//! Init-system identity as a closed domain type (design §5, §6).
//!
//! Nawa observes init identity only (§5): it never inspects or manages
//! service enablement or runtime service state, and it never infers
//! `init-system` from package names or file presence (§6.3(4)).


/// The init-system identities in scope. The migration design limits provider
/// selection to the exact `init-system=runit` and `init-system=systemd`
/// identities (§6); a positively identified init outside that pair is
/// reported as [`InitKind::Other`] rather than guessed into either.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum InitKind {
    Systemd,
    Runit,
    /// a positively identified init outside the migration scope (e.g. a
    /// shell or busybox as PID 1); the string is the observed identity
    Other(String),
}

impl InitKind {
    /// The capability identity this kind provides (§6.1:
    /// `init-system=<identity>`).
    pub(crate) fn capability_identity(&self) -> String {
        match self {
            InitKind::Systemd => "systemd".to_string(),
            InitKind::Runit => "runit".to_string(),
            InitKind::Other(name) => name.clone(),
        }
    }

    /// The init identity a declared capability selects, for the one domain
    /// in scope (§6: only `init-system`, and only the two exact identities
    /// `runit`/`systemd`). A declared identity outside that pair — or a
    /// capability in another domain — selects nothing.
    pub(crate) fn from_capability(
        cap: &crate::domain::system::capability::Capability,
    ) -> Option<InitKind> {
        if !cap.is_init_system() {
            return None;
        }
        match cap.identity.as_str() {
            "runit" => Some(InitKind::Runit),
            "systemd" => Some(InitKind::Systemd),
            _ => None,
        }
    }
}

/// What the readable PID 1 evidence establishes. Unknown is a valid result
/// and is never silently replaced by a guess (§5.1); contradictions are
/// reported, never resolved by a preference (§5.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Pid1 {
    /// both readable sources agree on one identity
    Confirmed(InitKind),
    /// one source identifies the init, the other is unreadable or generic
    Indicated(InitKind),
    /// only generic evidence is visible (typically everything says `init`)
    Ambiguous { reason: String },
    /// the sources identify different inits — reported explicitly (§5.4)
    Contradictory { reason: String },
}

/// Classify PID 1 from its two read-only evidence strings. `exe` is the
/// `/proc/1/exe` link target, `comm` the `/proc/1/comm` value; `None` means
/// that source is unreadable.
pub(crate) fn classify_pid1(exe: Option<&str>, comm: Option<&str>) -> Pid1 {
    let from_exe = exe.and_then(identify);
    let from_comm = comm.and_then(identify);
    match (from_exe, from_comm) {
        (Some(kind), Some(other)) if kind == other => Pid1::Confirmed(kind),
        (Some(kind), Some(other)) => Pid1::Contradictory {
            reason: format!(
                "PID 1 evidence disagrees: /proc/1/exe is {:?} ({}), /proc/1/comm is {:?} ({})",
                exe.unwrap_or_default(),
                kind.capability_identity(),
                comm.unwrap_or_default(),
                other.capability_identity(),
            ),
        },
        (Some(kind), None) | (None, Some(kind)) => Pid1::Indicated(kind),
        (None, None) => Pid1::Ambiguous {
            reason: match (exe, comm) {
                (Some(e), Some(c)) => {
                    format!("PID 1 identifies only generically (exe={e:?}, comm={c:?})")
                }
                (Some(e), None) => {
                    format!("PID 1 identifies only generically (exe={e:?}, comm unreadable)")
                }
                (None, Some(c)) => {
                    format!("PID 1 identifies only generically (exe unreadable, comm={c:?})")
                }
                (None, None) => "no PID 1 identity evidence is readable".to_string(),
            },
        },
    }
}

/// What one observed name (a `/proc/1/exe` link target, a `/proc/1/comm`
/// value, or a target-root symlink target) identifies. `None` means the
/// name is generic — typically the bare `init` entry point — and
/// deliberately identifies nothing (§5.1: unknown is a valid result, not a
/// guess). An empty or whitespace-only name identifies nothing either.
pub(crate) fn identify(observed: &str) -> Option<InitKind> {
    if observed.trim().is_empty() {
        return None;
    }
    // Identify only the executable basename. Substring matching against
    // the full path can mistake an unrelated PID 1 such as
    // /opt/systemd-tools/supervisor for systemd, or a path containing
    // "runit" for runit. A generic init basename remains unknown even when
    // its parent directory happens to contain a provider name.
    let base = observed.rsplit('/').next().unwrap_or(observed);
    if base.is_empty() || base.eq_ignore_ascii_case("init") {
        return None;
    }
    if base.eq_ignore_ascii_case("systemd") {
        return Some(InitKind::Systemd);
    }
    if base.eq_ignore_ascii_case("runit") || base.eq_ignore_ascii_case("runit-init") {
        return Some(InitKind::Runit);
    }
    Some(InitKind::Other(base.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_confirms_when_exe_and_comm_agree() {
        assert_eq!(
            classify_pid1(Some("/usr/lib/systemd/systemd"), Some("systemd")),
            Pid1::Confirmed(InitKind::Systemd)
        );
        assert_eq!(
            classify_pid1(Some("/usr/bin/runit"), Some("runit")),
            Pid1::Confirmed(InitKind::Runit)
        );
    }

    #[test]
    fn classify_reports_a_contradiction_explicitly() {
        let result = classify_pid1(Some("/usr/lib/systemd/systemd"), Some("runit"));
        match result {
            Pid1::Contradictory { reason } => {
                assert!(reason.contains("disagrees"), "unexpected reason: {reason}");
                assert!(reason.contains("systemd") && reason.contains("runit"));
            }
            other => panic!("expected Contradictory, got {other:?}"),
        }
    }

    #[test]
    fn classify_indicates_from_one_identifying_source() {
        assert_eq!(
            classify_pid1(Some("/usr/lib/systemd/systemd"), None),
            Pid1::Indicated(InitKind::Systemd)
        );
        assert_eq!(
            classify_pid1(None, Some("runit")),
            Pid1::Indicated(InitKind::Runit)
        );
        // the generic `init` evidence does not contradict an identifying source
        assert_eq!(
            classify_pid1(Some("/usr/lib/systemd/systemd"), Some("init")),
            Pid1::Indicated(InitKind::Systemd)
        );
    }

    #[test]
    fn classify_is_ambiguous_when_only_generic_evidence_is_visible() {
        for (exe, comm) in [
            (Some("/sbin/init"), Some("init")),
            (Some("/sbin/init"), None),
            (None, Some("init")),
            (None, None),
        ] {
            match classify_pid1(exe, comm) {
                Pid1::Ambiguous { reason } => {
                    assert!(
                        reason.contains("generically") || reason.contains("no PID 1"),
                        "unexpected reason for {exe:?}/{comm:?}: {reason}"
                    );
                }
                other => panic!("expected Ambiguous for {exe:?}/{comm:?}, got {other:?}"),
            }
        }
    }

    #[test]
    fn identify_matches_the_two_in_scope_identities() {
        assert_eq!(
            identify("/usr/lib/systemd/systemd"),
            Some(InitKind::Systemd)
        );
        assert_eq!(identify("/lib/systemd/systemd"), Some(InitKind::Systemd));
        assert_eq!(identify("systemd"), Some(InitKind::Systemd));
        assert_eq!(identify("/usr/bin/runit"), Some(InitKind::Runit));
        assert_eq!(identify("/sbin/runit-init"), Some(InitKind::Runit));
        assert_eq!(identify("SYSTEMD"), Some(InitKind::Systemd));
    }

    #[test]
    fn identify_treats_the_bare_init_name_as_generic() {
        assert_eq!(identify("init"), None);
        assert_eq!(identify("/sbin/init"), None);
        assert_eq!(identify("/usr/sbin/init"), None);
        assert_eq!(identify(""), None);
        assert_eq!(identify("   "), None);
    }

    #[test]
    fn identify_uses_exact_executable_names_not_path_substrings() {
        // Provider words in parent directories do not identify the process.
        assert_eq!(
            identify("/opt/systemd-tools/supervisor"),
            Some(InitKind::Other("supervisor".to_string()))
        );
        assert_eq!(
            identify("/opt/runit/sbin/init"),
            None,
            "a generic init basename stays unknown"
        );

        // Similar executable names are not aliases for the supported inits.
        for name in ["not-systemd", "systemd-compat", "runit-wrapper"] {
            assert_eq!(
                identify(&format!("/usr/bin/{name}")),
                Some(InitKind::Other(name.to_string())),
                "{name} must not be guessed into a supported init"
            );
        }
    }

    #[test]
    fn identify_reports_other_inits_without_guessing() {
        assert_eq!(
            identify("/bin/busybox"),
            Some(InitKind::Other("busybox".to_string()))
        );
        assert_eq!(identify("/bin/sh"), Some(InitKind::Other("sh".to_string())));
    }

    #[test]
    fn capability_identity_round_trips_through_the_declared_form() {
        assert_eq!(InitKind::Systemd.capability_identity(), "systemd");
        assert_eq!(InitKind::Runit.capability_identity(), "runit");
        assert_eq!(
            InitKind::Other("busybox".to_string()).capability_identity(),
            "busybox"
        );
    }

    #[test]
    fn capability_selection_is_limited_to_the_two_exact_identities() {
        let cap = |domain: &str, identity: &str| crate::domain::system::capability::Capability {
            domain: domain.to_string(),
            identity: identity.to_string(),
        };
        assert_eq!(
            InitKind::from_capability(&cap("init-system", "runit")),
            Some(InitKind::Runit)
        );
        assert_eq!(
            InitKind::from_capability(&cap("init-system", "systemd")),
            Some(InitKind::Systemd)
        );
        // declared but outside this migration's scope (§6)
        assert_eq!(
            InitKind::from_capability(&cap("init-system", "sysvinit")),
            None
        );
        // another domain is out of scope entirely (§6: only init-system)
        assert_eq!(
            InitKind::from_capability(&cap("session-manager", "runit")),
            None
        );
    }
}
