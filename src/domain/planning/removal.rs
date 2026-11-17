//! Removal planning (design §8): which installed dependency packages are
//! orphans, and the order a removal set must be applied in. Pure decisions
//! over normalized inputs (§8.1 installed records); `plan.rs` adapts
//! database records and recipes and delegates.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

/// One installed package as the removal planner sees it.
#[derive(Debug, Clone)]
pub(crate) struct InstalledPkg {
    pub(crate) name: String,
    /// installed as a dependency (db reason `dependency`) — the only kind
    /// `autoremove` may remove
    pub(crate) is_dependency: bool,
    /// the run dependencies recorded in the database
    pub(crate) run: Vec<String>,
}

/// Dependency packages nothing needs any more: the names, in the order the
/// installed records were given.
///
/// A package is *kept* when it was not installed as a dependency, or is
/// reachable from a kept package through its recorded run dependencies.
/// Unless `build_deps` is set, the **build** dependencies named by the
/// candidate of a kept package are kept too — in a source distribution the
/// compiler and the -dev libraries are what the next rebuild needs.
///
/// `known_deps` maps a package name to its candidate's full dependency
/// list (build + run, declaration order). Only dependencies that are
/// themselves installed are walked.
pub(crate) fn orphans(
    installed: &[InstalledPkg],
    known_deps: &BTreeMap<String, Vec<String>>,
    build_deps: bool,
) -> Vec<String> {
    let installed_names: HashSet<&str> = installed.iter().map(|e| e.name.as_str()).collect();
    let by_name: HashMap<&str, &InstalledPkg> =
        installed.iter().map(|e| (e.name.as_str(), e)).collect();
    let mut keep: BTreeSet<String> = BTreeSet::new();
    let mut stack: Vec<String> = installed
        .iter()
        .filter(|e| !e.is_dependency)
        .map(|e| e.name.clone())
        .collect();
    while let Some(n) = stack.pop() {
        if !keep.insert(n.clone()) {
            continue;
        }
        if let Some(e) = by_name.get(n.as_str()) {
            stack.extend(
                e.run
                    .iter()
                    .filter(|d| installed_names.contains(d.as_str()))
                    .cloned(),
            );
        }
        if !build_deps {
            if let Some(deps) = known_deps.get(n.as_str()) {
                stack.extend(
                    deps.iter()
                        .filter(|d| installed_names.contains(d.as_str()))
                        .cloned(),
                );
            }
        }
    }
    installed
        .iter()
        .filter(|e| e.is_dependency && !keep.contains(&e.name))
        .map(|e| e.name.clone())
        .collect()
}

