//! The install planner (design §8.1–8.2): deterministic dependency
//! resolution over normalized candidates. This is the domain core of
//! `nawa install` / `nawa update` planning — the same resolution `plan.rs`
//! has always performed, expressed over the §8.1 candidate model instead of
//! recipe and database types, so the profile-driven provider selection
//! (Phase 4) and the migration planner consume the same values.
//!
//! The planner performs no I/O and imports no adapter types: candidates and
//! installed state arrive normalized (`version-rel` strings, dependency
//! name lists). Error wording is part of the observable behavior and stays
//! byte-identical to the planner this replaces.
//!
//! Design notes (§8):
//!
//! - It extends the existing resolution rather than being a second
//!   resolver: `plan::install_plan` adapts recipes and database records to
//!   candidates and delegates here.
//! - Dependency forms supported: plain name edges, build + run for a
//!   source-backed candidate (§8.1). Authenticated schema-v4 binary
//!   candidates also carry runtime dependency names and provided
//!   capabilities; the repository adapter normalizes those values into
//!   this same resolver. Version ranges, conflicts and replacements are
//!   not yet represented by the current index schema.
//! - Provider selection (§6.2, the exclusive init-system role) needs the
//!   target profile (§7), which arrives with Phase 4. Candidates already
//!   carry their declared capabilities, so nothing has to be re-modeled.
//! - Architecture filtering is not a resolution rule: the application
//!   layer assembles candidates only from sources that fit the machine
//!   (`sys::arch_fits`), so the planner sees a feasible candidate set.

use anyhow::{bail, Result};
use std::collections::{BTreeMap, HashMap, HashSet};

use crate::domain::package::version::vercmp;
use crate::domain::system::capability::Provides;
use crate::domain::system::profile::SystemProfile;
use crate::suggest;

/// Why a package is part of a plan (the `reason` recorded in the database).
/// The database mapping lives with the application layer (`plan.rs`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Why {
    /// named on the command line
    Explicit,
    /// pulled in as a dependency of something else
    Dependency,
    /// requested directly as a member of a package group
    Group,
    /// an installed package being brought to the candidate's version
    /// (keeps whatever reason it already had)
    Update,
}

/// Where a normalized candidate came from. The application maps this
/// provenance back to its recipe or authenticated repository entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CandidateOrigin {
    Recipe,
    Repository,
}

/// One normalized candidate (§8.1): a trusted package the planner may
/// select. Recipe candidates carry build + run dependencies; repository
/// candidates carry authenticated runtime dependencies from index v4.
#[derive(Debug, Clone)]
pub(crate) struct Candidate {
    pub(crate) name: String,
    /// version without the rel; version_rel joins it for comparisons
    pub(crate) version: String,
    pub(crate) rel: u32,
    /// Recipe candidates include build + run edges; binary candidates include
    /// runtime dependencies only.
    pub(crate) deps: Vec<String>,
    /// False means repository metadata did not declare runtime dependencies.
    /// The package may be a direct action, but its dependency closure must
    /// remain visibly incomplete rather than being called empty.
    pub(crate) dependencies_known: bool,
    /// Source provenance is retained through resolution and explanation.
    pub(crate) origin: CandidateOrigin,
    /// Capability declarations from the trusted metadata chain.
    pub(crate) provides: Provides,
}

/// Result of looking for packages that provide one exact capability.
///
/// Unknown metadata is kept separate from a known-empty declaration: an
/// unknown candidate is never selected, but its existence is reported so
/// the caller cannot mistake missing evidence for proof that no provider
/// exists.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ProviderCandidates {
    /// Candidates declaring exactly one identity in the requested domain,
    /// equal to the requested capability.
    pub(crate) matching: Vec<String>,
    /// Candidates declaring multiple identities in the requested domain.
    /// Exclusive-provider callers must refuse these instead of guessing.
    pub(crate) ambiguous: Vec<String>,
    /// Candidates whose capability metadata is absent/legacy.
    pub(crate) unknown: Vec<String>,
}

