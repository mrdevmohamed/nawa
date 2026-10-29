//! Planning for `nawa install <name>`, `nawa update`, `nawa outdated` and
//! `nawa autoremove`: pure functions over the recipe graph and the local
//! database, so every decision is unit-testable without building anything.
//!
//! Dependency edges followed when installing by name are `depends.build`
//! (needed to compile) **and** `depends.run` (needed to work). Whatever is
//! already installed counts as satisfied and its own dependencies are not
//! walked — the same rule `nawa deps` uses.

use anyhow::{bail, Result};
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use crate::db::Entry;
use crate::recipe::Recipe;
use crate::version::vercmp;

/// Why a package is part of a plan; decides the `reason` recorded in the db.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Why {
    /// named on the command line
    Explicit,
    /// pulled in as a dependency of something else
    Dependency,
    /// an installed package being brought to the recipe's version
    /// (keeps whatever reason it already had)
    Update,
}

impl Why {
    /// the `reason` argument of `install::install_archive`
    pub fn db_reason(self) -> &'static str {
        match self {
            Why::Explicit => "explicit",
            Why::Dependency => "dependency",
            Why::Update => "keep",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Why::Explicit => "explicit",
            Why::Dependency => "dependency",
            Why::Update => "update",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Item {
    pub rec: Recipe,
    pub why: Why,
    /// `version-rel` currently installed, when this replaces something
    pub replaces: Option<String>,
}

#[derive(Debug, Default)]
pub struct Plan {
    /// build + install order: dependencies first
    pub items: Vec<Item>,
    /// requested targets that need nothing (already at the recipe's version
    /// or newer) — reported, not rebuilt
    pub up_to_date: Vec<String>,
}

fn full(version: &str, rel: u32) -> String {
    format!("{version}-{rel}")
}

fn recipe_full(r: &Recipe) -> String {
    full(&r.package.version, r.package.rel)
}

/// Is the recipe strictly newer than what is installed?
fn newer_than_installed(r: &Recipe, e: &Entry) -> bool {
    vercmp(&recipe_full(r), &full(&e.version, e.rel)) == Ordering::Greater
}

/// Everything that must exist before `rec` can be built and used.
fn deps_of(rec: &Recipe) -> impl Iterator<Item = &String> {
    rec.depends.build.iter().chain(rec.depends.run.iter())
}

/// Plan the installation of `targets`.
///
/// * `target_why` — `Explicit` for `nawa install`, `Update` for `nawa update`
/// * `with_deps`  — follow dependencies (`--no-deps` turns this off)
pub fn install_plan(
    targets: &[String],
    installed: &HashMap<String, Entry>,
    known: &BTreeMap<String, Recipe>,
    target_why: Why,
    with_deps: bool,
) -> Result<Plan> {
    let wanted: HashSet<&str> = targets.iter().map(|s| s.as_str()).collect();
    let mut plan = Plan::default();
    let mut done: HashSet<String> = HashSet::new();
    let mut visiting: Vec<String> = Vec::new();

    for t in targets {
        visit(t, &wanted, installed, known, target_why, with_deps, &mut done, &mut visiting, &mut plan)?;
    }
    Ok(plan)
}

#[allow(clippy::too_many_arguments)]
fn visit(
    name: &str,
    wanted: &HashSet<&str>,
    installed: &HashMap<String, Entry>,
    known: &BTreeMap<String, Recipe>,
    target_why: Why,
    with_deps: bool,
    done: &mut HashSet<String>,
    visiting: &mut Vec<String>,
    plan: &mut Plan,
) -> Result<()> {
    if done.contains(name) {
        return Ok(());
    }
    let is_target = wanted.contains(name);
    let rec = match known.get(name) {
        Some(r) => r,
        None if installed.contains_key(name) && !is_target => {
            // installed from somewhere else (seed, third-party repo): satisfied
            done.insert(name.to_string());
            return Ok(());
        }
        None => {
            let names: Vec<String> = known.keys().cloned().collect();
            let hint = crate::suggest::hint(name, &names);
            if is_target {
                bail!("no recipe for `{name}`{hint}");
            }
            let chain = visiting.join(" -> ");
            bail!("dependency `{name}` (needed by {chain}) is neither installed nor has a recipe{hint}");
        }
    };

    // an installed package only needs work when it is a requested target and
    // the recipe is newer; installed dependencies are simply satisfied
    let current = installed.get(name);
    if let Some(e) = current {
        if !is_target || !newer_than_installed(rec, e) {
            if is_target {
                plan.up_to_date.push(name.to_string());
            }
            done.insert(name.to_string());
            return Ok(());
        }
    }

    if visiting.iter().any(|v| v == name) {
        let mut cycle = visiting.clone();
        cycle.push(name.to_string());
        bail!("circular dependency: {}", cycle.join(" -> "));
    }
    visiting.push(name.to_string());
    if with_deps {
        for dep in deps_of(rec) {
            visit(dep, wanted, installed, known, target_why, with_deps, done, visiting, plan)?;
        }
    }
    visiting.pop();

    done.insert(name.to_string());
    plan.items.push(Item {
        rec: rec.clone(),
        why: if is_target { target_why } else { Why::Dependency },
        replaces: current.map(|e| full(&e.version, e.rel)),
    });
    Ok(())
}

/// Installed packages whose recipe is newer: `(installed entry, recipe)`,
/// alphabetical. Packages without a recipe are not "outdated", just unknown.
pub fn outdated<'a>(
    installed: &'a [Entry],
    known: &'a BTreeMap<String, Recipe>,
) -> Vec<(&'a Entry, &'a Recipe)> {
    installed
        .iter()
        .filter_map(|e| known.get(&e.name).map(|r| (e, r)))
        .filter(|(e, r)| newer_than_installed(r, e))
        .collect()
}

/// Dependency packages nothing needs any more.
///
/// A package is *kept* when it was installed explicitly, or is reachable
/// from a kept package through its recorded run dependencies. Unless
/// `build_deps` is set, the **build** dependencies named by the recipes of
/// kept packages are kept too — in a source distribution the compiler and
/// the -dev libraries are what the next rebuild needs.
pub fn orphans(
    installed: &[Entry],
    known: &BTreeMap<String, Recipe>,
    build_deps: bool,
) -> Vec<String> {
    let by_name: HashMap<&str, &Entry> = installed.iter().map(|e| (e.name.as_str(), e)).collect();
    let mut keep: BTreeSet<String> = BTreeSet::new();
    let mut stack: Vec<String> = installed
        .iter()
        .filter(|e| e.reason != "dependency")
        .map(|e| e.name.clone())
        .collect();
    while let Some(n) = stack.pop() {
        if !keep.insert(n.clone()) {
            continue;
        }
        if let Some(e) = by_name.get(n.as_str()) {
            stack.extend(e.run.iter().filter(|d| by_name.contains_key(d.as_str())).cloned());
        }
        if !build_deps {
            if let Some(r) = known.get(&n) {
                stack.extend(deps_of(r).filter(|d| by_name.contains_key(d.as_str())).cloned());
            }
        }
    }
    installed
        .iter()
        .filter(|e| e.reason == "dependency" && !keep.contains(&e.name))
        .map(|e| e.name.clone())
        .collect()
}

/// Order `names` so that a package is removed before the packages it
/// depends on (dependents first) — `remove` refuses to break a live
/// dependency. Anything that cannot be ordered (a cycle) goes last.
pub fn removal_order(names: &[String], installed: &[Entry]) -> Vec<String> {
    let set: HashSet<&str> = names.iter().map(|s| s.as_str()).collect();
    let run: HashMap<&str, Vec<&str>> = installed
        .iter()
        .filter(|e| set.contains(e.name.as_str()))
        .map(|e| (e.name.as_str(), e.run.iter().map(|s| s.as_str()).filter(|d| set.contains(d)).collect()))
        .collect();
    let mut left: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
    let mut out: Vec<String> = Vec::new();
    while !left.is_empty() {
        // removable now: no remaining package still needs it
        let pick: Vec<&str> = left
            .iter()
            .copied()
            .filter(|n| !left.iter().any(|o| o != n && run.get(o).is_some_and(|d| d.contains(n))))
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
        let p = install_plan(&["curl".into()], &HashMap::new(), &known, Why::Explicit, true).unwrap();
        assert_eq!(names(&p), ["zlib", "openssl", "curl"]);
        assert_eq!(p.items[0].why, Why::Dependency);
        assert_eq!(p.items[2].why, Why::Explicit);
    }

    #[test]
    fn installed_deps_are_skipped_and_not_walked() {
        let known = tree(vec![rec("zlib", "1", &[], &[]), rec("curl", "1", &["zlib"], &[])]);
        let have = inst(vec![entry("zlib", "0.5", "explicit", &[])]);
        let p = install_plan(&["curl".into()], &have, &known, Why::Explicit, true).unwrap();
        assert_eq!(names(&p), ["curl"], "an installed (even older) dep is satisfied");
    }

    #[test]
    fn up_to_date_target_is_reported_not_rebuilt() {
        let known = tree(vec![rec("zlib", "1", &[], &[])]);
        let have = inst(vec![entry("zlib", "1", "explicit", &[])]);
        let p = install_plan(&["zlib".into()], &have, &known, Why::Explicit, true).unwrap();
        assert!(p.items.is_empty());
        assert_eq!(p.up_to_date, ["zlib"]);
    }

    #[test]
    fn older_installed_target_becomes_a_replacement() {
        let known = tree(vec![rec("zlib", "1.3", &[], &[])]);
        let have = inst(vec![entry("zlib", "1.2", "explicit", &[])]);
        let p = install_plan(&["zlib".into()], &have, &known, Why::Explicit, true).unwrap();
        assert_eq!(p.items[0].replaces.as_deref(), Some("1.2-1"));
    }

    #[test]
    fn no_deps_plans_only_the_target() {
        let known = tree(vec![rec("zlib", "1", &[], &[]), rec("curl", "1", &["zlib"], &[])]);
        let p = install_plan(&["curl".into()], &HashMap::new(), &known, Why::Explicit, false).unwrap();
        assert_eq!(names(&p), ["curl"]);
    }

    #[test]
    fn typo_gets_a_suggestion_and_cycles_are_errors() {
        let known = tree(vec![rec("zlib", "1", &[], &[])]);
        let e = install_plan(&["zlb".into()], &HashMap::new(), &known, Why::Explicit, true).unwrap_err();
        assert!(e.to_string().contains("zlib"), "{e}");

        let cyc = tree(vec![rec("a", "1", &["b"], &[]), rec("b", "1", &[], &["a"])]);
        let e = install_plan(&["a".into()], &HashMap::new(), &cyc, Why::Explicit, true).unwrap_err();
        assert!(e.to_string().contains("a -> b -> a"), "{e}");
    }

    #[test]
    fn missing_dependency_names_who_needs_it() {
        let known = tree(vec![rec("a", "1", &["ghost"], &[])]);
        let e = install_plan(&["a".into()], &HashMap::new(), &known, Why::Explicit, true).unwrap_err();
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
