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

use crate::domain::package::relations::{PackageRelations, Requirement};
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
    pub(crate) relations: Option<PackageRelations>,
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
    pub(crate) retained_dependencies: Vec<String>,
    /// package candidates whose dependency closure is incomplete because
    /// runtime dependency metadata was not declared by their source.
    pub(crate) unknown_dependencies: Vec<String>,
    pub(crate) unknown_relations: Vec<String>,
    pub(crate) removals: Vec<Replacement>,
    /// the init-provider decision the target profile makes (§8.2)
    pub(crate) init: InitSelection,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Replacement {
    pub(crate) name: String,
    pub(crate) current: String,
    pub(crate) by: String,
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
    let selected = constrained_selection(targets, installed, known, with_deps)?;

    for t in targets {
        visit(
            t,
            &wanted,
            installed,
            known,
            target_why,
            with_deps,
            &selected,
            &mut done,
            &mut visiting,
            &mut resolution,
        )?;
    }
    resolve_relations(&wanted, installed, known, &mut resolution)?;
    Ok(resolution)
}

/// Monotone constraint propagation within the shared resolver. Nawa exposes
/// one source-selected candidate per name; an incompatible range fails rather
/// than silently choosing a different source, older archive or downgrade.
fn constrained_selection(
    targets: &[String],
    installed: &HashMap<String, String>,
    known: &BTreeMap<String, Candidate>,
    with_deps: bool,
) -> Result<HashSet<String>> {
    let mut required: BTreeMap<String, Vec<Requirement>> = BTreeMap::new();
    for target in targets {
        required
            .entry(target.clone())
            .or_default()
            .push(Requirement::parse(target).map_err(anyhow::Error::msg)?);
    }
    let mut selected = HashSet::new();
    loop {
        let mut changed = false;
        for (name, constraints) in required.clone() {
            let candidate = known.get(&name);
            let current = installed.get(&name);
            let installed_satisfies =
                current.is_some_and(|v| constraints.iter().all(|r| r.matches(v)));
            let target_newer = targets.contains(&name)
                && candidate
                    .zip(current)
                    .is_some_and(|(c, v)| vercmp(&c.full(), v).is_gt());
            if installed_satisfies && !target_newer {
                continue;
            }
            let Some(candidate) = candidate else {
                if constraints.iter().any(|r| r.comparison.is_some()) {
                    bail!("no candidate satisfies the version requirements for {name}");
                }
                continue; // visit retains the established missing/cycle diagnostics
            };
            if !constraints.iter().all(|r| r.matches(&candidate.full())) {
                bail!(
                    "selected candidate {} {} does not satisfy [{}]",
                    name,
                    candidate.full(),
                    constraints
                        .iter()
                        .map(Requirement::canonical)
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
            if current.is_some_and(|v| vercmp(&candidate.full(), v).is_lt()) {
                bail!("dependency constraints would require downgrading {name}; explicit downgrade policy is required");
            }
            if selected.insert(name.clone()) {
                changed = true;
                if with_deps {
                    for dep in &candidate.deps {
                        let requirement = Requirement::parse(dep).map_err(anyhow::Error::msg)?;
                        if requirement.comparison.is_some() && candidate.relations.is_none() {
                            bail!("versioned dependency in {name} has unknown package relations metadata");
                        }
                        let list = required.entry(requirement.name.clone()).or_default();
                        if !list.contains(&requirement) {
                            list.push(requirement);
                            changed = true;
                        }
                    }
                }
            }
        }
        if !changed {
            break;
        }
    }
    Ok(selected)
}

fn resolve_relations(
    wanted: &HashSet<&str>,
    installed: &HashMap<String, String>,
    known: &BTreeMap<String, Candidate>,
    resolution: &mut Resolution,
) -> Result<()> {
    let selected: std::collections::BTreeSet<_> =
        resolution.items.iter().map(|i| i.name.as_str()).collect();
    let mut removals: BTreeMap<String, Replacement> = BTreeMap::new();
    for item in &resolution.items {
        let candidate = &known[&item.name];
        let Some(relations) = &candidate.relations else {
            resolution.unknown_relations.push(item.name.clone());
            continue;
        };
        let relations = relations.normalized().map_err(anyhow::Error::msg)?;
        for raw in &relations.replaces {
            let requirement = Requirement::parse(raw).map_err(anyhow::Error::msg)?;
            if requirement.name == item.name {
                bail!("{} cannot replace itself", item.name);
            }
            if let Some(current) = installed
                .get(&requirement.name)
                .filter(|v| requirement.matches(v))
            {
                if !wanted.contains(item.name.as_str()) {
                    bail!(
                        "replacement by {} must be an explicitly requested target",
                        item.name
                    );
                }
                if selected.contains(requirement.name.as_str()) {
                    bail!("{} is both selected and replaced", requirement.name);
                }
                let removal = Replacement {
                    name: requirement.name.clone(),
                    current: current.clone(),
                    by: item.name.clone(),
                };
                if let Some(other) = removals.insert(requirement.name.clone(), removal) {
                    if other.by != item.name {
                        bail!(
                            "ambiguous replacements for {} by {} and {}",
                            requirement.name,
                            other.by,
                            item.name
                        );
                    }
                }
            }
        }
    }
    let mut projected: BTreeMap<_, _> = installed
        .iter()
        .map(|(n, v)| (n.clone(), v.clone()))
        .collect();
    for name in removals.keys() {
        projected.remove(name);
    }
    for item in &resolution.items {
        projected.insert(item.name.clone(), known[&item.name].full());
    }
    for (name, version) in &projected {
        // Never apply a newer repository candidate's relations to an older
        // installed package. Its missing declaration remains unknown.
        let Some(candidate) = known.get(name).filter(|c| c.full() == *version) else {
            continue;
        };
        let Some(relations) = &candidate.relations else {
            continue;
        };
        for raw in &relations.conflicts {
            let requirement = Requirement::parse(raw).map_err(anyhow::Error::msg)?;
            if requirement.name == *name {
                bail!("{name} cannot conflict with itself");
            }
            if requirement.name != *name
                && projected
                    .get(&requirement.name)
                    .is_some_and(|v| requirement.matches(v))
            {
                bail!(
                    "package conflict: {name} conflicts with {}",
                    requirement.canonical()
                );
            }
        }
        if selected.contains(name.as_str()) {
            for dep in &candidate.deps {
                let req = Requirement::parse(dep).map_err(anyhow::Error::msg)?;
                if removals.contains_key(&req.name) {
                    bail!(
                        "replacement would remove dependency {} required by {name}",
                        req.name
                    );
                }
            }
        }
    }
    resolution.removals = removals.into_values().collect();
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn visit(
    name: &str,
    wanted: &HashSet<&str>,
    installed: &HashMap<String, String>,
    known: &BTreeMap<String, Candidate>,
    target_why: Why,
    with_deps: bool,
    selected: &HashSet<String>,
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
            resolution.unknown_relations.push(name.to_string());
            resolution.retained_dependencies.push(name.to_string());
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
    if current.is_some() && !selected.contains(name) {
        if is_target {
            resolution.up_to_date.push(name.to_string());
        } else {
            resolution.retained_dependencies.push(name.to_string());
        }
        if cand.relations.is_none() {
            resolution.unknown_relations.push(name.to_string());
        }
        done.insert(name.to_string());
        return Ok(());
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
            let requirement = Requirement::parse(dep).map_err(anyhow::Error::msg)?;
            visit(
                &requirement.name,
                wanted,
                installed,
                known,
                target_why,
                with_deps,
                selected,
                done,
                visiting,
                resolution,
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
            relations: None,
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
            relations: None,
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

    fn related(name: &str, version: &str, deps: &[&str]) -> Candidate {
        let mut candidate = cand(name, version, 1, deps);
        candidate.relations = Some(PackageRelations {
            schema: 1,
            conflicts: vec![],
            replaces: vec![],
        });
        candidate
    }

    #[test]
    fn version_requirements_upgrade_installed_dependencies_before_dependents() {
        let known = tree(vec![
            related("app", "1", &["lib>=2-1", "lib<3-1"]),
            related("lib", "2", &[]),
        ]);
        let result = install_plan(
            &["app".into()],
            &inst(&[("lib", "1-1")]),
            &known,
            None,
            Why::Explicit,
            true,
        )
        .unwrap();
        assert_eq!(names(&result), ["lib", "app"]);
        assert_eq!(result.items[0].replaces.as_deref(), Some("1-1"));
        assert_eq!(result.items[0].why, Why::Dependency);
    }

    #[test]
    fn requirements_from_later_targets_are_not_hidden_by_a_previous_visit() {
        let known = tree(vec![
            related("first", "1", &["lib>=1-1"]),
            related("second", "1", &["lib>=2-1"]),
            related("lib", "2", &[]),
        ]);
        let result = install_plan(
            &["first".into(), "second".into()],
            &inst(&[("lib", "1-1")]),
            &known,
            None,
            Why::Explicit,
            true,
        )
        .unwrap();
        assert_eq!(names(&result), ["lib", "first", "second"]);
        let mut incompatible = known.clone();
        incompatible
            .get_mut("first")
            .unwrap()
            .deps
            .push("lib<2-1".into());
        assert!(install_plan(
            &["first".into(), "second".into()],
            &inst(&[("lib", "1-1")]),
            &incompatible,
            None,
            Why::Explicit,
            true
        )
        .is_err());
    }

    #[test]
    fn conflicts_are_checked_in_both_directions_at_the_projected_versions() {
        let mut old = related("old", "1", &[]);
        old.relations.as_mut().unwrap().conflicts = vec!["new>=1-1".into()];
        let known = tree(vec![old, related("new", "1", &[])]);
        assert!(install_plan(
            &["new".into()],
            &inst(&[("old", "1-1")]),
            &known,
            None,
            Why::Explicit,
            true
        )
        .is_err());
        assert!(install_plan(
            &["new".into(), "old".into()],
            &HashMap::new(),
            &known,
            None,
            Why::Explicit,
            true
        )
        .is_err());
    }

    #[test]
    fn replacements_are_explicit_versioned_and_ambiguity_is_rejected() {
        let mut candidate = related("new", "2", &[]);
        candidate.relations.as_mut().unwrap().replaces = vec!["old<2-1".into()];
        candidate.relations.as_mut().unwrap().conflicts = vec!["old".into()];
        let known = tree(vec![candidate.clone()]);
        let result = install_plan(
            &["new".into()],
            &inst(&[("old", "1-1")]),
            &known,
            None,
            Why::Explicit,
            true,
        )
        .unwrap();
        assert_eq!(
            result.removals,
            [Replacement {
                name: "old".into(),
                current: "1-1".into(),
                by: "new".into()
            }]
        );
        assert!(install_plan(
            &["new".into()],
            &inst(&[("old", "2-1")]),
            &known,
            None,
            Why::Explicit,
            true
        )
        .is_err());
        candidate.name = "also-new".into();
        let mut ambiguous = known;
        ambiguous.insert(candidate.name.clone(), candidate);
        assert!(install_plan(
            &["new".into(), "also-new".into()],
            &inst(&[("old", "1-1")]),
            &ambiguous,
            None,
            Why::Explicit,
            true
        )
        .is_err());
        ambiguous.insert("app".into(), related("app", "1", &["new"]));
        assert!(install_plan(
            &["app".into()],
            &inst(&[("old", "1-1")]),
            &ambiguous,
            None,
            Why::Explicit,
            true
        )
        .is_err());
    }

    #[test]
    fn missing_legacy_relations_remain_unknown_and_constraints_fail_closed() {
        let known = tree(vec![cand("legacy", "1", 1, &[])]);
        let result = install_plan(
            &["legacy".into()],
            &HashMap::new(),
            &known,
            None,
            Why::Explicit,
            true,
        )
        .unwrap();
        assert_eq!(result.unknown_relations, ["legacy"]);
        let known = tree(vec![
            cand("app", "1", 1, &["lib>=1-1"]),
            cand("lib", "1", 1, &[]),
        ]);
        assert!(install_plan(
            &["app".into()],
            &HashMap::new(),
            &known,
            None,
            Why::Explicit,
            true
        )
        .is_err());
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
                relations: None,
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
                relations: None,
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
                relations: None,
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