/// Find providers for any normalized capability without performing I/O or
/// applying domain-specific policy. The caller decides whether the domain is
/// supported and whether multiple providers are permitted.
pub(crate) fn providers_for(
    candidates: &BTreeMap<String, Candidate>,
    requested: &crate::domain::system::capability::Capability,
) -> ProviderCandidates {
    let mut result = ProviderCandidates::default();
    for (name, candidate) in candidates {
        match &candidate.provides {
            Provides::Unknown => result.unknown.push(name.clone()),
            Provides::Declared(declarations) => {
                let domain: Vec<_> = declarations
                    .iter()
                    .filter(|capability| capability.domain == requested.domain)
                    .collect();
                match domain.as_slice() {
                    [only] if *only == requested => result.matching.push(name.clone()),
                    [_, _, ..] => result.ambiguous.push(name.clone()),
                    _ => {}
                }
            }
        }
    }
    result
}

/// Candidate names that declare `identity` as their single init-system
/// provider (§6.2/§8.4). Candidates with unknown or ambiguous metadata are
/// never chosen by preference — they are simply not reported as providers.
pub(crate) fn init_provider_names(
    candidates: &std::collections::BTreeMap<String, Candidate>,
    identity: &str,
) -> Vec<String> {
    use crate::domain::system::capability::InitProvider;
    let mut names: Vec<String> = candidates
        .iter()
        .filter(|(_, candidate)| {
            matches!(
                candidate.provides.init_provider(),
                InitProvider::Single(cap) if cap.identity == identity
            )
        })
        .map(|(name, _)| name.clone())
        .collect();
    names.sort();
    names
}

/// The installed-state value format the planner compares: `version-rel`.
pub(crate) fn version_rel(version: &str, rel: u32) -> String {
    format!("{version}-{rel}")
}

impl Candidate {
    /// The candidate's own `version-rel`.
    fn full(&self) -> String {
        version_rel(&self.version, self.rel)
    }
}

/// The init-provider decision carried by a resolution (§8.2). `Declared`
/// is the desired selection made by the effective target profile (§7).
/// Candidate-provider explanation is computed separately from normalized
/// recipe metadata and authenticated index-v4 metadata by the application
/// adapter. A package transaction never switches the running init: that
/// remains a separately authorized migration (§7).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) enum InitSelection {
    /// the effective target profile declares the `init-system` role with
    /// this normalized identity (§8.4: "required by: target profile")
    Declared { identity: String },
    /// no profile declares an init provider: the decision is unresolved —
    /// reported, never guessed (§8.2)
    #[default]
    Unresolved,
}

/// The init decision of one solve: what the target profile declares.
fn init_selection(profile: Option<&SystemProfile>) -> InitSelection {
    // The profile's normalized init capability (§6.1) is the single source of
    // the declared decision; only the two in-scope identities select one.
    match profile.and_then(SystemProfile::init_capability) {
        Some(cap) => InitSelection::Declared {
            identity: cap.identity.clone(),
        },
        None => InitSelection::Unresolved,
    }
}

