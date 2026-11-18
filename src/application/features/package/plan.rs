//! Planning for `nawa install <name>`, `nawa update`, `nawa outdated` and
//! `nawa autoremove`: pure functions over the recipe graph and the local
//! database, so every decision is unit-testable without building anything.
//!
//! Dependency edges followed when installing by name are `depends.build`
//! (needed to compile) **and** `depends.run` (needed to work). Whatever is
//! already installed counts as satisfied and its own dependencies are not
//! walked — the same rule `nawa deps` uses.
//!
//! The install/update resolution core is `domain::planning::install`
//! (design §8): this module adapts recipes and database records to the
//! normalized candidate model and delegates, so there is exactly one
//! resolver. `outdated`, `orphans` and `removal_order` are thin adapters
//! over the same model: the update-candidate query lives in
//! `domain::planning::install`, autoremove and removal ordering in
//! `domain::planning::removal`.

use anyhow::Result;
use std::collections::{BTreeMap, HashMap, HashSet};

use crate::adapters::config::InstallMode;
use crate::adapters::db::Entry;
use crate::domain::planning::install::{self, Candidate, CandidateOrigin};
pub(crate) use crate::domain::planning::install::{InitSelection, Why};
use crate::domain::package::metadata::RunDeps;
use crate::repo::RepoEntry;
use crate::domain::planning::removal::{self, InstalledPkg};
use crate::domain::system::capability::Provides;
use crate::domain::system::profile::SystemProfile;
use crate::recipe::Recipe;