/// Order `names` so that a package is removed before the packages that
/// depend on it (dependents first) — `remove` refuses to break a live
/// dependency. Anything that cannot be ordered (a cycle) goes last.
///
/// `run_edges` maps each name to its recorded run dependencies *within the
/// removal set*: a package is removable when no remaining package still
/// needs it.
pub(crate) fn removal_order(
    names: &[String],
    run_edges: &HashMap<String, Vec<String>>,
) -> Vec<String> {
    let mut left: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
    let mut out: Vec<String> = Vec::new();
    while !left.is_empty() {
        // removable now: no remaining package still needs it
        let pick: Vec<&str> = left
            .iter()
            .copied()
            .filter(|n| {
                !left.iter().any(|o| {
                    o != n
                        && run_edges
                            .get(*o)
                            .is_some_and(|d| d.iter().any(|dep| dep.as_str() == *n))
                })
            })
            .collect();
        if pick.is_empty() {
            out.extend(left.iter().map(|s| s.to_string()));
            break;
        }
        for p in &pick {
            out.push(p.to_string());
        }
        left.retain(|n| !pick.contains(n));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pkg(name: &str, is_dependency: bool, run: &[&str]) -> InstalledPkg {
        InstalledPkg {
            name: name.to_string(),
            is_dependency,
            run: run.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn installed(list: Vec<InstalledPkg>) -> Vec<InstalledPkg> {
        list
    }

    fn deps(list: Vec<(&str, Vec<&str>)>) -> BTreeMap<String, Vec<String>> {
        list.into_iter()
            .map(|(n, d)| {
                (
                    n.to_string(),
                    d.into_iter().map(|s| s.to_string()).collect(),
                )
            })
            .collect()
    }

    fn order(names: &[&str], edges: Vec<(&str, Vec<&str>)>) -> Vec<String> {
        let edges: HashMap<String, Vec<String>> = edges
            .into_iter()
            .map(|(n, d)| {
                (
                    n.to_string(),
                    d.into_iter().map(|s| s.to_string()).collect(),
                )
            })
            .collect();
        let names: Vec<String> = names.iter().map(|s| s.to_string()).collect();
        removal_order(&names, &edges)
    }

    #[test]
    fn orphans_keep_run_deps_and_candidate_deps_of_kept() {
        let have = installed(vec![
            pkg("app", false, &["libx"]),
            pkg("libx", true, &[]),
            pkg("gcc", true, &[]),
            pkg("stale", true, &[]),
        ]);
        let known = deps(vec![("app", vec!["gcc", "libx"])]);
        assert_eq!(
            orphans(&have, &known, false),
            ["stale"],
            "build deps of kept packages stay"
        );
        assert_eq!(
            orphans(&have, &known, true),
            ["gcc", "stale"],
            "--build-deps frees them"
        );
    }

    #[test]
    fn orphan_reachability_is_transitive_through_recorded_run_deps() {
        let have = installed(vec![
            pkg("app", false, &["mid"]),
            pkg("mid", true, &["lib"]),
            pkg("lib", true, &[]),
            pkg("stale", true, &[]),
        ]);
        assert_eq!(orphans(&have, &BTreeMap::new(), false), ["stale"]);
    }

    #[test]
    fn orphans_walk_only_installed_candidate_deps() {
        // a kept package's candidate names a dep that is not installed:
        // there is no record to keep, so the walk skips it
        let have = installed(vec![pkg("app", false, &[]), pkg("stale", true, &[])]);
        let known = deps(vec![("app", vec!["ghost"])]);
        assert_eq!(
            orphans(&have, &known, false),
            ["stale"],
            "ghost is not installed, keeps nothing"
        );
    }

    #[test]
    fn installed_candidate_deps_are_kept_even_without_recorded_run_deps() {
        // reachability also follows the candidate's dependency list, not
        // just the recorded run deps
        let have = installed(vec![pkg("app", false, &[]), pkg("stale", true, &[])]);
        let known = deps(vec![("app", vec!["stale"])]);
        assert_eq!(orphans(&have, &known, false), Vec::<String>::new());
    }

    #[test]
    fn non_dependency_reasons_are_roots_even_without_run_deps() {
        let have = installed(vec![
            pkg("dev-tool", false, &[]),
            pkg("shared-lib", true, &[]),
            pkg("stale", true, &[]),
        ]);
        let known = deps(vec![("dev-tool", vec!["shared-lib"])]);
        assert_eq!(orphans(&have, &known, false), ["stale"]);
    }

    #[test]
    fn removal_order_puts_dependents_first() {
        let o = order(
            &["lib", "mid", "top"],
            vec![("mid", vec!["lib"]), ("top", vec!["mid"])],
        );
        assert_eq!(o, ["top", "mid", "lib"]);
    }

    #[test]
    fn a_cycle_goes_last_unordered() {
        let o = order(&["a", "b", "x"], vec![("a", vec!["b"]), ("b", vec!["a"])]);
        // x is removable first; the a/b cycle is appended as-is
        assert_eq!(o, ["x", "a", "b"]);
    }

    #[test]
    fn names_outside_the_edges_are_removable_immediately() {
        // no edges: every package is removable in the first round, in
        // input order
        let o = order(&["lib", "free"], vec![]);
        assert_eq!(o, ["lib", "free"]);
    }
}