/// The normalized planner output (§8.2): the package decisions. Provider
/// selection and migration-candidate fields arrive with the Phase 4/5 work.
#[derive(Debug, Default)]
pub(crate) struct Resolution {
    /// build + install order: dependencies first
    pub(crate) items: Vec<ResolvedItem>,
    /// requested targets that need no work — reported, not rebuilt
    pub(crate) up_to_date: Vec<String>,
    /// package candidates whose dependency closure is incomplete because
    /// runtime dependency metadata was not declared by their source.
    pub(crate) unknown_dependencies: Vec<String>,
    /// the init-provider decision the target profile makes (§8.2)
    pub(crate) init: InitSelection,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedItem {
    pub(crate) name: String,
    pub(crate) origin: CandidateOrigin,
    pub(crate) why: Why,
    /// the installed `version-rel` this plan replaces
    pub(crate) replaces: Option<String>,
}

/// Plan the installation of `targets`.
///
/// * `installed` — the installed `version-rel` per package name; an
///   installed package that is not a requested target is satisfied and its
///   own dependencies are not walked
/// * `profile` — the effective target profile (§8.1 input); its
///   `init-system` declaration becomes the resolution's
///   [`InitSelection`] (§8.2)
/// * `target_why` — `Explicit` for `nawa install`, `Update` for `nawa update`
/// * `with_deps` — follow dependency edges (`--no-deps` turns this off)
///
/// Deterministic for identical inputs (§8.2): the walk order follows the
/// target order and the candidate dependency lists.
pub(crate) fn install_plan(
    targets: &[String],
    installed: &HashMap<String, String>,
    known: &BTreeMap<String, Candidate>,
    profile: Option<&SystemProfile>,
    target_why: Why,
    with_deps: bool,
) -> Result<Resolution> {
    let wanted: HashSet<&str> = targets.iter().map(|s| s.as_str()).collect();
    let mut resolution = Resolution {
        init: init_selection(profile),
        ..Resolution::default()
    };
    let mut done: HashSet<String> = HashSet::new();
    let mut visiting: Vec<String> = Vec::new();

    for t in targets {
        visit(
            t,
            &wanted,
            installed,
            known,
            target_why,
            with_deps,
            &mut done,
            &mut visiting,
            &mut resolution,
        )?;
    }
    Ok(resolution)
}

#[allow(clippy::too_many_arguments)]
fn visit(
    name: &str,
    wanted: &HashSet<&str>,
    installed: &HashMap<String, String>,
    known: &BTreeMap<String, Candidate>,
    target_why: Why,
    with_deps: bool,
    done: &mut HashSet<String>,
    visiting: &mut Vec<String>,
    resolution: &mut Resolution,
) -> Result<()> {
    if done.contains(name) {
        return Ok(());
    }
    let is_target = wanted.contains(name);
    let cand = match known.get(name) {
        Some(c) => c,
        None if installed.contains_key(name) && !is_target => {
            // installed from somewhere else (seed, third-party repo): satisfied
            done.insert(name.to_string());
            return Ok(());
        }
        None => {
            let names: Vec<String> = known.keys().cloned().collect();
            let hint = suggest::hint(name, &names);
            if is_target {
                bail!("no recipe for `{name}`{hint}");
            }
            let chain = visiting.join(" -> ");
            bail!("dependency `{name}` (needed by {chain}) is neither installed nor has a recipe{hint}");
        }
    };

    // an installed package only needs work when it is a requested target and
    // the candidate is newer; installed dependencies are simply satisfied
    let current = installed.get(name);
    if let Some(have) = current {
        if !is_target || vercmp(&cand.full(), have) != std::cmp::Ordering::Greater {
            if is_target {
                resolution.up_to_date.push(name.to_string());
            }
            done.insert(name.to_string());
            return Ok(());
        }
    }

    if with_deps && !cand.dependencies_known {
        // Keep legacy binary installation possible without claiming an empty
        // dependency list. Callers requiring a complete closure (notably
        // system migration) filter unknown candidates before resolving.
        resolution.unknown_dependencies.push(name.to_string());
    }

    if visiting.iter().any(|v| v == name) {
        let mut cycle = visiting.clone();
        cycle.push(name.to_string());
        bail!("circular dependency: {}", cycle.join(" -> "));
    }
    visiting.push(name.to_string());
    if with_deps {
        for dep in &cand.deps {
            visit(
                dep, wanted, installed, known, target_why, with_deps, done, visiting, resolution,
            )?;
        }
    }
    visiting.pop();

    done.insert(name.to_string());
    resolution.items.push(ResolvedItem {
        name: cand.name.clone(),
        origin: cand.origin,
        why: if is_target {
            target_why
        } else {
            Why::Dependency
        },
        replaces: current.cloned(),
    });
    Ok(())
}

/// Installed packages whose candidate is newer: their names, in the order
/// the installed records were given (the database lists them sorted, so
/// this is alphabetical in practice).
///
/// * `installed` — `(name, version-rel)` pairs; a package without a
///   candidate is not "outdated", just unknown to the planner
///
/// The same comparison `install_plan` applies to a requested target: a
/// candidate only replaces an installed package when it is strictly newer
/// (`update` keeps the db's reason).
pub(crate) fn outdated(
    installed: &[(String, String)],
    known: &BTreeMap<String, Candidate>,
) -> Vec<String> {
    installed
        .iter()
        .filter(|(name, have)| {
            known
                .get(name)
                .is_some_and(|c| vercmp(&c.full(), have) == std::cmp::Ordering::Greater)
        })
        .map(|(name, _)| name.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(name: &str, ver: &str, rel: u32, deps: &[&str]) -> Candidate {
        Candidate {
            name: name.to_string(),
            version: ver.to_string(),
            rel,
            deps: deps.iter().map(|d| d.to_string()).collect(),
            dependencies_known: true,
            origin: CandidateOrigin::Recipe,
            provides: Provides::Unknown,
        }
    }

    fn binary_cand(name: &str, ver: &str, deps: &[&str], known: bool) -> Candidate {
        Candidate {
            name: name.to_string(),
            version: ver.to_string(),
            rel: 1,
            deps: deps.iter().map(|d| d.to_string()).collect(),
            dependencies_known: known,
            origin: CandidateOrigin::Repository,
            provides: Provides::Unknown,
        }
    }

    fn tree(cands: Vec<Candidate>) -> BTreeMap<String, Candidate> {
        cands.into_iter().map(|c| (c.name.clone(), c)).collect()
    }

    fn inst(full: &[(&str, &str)]) -> HashMap<String, String> {
        full.iter()
            .map(|(n, v)| (n.to_string(), v.to_string()))
            .collect()
    }

    fn names(r: &Resolution) -> Vec<&str> {
        r.items.iter().map(|i| i.name.as_str()).collect()
    }

    #[test]
    fn version_rel_is_the_installed_state_format() {
        assert_eq!(version_rel("1.2.3", 4), "1.2.3-4");
    }

    #[test]
    fn provider_lookup_is_domain_generic_and_keeps_unknown_separate() {
        let requested =
            crate::domain::system::capability::Capability::parse("network-stack=networkd").unwrap();
        let candidates = tree(vec![
            Candidate {
                name: "networkd".to_string(),
                version: "1".to_string(),
                rel: 1,
                deps: vec![],
                dependencies_known: true,
                origin: CandidateOrigin::Recipe,
                provides: Provides::parse_all(&["network-stack=networkd".to_string()]).unwrap(),
            },
            Candidate {
                name: "dual-network".to_string(),
                version: "1".to_string(),
                rel: 1,
                deps: vec![],
                dependencies_known: true,
                origin: CandidateOrigin::Recipe,
                provides: Provides::parse_all(&[
                    "network-stack=networkd".to_string(),
                    "network-stack=netifd".to_string(),
                ])
                .unwrap(),
            },
            cand("legacy-provider", "1", 1, &[]),
            Candidate {
                name: "other-domain".to_string(),
                version: "1".to_string(),
                rel: 1,
                deps: vec![],
                dependencies_known: true,
                origin: CandidateOrigin::Recipe,
                provides: Provides::parse_all(&["init-system=systemd".to_string()]).unwrap(),
            },
        ]);
        let result = providers_for(&candidates, &requested);
        assert_eq!(result.matching, vec!["networkd".to_string()]);
        assert_eq!(result.ambiguous, vec!["dual-network".to_string()]);
        assert_eq!(result.unknown, vec!["legacy-provider".to_string()]);
    }

    #[test]
    fn candidates_without_declared_metadata_remain_unknown() {
        // this fixture supplies no capabilities, so Unknown is not an empty declaration
        let c = cand("runit", "2.1.2", 2, &[]);
        assert_eq!(c.provides, Provides::Unknown);
    }

    #[test]
    fn recipe_and_binary_candidates_share_one_dependency_graph() {
        let known = tree(vec![
            cand("app", "1", 1, &["middle"]),
            binary_cand("middle", "2", &["runtime-lib", "leaf"], true),
            cand("runtime-lib", "3", 1, &[]),
            binary_cand("leaf", "4", &[], true),
        ]);
        let result = install_plan(
            &["app".to_string()],
            &HashMap::new(),
            &known,
            None,
            Why::Explicit,
            true,
        )
        .unwrap();
        assert_eq!(names(&result), ["runtime-lib", "leaf", "middle", "app"]);
        assert_eq!(
            result
                .items
                .iter()
                .map(|item| item.origin)
                .collect::<Vec<_>>(),
            [
                CandidateOrigin::Recipe,
                CandidateOrigin::Repository,
                CandidateOrigin::Repository,
                CandidateOrigin::Recipe,
            ]
        );
        assert_eq!(result.items[0].why, Why::Dependency);
        assert_eq!(result.items[3].why, Why::Explicit);
    }

    #[test]
    fn unknown_binary_runtime_dependencies_are_reported_not_treated_as_empty() {
        let known = tree(vec![binary_cand("legacy-bin", "1", &[], false)]);
        let planned = install_plan(
            &["legacy-bin".to_string()],
            &HashMap::new(),
            &known,
            None,
            Why::Explicit,
            true,
        )
        .unwrap();
        assert_eq!(names(&planned), ["legacy-bin"]);
        assert_eq!(planned.unknown_dependencies, ["legacy-bin"]);

        let skipped = install_plan(
            &["legacy-bin".to_string()],
            &HashMap::new(),
            &known,
            None,
            Why::Explicit,
            false,
        )
        .unwrap();
        assert_eq!(names(&skipped), ["legacy-bin"]);
        assert!(skipped.unknown_dependencies.is_empty());
    }

    #[test]
    fn pulls_deps_in_order_and_marks_them_dependencies() {
        let known = tree(vec![
            cand("zlib", "1", 1, &[]),
            cand("openssl", "1", 1, &[]),
            cand("curl", "1", 1, &["zlib", "openssl"]),
        ]);
        let r = install_plan(
            &["curl".to_string()],
            &HashMap::new(),
            &known,
            None,
            Why::Explicit,
            true,
        )
        .unwrap();
        assert_eq!(names(&r), ["zlib", "openssl", "curl"]);
        assert_eq!(r.items[0].why, Why::Dependency);
        assert_eq!(r.items[2].why, Why::Explicit);
        assert!(r.items.iter().all(|i| i.replaces.is_none()));
        assert!(r.up_to_date.is_empty());
    }

    #[test]
    fn installed_deps_are_satisfied_and_not_walked() {
        let known = tree(vec![
            cand("zlib", "1", 1, &[]),
            cand("curl", "1", 1, &["zlib"]),
        ]);
        let have = inst(&[("zlib", "0.5-1")]);
        let r = install_plan(
            &["curl".to_string()],
            &have,
            &known,
            None,
            Why::Explicit,
            true,
        )
        .unwrap();
        assert_eq!(
            names(&r),
            ["curl"],
            "an installed (even older) dep is satisfied"
        );
    }

    #[test]
    fn an_installed_dep_without_a_candidate_is_satisfied() {
        // installed from a seed or third-party repository: no candidate
        // exists, the planner must not invent one
        let known = tree(vec![cand("curl", "1", 1, &["zlib"])]);
        let have = inst(&[("zlib", "0.5-1")]);
        let r = install_plan(
            &["curl".to_string()],
            &have,
            &known,
            None,
            Why::Explicit,
            true,
        )
        .unwrap();
        assert_eq!(names(&r), ["curl"]);
    }

    #[test]
    fn up_to_date_target_is_reported_not_rebuilt() {
        let known = tree(vec![cand("zlib", "1", 1, &[])]);
        let have = inst(&[("zlib", "1-1")]);
        let r = install_plan(
            &["zlib".to_string()],
            &have,
            &known,
            None,
            Why::Explicit,
            true,
        )
        .unwrap();
        assert!(r.items.is_empty());
        assert_eq!(r.up_to_date, ["zlib"]);
    }

    #[test]
    fn an_older_installed_target_is_replaced_with_its_installed_version() {
        let known = tree(vec![cand("zlib", "1.3", 1, &[])]);
        let have = inst(&[("zlib", "1.2-1")]);
        let r = install_plan(
            &["zlib".to_string()],
            &have,
            &known,
            None,
            Why::Explicit,
            true,
        )
        .unwrap();
        assert_eq!(r.items[0].replaces.as_deref(), Some("1.2-1"));
    }

    #[test]
    fn no_deps_plans_only_the_target() {
        let known = tree(vec![
            cand("zlib", "1", 1, &[]),
            cand("curl", "1", 1, &["zlib"]),
        ]);
        let r = install_plan(
            &["curl".to_string()],
            &HashMap::new(),
            &known,
            None,
            Why::Explicit,
            false,
        )
        .unwrap();
        assert_eq!(names(&r), ["curl"]);
    }

    #[test]
    fn a_typo_gets_a_suggestion_and_a_cycle_is_an_error() {
        let known = tree(vec![cand("zlib", "1", 1, &[])]);
        let e = install_plan(
            &["zlb".to_string()],
            &HashMap::new(),
            &known,
            None,
            Why::Explicit,
            true,
        )
        .unwrap_err();
        assert!(e.to_string().contains("no recipe for `zlb`"), "{e}");
        assert!(e.to_string().contains("zlib"), "{e}");

        let cyc = tree(vec![cand("a", "1", 1, &["b"]), cand("b", "1", 1, &["a"])]);
        let e = install_plan(
            &["a".to_string()],
            &HashMap::new(),
            &cyc,
            None,
            Why::Explicit,
            true,
        )
        .unwrap_err();
        assert!(
            e.to_string().contains("circular dependency: a -> b -> a"),
            "{e}"
        );
    }

    #[test]
    fn a_missing_dependency_names_who_needs_it() {
        let known = tree(vec![cand("a", "1", 1, &["ghost"])]);
        let e = install_plan(
            &["a".to_string()],
            &HashMap::new(),
            &known,
            None,
            Why::Explicit,
            true,
        )
        .unwrap_err();
        let s = e.to_string();
        assert!(s.contains("ghost") && s.contains("needed by a"), "{s}");
    }

    #[test]
    fn resolution_is_deterministic_for_identical_inputs() {
        let known = tree(vec![
            cand("zlib", "1", 1, &[]),
            cand("curl", "1", 1, &["zlib"]),
        ]);
        let targets = ["curl".to_string()];
        let a = install_plan(&targets, &HashMap::new(), &known, None, Why::Explicit, true).unwrap();
        let b = install_plan(&targets, &HashMap::new(), &known, None, Why::Explicit, true).unwrap();
        assert_eq!(format!("{a:?}"), format!("{b:?}"));
    }

    #[test]
    fn outdated_lists_strictly_newer_in_input_order() {
        let known = tree(vec![
            cand("a", "1.10", 1, &[]),
            cand("b", "2", 1, &[]),
            cand("c", "1", 1, &[]),
        ]);
        // given unsorted, to prove the input order is preserved
        let have = vec![
            ("c".to_string(), "1-1".to_string()),
            ("b".to_string(), "1-1".to_string()),
            ("a".to_string(), "1.9-1".to_string()),
        ];
        assert_eq!(
            outdated(&have, &known),
            ["b", "a"],
            "1.10 > 1.9, rel counts"
        );
    }

    #[test]
    fn outdated_skips_equal_older_and_unknown() {
        let known = tree(vec![cand("a", "1", 2, &[]), cand("b", "1", 1, &[])]);
        let have = vec![
            // same version, same rel: up to date
            ("a".to_string(), "1-2".to_string()),
            // same version, older rel: not outdated
            ("b".to_string(), "1-1".to_string()),
            // no candidate: unknown, not outdated
            ("ghost".to_string(), "0-1".to_string()),
        ];
        assert_eq!(outdated(&have, &known), Vec::<String>::new());
    }

    fn profile_doc(pairs: &[(&str, &str)], init: Option<&str>) -> SystemProfile {
        let scalars: BTreeMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let providers = init.map(|i| BTreeMap::from([("init-system".to_string(), i.to_string())]));
        SystemProfile::from_doc(&scalars, &providers).unwrap()
    }

    #[test]
    fn the_profiles_init_declaration_is_the_resolution_s_decision() {
        // §8.4's example: systemd required by the target profile
        let p = profile_doc(&[("schema", "1")], Some("systemd"));
        let r = install_plan(
            &["zlib".to_string()],
            &HashMap::new(),
            &tree(vec![cand("zlib", "1", 1, &[])]),
            Some(&p),
            Why::Explicit,
            true,
        )
        .unwrap();
        assert_eq!(
            r.init,
            InitSelection::Declared {
                identity: "systemd".to_string()
            }
        );
    }

    #[test]
    fn no_declared_init_provider_is_an_explicit_unresolved_result() {
        // §8.2: an unresolved result, never a guess — with no profile at
        // all and with a profile that has no init-system role
        let silent = profile_doc(&[("schema", "1")], None);
        for profile in [None, Some(&silent)] {
            let r = install_plan(
                &["zlib".to_string()],
                &HashMap::new(),
                &tree(vec![cand("zlib", "1", 1, &[])]),
                profile,
                Why::Explicit,
                true,
            )
            .unwrap();
            assert_eq!(r.init, InitSelection::Unresolved);
        }
    }

    #[test]
    fn the_init_decision_is_deterministic_and_package_decisions_unaffected() {
        let known = tree(vec![cand("zlib", "1", 1, &[])]);
        let targets = ["zlib".to_string()];
        let p = profile_doc(&[("schema", "1")], Some("runit"));
        let with = install_plan(
            &targets,
            &HashMap::new(),
            &known,
            Some(&p),
            Why::Explicit,
            true,
        )
        .unwrap();
        let without =
            install_plan(&targets, &HashMap::new(), &known, None, Why::Explicit, true).unwrap();
        assert_eq!(with.items, without.items);
        assert_ne!(with.init, without.init);
        // out-of-scope identities are still the profile's declared desire;
        // scope checking belongs to the migration, not the solve
        assert_eq!(
            with.init,
            InitSelection::Declared {
                identity: "runit".to_string()
            }
        );
    }
}