/// The planning semantics of [`Why`] are the domain enum; the database
/// reason mapping and the label stay here, with the adapters.
impl Why {
    /// the `reason` argument of `install::install_archive`
    pub fn db_reason(self) -> crate::adapters::db::InstallReason {
        match self {
            Why::Explicit => crate::adapters::db::InstallReason::Explicit,
            Why::Dependency => crate::adapters::db::InstallReason::Dependency,
            Why::Group => crate::adapters::db::InstallReason::Group,
            Why::Update => crate::adapters::db::InstallReason::Keep,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Why::Explicit => "explicit",
            Why::Dependency => "dependency",
            Why::Group => "group",
            Why::Update => "update",
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Item {
    pub rec: Recipe,
    pub why: Why,
    /// `version-rel` currently installed, when this replaces something
    pub replaces: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PlanStep {
    Recipe(usize),
    Binary(usize),
}

#[derive(Debug, Default)]
pub(crate) struct Plan {
    /// Recipe-backed actions; PlanStep indexes preserve their mixed order.
    pub items: Vec<Item>,
    /// One topological order spanning recipe and binary repository packages.
    pub order: Vec<PlanStep>,
    /// Install reason and replaced version for each binary action.
    pub binary_whys: Vec<Why>,
    pub binary_replaces: Vec<Option<String>>,
    /// repository packages whose runtime dependency closure is unknown.
    pub unknown_dependencies: Vec<String>,
    /// requested targets that need nothing at the selected source version
    /// or newer — reported, not rebuilt
    pub up_to_date: Vec<String>,
    /// the init-provider decision the target profile makes (§8.2)
    pub init: InitSelection,
    /// candidates whose authenticated metadata declares that init provider
    /// (§8.4: the solver explains provider selection)
    pub init_providers: Vec<String>,
}

fn full(version: &str, rel: u32) -> String {
    format!("{version}-{rel}")
}

/// Everything that must exist before `rec` can be built and used.
fn deps_of(
    rec: &Recipe,
) -> impl Iterator<Item = String> + use<'_> {
    rec.build_deps().into_iter().chain(rec.depends.run.iter().cloned())
}

/// Plan the installation of `targets`: adapt recipes and database records
/// to the normalized candidate model (§8.1) and delegate the resolution to
/// the domain planner — the one resolver (design §8).
///
/// * `target_why` — `Explicit` for `nawa install`, `Update` for `nawa update`
/// * `with_deps`  — follow dependencies (`--no-deps` turns this off)
/// * `profile`    — the effective target profile (§8.1 plan input); its
///   `init-system` declaration becomes `Plan::init` (§8.2)
pub(crate) fn install_plan(
    targets: &[String],
    installed: &HashMap<String, Entry>,
    known: &BTreeMap<String, Recipe>,
    profile: Option<&SystemProfile>,
    target_why: Why,
    with_deps: bool,
) -> Result<Plan> {
    let (plan, _) = install_plan_with_binaries(
        targets, installed, known, &BTreeMap::new(), InstallMode::Build, profile, target_why, with_deps,
    )?;
    Ok(plan)
}

/// Resolve recipe and authenticated binary candidates through the same
/// domain graph. Repository candidates fill names that have no recipe.
pub(crate) fn install_plan_with_binaries(
    targets: &[String],
    installed: &HashMap<String, Entry>,
    known: &BTreeMap<String, Recipe>,
    binaries: &BTreeMap<String, RepoEntry>,
    mode: InstallMode,
    profile: Option<&SystemProfile>,
    target_why: Why,
    with_deps: bool,
) -> Result<(Plan, Vec<RepoEntry>)> {
    let installed_full: HashMap<String, String> = installed
        .iter()
        .map(|(name, e)| (name.clone(), full(&e.version, e.rel)))
        .collect();
    let candidates = candidates_of_with_binaries(known, binaries, mode)?;
    let init_providers = match profile.and_then(|p| p.init_capability()) {
        Some(cap) => install::init_provider_names(&candidates, &cap.identity),
        None => Vec::new(),
    };
    let resolved = install::install_plan(
        targets, &installed_full, &candidates, profile, target_why, with_deps,
    )?;

    let mut items = Vec::new();
    let mut extras = Vec::new();
    let mut order = Vec::with_capacity(resolved.items.len());
    let mut binary_whys = Vec::new();
    let mut binary_replaces = Vec::new();

    for item in resolved.items {
        match item.origin {
            CandidateOrigin::Recipe => {
                let index = items.len();
                items.push(Item {
                    rec: known.get(&item.name)
                        .expect("recipe candidate must retain its recipe").clone(),
                    why: item.why,
                    replaces: item.replaces,
                });
                order.push(PlanStep::Recipe(index));
            }
            CandidateOrigin::Repository => {
                let entry = binaries.get(&item.name).ok_or_else(|| {
                    anyhow::anyhow!("resolved binary candidate {} has no repository entry", item.name)
                })?;
                let index = extras.len();
                extras.push(entry.clone());
                binary_whys.push(item.why);
                binary_replaces.push(item.replaces);
                order.push(PlanStep::Binary(index));
            }
        }
    }

    Ok((
        Plan {
            items, order, binary_whys, binary_replaces,
            unknown_dependencies: resolved.unknown_dependencies,
            up_to_date: resolved.up_to_date,
            init: resolved.init,
            init_providers,
        },
        extras,
    ))
}

/// The normalized candidate (§8.1) of a source-backed package: build + run
/// edges in declaration order, plus the capabilities declared in its
/// `[capabilities]` table (B3 chain §3). A recipe without the table is
/// `Provides::Unknown`, never an empty declaration (§7).
fn candidate_of(rec: &Recipe) -> Candidate {
    let provides = match &rec.capabilities {
        None => Provides::Unknown,
        Some(caps) => {
            let normalized = crate::domain::package::metadata::normalize_provides(&caps.provides)
                .expect("recipe capabilities are validated at load time");
            Provides::parse_all(&normalized).expect("normalized capabilities parse")
        }
    };
    Candidate {
        name: rec.package.name.clone(),
        version: rec.package.version.clone(),
        rel: rec.package.rel,
        deps: deps_of(rec).collect(),
        dependencies_known: true,
        origin: CandidateOrigin::Recipe,
        provides,
    }
}

fn candidates_of(known: &BTreeMap<String, Recipe>) -> BTreeMap<String, Candidate> {
    known.iter().map(|(name, rec)| (name.clone(), candidate_of(rec))).collect()
}

fn binary_candidate_of(entry: &RepoEntry) -> Result<Candidate> {
    let metadata = crate::domain::package::metadata::metadata_from_options(
        entry.rundeps.as_ref(), entry.provides.as_ref(),
    ).map_err(anyhow::Error::msg)?;
    let (deps, dependencies_known) = match metadata.rundeps {
        RunDeps::Declared(deps) => (deps, true),
        RunDeps::Unknown => (Vec::new(), false),
    };
    Ok(Candidate {
        name: entry.name.clone(),
        version: entry.version.clone(),
        rel: entry.rel,
        deps,
        dependencies_known,
        origin: CandidateOrigin::Repository,
        provides: metadata.provides,
    })
}

fn candidates_of_with_binaries(
    known: &BTreeMap<String, Recipe>,
    binaries: &BTreeMap<String, RepoEntry>,
    mode: InstallMode,
) -> Result<BTreeMap<String, Candidate>> {
    match mode {
        InstallMode::Build => Ok(candidates_of(known)),
        // Strict repository mode must resolve the package that will actually
        // be fetched, not the recipe that happens to share its name.
        InstallMode::Repo => binaries
            .iter()
            .map(|(name, entry)| Ok((name.clone(), binary_candidate_of(entry)?)))
            .collect(),
        InstallMode::Auto | InstallMode::Ask => {
            let mut candidates = candidates_of(known);
            for (name, entry) in binaries {
                match known.get(name) {
                    Some(recipe) => {
                        let recipe_version = full(&recipe.package.version, recipe.package.rel);
                        let repository_version = full(&entry.version, entry.rel);
                        if crate::domain::package::version::vercmp(
                            &repository_version,
                            &recipe_version,
                        ) != std::cmp::Ordering::Less
                        {
                            // Auto can fetch this archive or fall back to the
                            // recipe; Ask can choose either source. Preserve a
                            // recipe action for the executor, but plan a
                            // conservative union of both possible closures.
                            let mut candidate = candidate_of(recipe);
                            let binary = binary_candidate_of(entry)?;
                            candidate.version = binary.version;
                            candidate.rel = binary.rel;
                            let mut seen: HashSet<String> =
                                candidate.deps.iter().cloned().collect();
                            for dependency in binary.deps {
                                if seen.insert(dependency.clone()) {
                                    candidate.deps.push(dependency);
                                }
                            }
                            candidate.dependencies_known = binary.dependencies_known;
                            // The selected artifact may change on fallback.
                            // Only explain a provider when both source
                            // declarations agree exactly.
                            if candidate.provides != binary.provides {
                                candidate.provides = Provides::Unknown;
                            }
                            candidates.insert(name.clone(), candidate);
                        }
                    }
                    None => {
                        candidates.insert(name.clone(), binary_candidate_of(entry)?);
                    }
                }
            }
            Ok(candidates)
        }
    }
}

/// Installed packages whose candidate is newer: `(installed entry, recipe)`,
/// alphabetical. Packages without a recipe are not "outdated", just unknown.
pub(crate) fn outdated<'a>(
    installed: &'a [Entry],
    known: &'a BTreeMap<String, Recipe>,
) -> Vec<(&'a Entry, &'a Recipe)> {
    let installed_full: Vec<(String, String)> = installed
        .iter()
        .map(|e| (e.name.clone(), full(&e.version, e.rel)))
        .collect();
    let by_name: HashMap<&str, &Entry> = installed.iter().map(|e| (e.name.as_str(), e)).collect();
    install::outdated(&installed_full, &candidates_of(known))
        .into_iter()
        .filter_map(|name| {
            let e = by_name.get(name.as_str())?;
            let r = known.get(&name)?;
            Some((*e, r))
        })
        .collect()
}

/// Dependency packages nothing needs any more.
///
/// A package is *kept* when it was installed explicitly, or is reachable
/// from a kept package through its recorded run dependencies. Unless
/// `build_deps` is set, the **build** dependencies named by the recipes of
/// kept packages are kept too — in a source distribution the compiler and
/// the -dev libraries are what the next rebuild needs.
pub(crate) fn orphans(
    installed: &[Entry],
    known: &BTreeMap<String, Recipe>,
    build_deps: bool,
) -> Vec<String> {
    let pkgs: Vec<InstalledPkg> = installed
        .iter()
        .map(|e| InstalledPkg {
            name: e.name.clone(),
            is_dependency: e.reason == "dependency",
            run: e.run.clone(),
        })
        .collect();
    let known_deps: BTreeMap<String, Vec<String>> = known
        .iter()
        .map(|(name, rec)| (name.clone(), deps_of(rec).collect()))
        .collect();
    removal::orphans(&pkgs, &known_deps, build_deps)
}

/// Order `names` so that a package is removed before the packages it
/// depends on (dependents first) — `remove` refuses to break a live
/// dependency. Anything that cannot be ordered (a cycle) goes last.
pub(crate) fn removal_order(names: &[String], installed: &[Entry]) -> Vec<String> {
    let set: HashSet<&str> = names.iter().map(|s| s.as_str()).collect();
    let run_edges: HashMap<String, Vec<String>> = installed
        .iter()
        .filter(|e| set.contains(e.name.as_str()))
        .map(|e| {
            (
                e.name.clone(),
                e.run
                    .iter()
                    .filter(|d| set.contains(d.as_str()))
                    .cloned()
                    .collect(),
            )
        })
        .collect();
    removal::removal_order(names, &run_edges)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(name: &str, ver: &str, build: &[&str], run: &[&str]) -> Recipe {
        let q = |l: &[&str]| l.iter().map(|d| format!("{d:?}")).collect::<Vec<_>>().join(", ");
        toml::from_str(&format!(
            "[package]\nname = \"{name}\"\nversion = \"{ver}\"\n\
             [depends]\nbuild = [{}]\nrun = [{}]\n[build]\ninstall = \"true\"\n",
            q(build),
            q(run)
        ))
        .unwrap()
    }

    fn tree(recs: Vec<Recipe>) -> BTreeMap<String, Recipe> {
        recs.into_iter().map(|r| (r.package.name.clone(), r)).collect()
    }

    fn entry(name: &str, ver: &str, reason: &str, run: &[&str]) -> Entry {
        Entry {
            name: name.into(),
            version: ver.into(),
            rel: 1,
            arch: String::new(),
            reason: reason.into(),
            run: run.iter().map(|s| s.to_string()).collect(),
            install_date: 0,
            size: 0,
            artifact_sha256: String::new(),
        }
    }

    fn inst(es: Vec<Entry>) -> HashMap<String, Entry> {
        es.into_iter().map(|e| (e.name.clone(), e)).collect()
    }

    fn names(p: &Plan) -> Vec<&str> {
        p.items.iter().map(|i| i.rec.package.name.as_str()).collect()
    }

    #[test]
    fn pulls_build_and_run_deps_in_order() {
        let known = tree(vec![
            rec("zlib", "1", &[], &[]),
            rec("curl", "1", &["zlib"], &["openssl"]),
            rec("openssl", "1", &["zlib"], &[]),
        ]);
        let p = install_plan(&["curl".into()], &HashMap::new(), &known, None, Why::Explicit, true).unwrap();
        assert_eq!(names(&p), ["zlib", "openssl", "curl"]);
        assert_eq!(p.items[0].why, Why::Dependency);
        assert_eq!(p.items[2].why, Why::Explicit);
    }

    #[test]
    fn installed_deps_are_skipped_and_not_walked() {
        let known = tree(vec![rec("zlib", "1", &[], &[]), rec("curl", "1", &["zlib"], &[])]);
        let have = inst(vec![entry("zlib", "0.5", "explicit", &[])]);
        let p = install_plan(&["curl".into()], &have, &known, None, Why::Explicit, true).unwrap();
        assert_eq!(names(&p), ["curl"], "an installed (even older) dep is satisfied");
    }

    #[test]
    fn up_to_date_target_is_reported_not_rebuilt() {
        let known = tree(vec![rec("zlib", "1", &[], &[])]);
        let have = inst(vec![entry("zlib", "1", "explicit", &[])]);
        let p = install_plan(&["zlib".into()], &have, &known, None, Why::Explicit, true).unwrap();
        assert!(p.items.is_empty());
        assert_eq!(p.up_to_date, ["zlib"]);
    }

    #[test]
    fn older_installed_target_becomes_a_replacement() {
        let known = tree(vec![rec("zlib", "1.3", &[], &[])]);
        let have = inst(vec![entry("zlib", "1.2", "explicit", &[])]);
        let p = install_plan(&["zlib".into()], &have, &known, None, Why::Explicit, true).unwrap();
        assert_eq!(p.items[0].replaces.as_deref(), Some("1.2-1"));
    }

    #[test]
    fn no_deps_plans_only_the_target() {
        let known = tree(vec![rec("zlib", "1", &[], &[]), rec("curl", "1", &["zlib"], &[])]);
        let p = install_plan(&["curl".into()], &HashMap::new(), &known, None, Why::Explicit, false).unwrap();
        assert_eq!(names(&p), ["curl"]);
    }

    #[test]
    fn typo_gets_a_suggestion_and_cycles_are_errors() {
        let known = tree(vec![rec("zlib", "1", &[], &[])]);
        let e = install_plan(&["zlb".into()], &HashMap::new(), &known, None, Why::Explicit, true).unwrap_err();
        assert!(e.to_string().contains("zlib"), "{e}");

        let cyc = tree(vec![rec("a", "1", &["b"], &[]), rec("b", "1", &[], &["a"])]);
        let e = install_plan(
            &["a".into()],
            &HashMap::new(),
            &cyc,
            None,
            Why::Explicit,
            true,
        )
        .unwrap_err();
        assert!(e.to_string().contains("a -> b -> a"), "{e}");
    }

    #[test]
    fn missing_dependency_names_who_needs_it() {
        let known = tree(vec![rec("a", "1", &["ghost"], &[])]);
        let e = install_plan(
            &["a".into()],
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
    fn outdated_compares_version_and_rel() {
        let known = tree(vec![rec("a", "1.10", &[], &[]), rec("b", "1", &[], &[])]);
        let have = vec![entry("a", "1.9", "explicit", &[]), entry("b", "1", "explicit", &[]), entry("c", "1", "explicit", &[])];
        let o = outdated(&have, &known);
        assert_eq!(o.len(), 1);
        assert_eq!(o[0].0.name, "a");
    }

    #[test]
    fn orphans_keep_run_deps_and_build_deps_of_kept() {
        let known = tree(vec![
            rec("app", "1", &["gcc"], &["libx"]),
            rec("libx", "1", &[], &[]),
            rec("gcc", "1", &[], &[]),
        ]);
        let have = vec![
            entry("app", "1", "explicit", &["libx"]),
            entry("libx", "1", "dependency", &[]),
            entry("gcc", "1", "dependency", &[]),
            entry("stale", "1", "dependency", &[]),
        ];
        assert_eq!(orphans(&have, &known, false), ["stale"], "build deps of kept packages stay");
        let mut both = orphans(&have, &known, true);
        both.sort();
        assert_eq!(both, ["gcc", "stale"], "--build-deps frees them");
    }

    #[test]
    fn group_direct_members_are_roots_for_autoremove() {
        let known = tree(vec![
            rec("dev-tool", "1", &[], &["shared-lib"]),
            rec("shared-lib", "1", &[], &[]),
        ]);
        let have = vec![
            entry("dev-tool", "1", "group", &["shared-lib"]),
            entry("shared-lib", "1", "dependency", &[]),
            entry("stale", "1", "dependency", &[]),
        ];
        assert_eq!(orphans(&have, &known, false), ["stale"]);
    }

    #[test]
    fn group_install_reason_is_distinct_from_explicit() {
        let known = tree(vec![rec("dev-tool", "1", &[], &[])]);
        let plan = install_plan(&["dev-tool".into()], &HashMap::new(), &known, None, Why::Group, true).unwrap();
        assert_eq!(plan.items[0].why.db_reason(), crate::adapters::db::InstallReason::Group);
        assert_eq!(plan.items[0].why.label(), "group");
    }

    #[test]
    fn removal_order_puts_dependents_first() {
        let have = vec![
            entry("lib", "1", "dependency", &[]),
            entry("mid", "1", "dependency", &["lib"]),
            entry("top", "1", "dependency", &["mid"]),
        ];
        let order = removal_order(&["lib".into(), "mid".into(), "top".into()], &have);
        assert_eq!(order, ["top", "mid", "lib"]);
    }
}
