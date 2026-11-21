//! Dependency resolution: DFS over the recipe graph with topological
//! ordering and cycle detection.
//!
//! The order is the order packages must be *installed* in, so a package's
//! runtime requirements come before it as well as its build dependencies:
//! archives that declare relations (schema 5) are verified against the
//! installed state when they are installed, and `rust` cannot be installed
//! before the `gcc` it requires at run time.

use anyhow::{bail, Result};
use std::collections::{BTreeMap, HashSet};

use crate::domain::package::relations::Requirement;
use crate::recipe::Recipe;

/// Resolve build order for `targets` given the set of installed packages.
/// Returns recipes in dependency order (deps first, targets last).
/// Unknown names get a "did you mean" hint built from the known graph.
pub(crate) fn resolve(
    targets: &[String],
    installed: &HashSet<String>,
    known: &BTreeMap<String, Recipe>,
) -> Result<Vec<Recipe>> {
    let mut order = Vec::new();
    let mut visiting: HashSet<String> = HashSet::new();
    let mut done: HashSet<String> = installed.clone();

    for t in targets {
        if !known.contains_key(t) && !installed.contains(t) {
            bail!(
                "unknown package: `{}`{}",
                t,
                crate::suggest::hint(t, &known.keys().cloned().collect::<Vec<_>>())
            );
        }
        visit(t, known, &mut done, &mut visiting, &mut order)?;
    }
    Ok(order)
}

fn visit(
    name: &str,
    known: &BTreeMap<String, Recipe>,
    done: &mut HashSet<String>,
    visiting: &mut HashSet<String>,
    order: &mut Vec<Recipe>,
) -> Result<()> {
    if done.contains(name) {
        return Ok(());
    }
    if !visiting.insert(name.to_string()) {
        bail!("circular dependency detected while resolving `{}`", name);
    }
    let rec = match known.get(name) {
        Some(r) => r,
        None => bail!(
            "dependency `{}` is neither installed nor has a recipe{}",
            name,
            crate::suggest::hint(name, &known.keys().cloned().collect::<Vec<_>>())
        ),
    };
    for dep in rec.build_deps() {
        visit(&dep, known, done, visiting, order)?;
    }
    // Runtime requirements are ordered, never enforced as a graph property:
    // a name without a recipe (external, virtual) has nothing to build, and a
    // run-time cycle is not an error (build-time cycles above still are) — the
    // edge that would close it is dropped so a mutual runtime pair keeps
    // resolving exactly as it did before run dependencies were ordered.
    for raw in &rec.depends.run {
        let Ok(requirement) = Requirement::parse(raw) else {
            continue;
        };
        let dep = requirement.name;
        if dep == name
            || done.contains(&dep)
            || visiting.contains(&dep)
            || !known.contains_key(&dep)
        {
            continue;
        }
        visit(&dep, known, done, visiting, order)?;
    }
    visiting.remove(name);
    done.insert(name.to_string());
    order.push(rec.clone());
    Ok(())
}

/// Like [`resolve`] for ONE explicitly requested package: its own
/// dependencies are walked even when the package itself is installed.
///
/// `resolve` seeds `done` with the installed set, the target included, so for
/// an installed target it returns nothing and never looks at its dependencies.
/// That is wrong for a package that ships in the rootfs but is being rebuilt
/// (glibc): the tools its recipe needs (bison) are not necessarily installed.
/// Here the target stays in `done` while its dependencies are visited — a
/// dependency that leads back to it counts as satisfied instead of a cycle —
/// and it is listed last, like any other target.
#[cfg(test)]
pub(crate) fn resolve_target(
    name: &str,
    installed: &HashSet<String>,
    known: &BTreeMap<String, Recipe>,
) -> Result<Vec<Recipe>> {
    if !installed.contains(name) {
        return resolve(&[name.to_string()], installed, known);
    }
    // installed from somewhere else (seed, third party): nothing to resolve
    let Some(rec) = known.get(name) else {
        return Ok(Vec::new());
    };
    let mut order = Vec::new();
    let mut visiting: HashSet<String> = HashSet::new();
    let mut done: HashSet<String> = installed.clone();
    for dep in rec.build_deps() {
        visit(&dep, known, &mut done, &mut visiting, &mut order)?;
    }
    for raw in &rec.depends.run {
        if let Ok(requirement) = Requirement::parse(raw) {
            if requirement.name != name && known.contains_key(&requirement.name) {
                visit(
                    &requirement.name,
                    known,
                    &mut done,
                    &mut visiting,
                    &mut order,
                )?;
            }
        }
    }
    order.push(rec.clone());
    Ok(order)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(name: &str, deps: &[&str]) -> Recipe {
        // tiny helper: build recipes via TOML
        let deps_list: Vec<String> = deps.iter().map(|s| s.to_string()).collect();
        let toml_src = format!(
            r#"
            [package]
            name = "{name}"
            version = "1.0"

            [depends]
            build = [{}]

            [build]
            install = "true"
            "#,
            deps_list
                .iter()
                .map(|d| format!("{:?}", d))
                .collect::<Vec<_>>()
                .join(", ")
        );
        toml::from_str(&toml_src).unwrap()
    }

    #[test]
    fn orders_dependencies() {
        let known: BTreeMap<String, Recipe> = [
            ("gmp", rec("gmp", &[])),
            ("mpfr", rec("mpfr", &["gmp"])),
            ("gcc", rec("gcc", &["mpfr", "gmp"])),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();

        let order = resolve(&["gcc".to_string()], &HashSet::new(), &known).unwrap();
        let names: Vec<&str> = order.iter().map(|r| r.package.name.as_str()).collect();
        assert_eq!(names, vec!["gmp", "mpfr", "gcc"]);
    }

    fn rec_with_run(name: &str, build: &[&str], run: &[&str]) -> Recipe {
        let list = |items: &[&str]| {
            items
                .iter()
                .map(|d| format!("{:?}", d))
                .collect::<Vec<_>>()
                .join(", ")
        };
        toml::from_str(&format!(
            r#"
            [package]
            name = "{name}"
            version = "1.0"

            [depends]
            build = [{}]
            run = [{}]

            [build]
            install = "true"
            "#,
            list(build),
            list(run)
        ))
        .unwrap()
    }

    #[test]
    fn orders_runtime_requirements_before_the_package() {
        // rust has no build dependencies but needs gcc and glibc at run time:
        // a package that declares relations is verified on install, so gcc
        // must already be installed. Alphabetical/BTree order alone puts rust
        // first.
        let known: BTreeMap<String, Recipe> = [
            ("glibc", rec_with_run("glibc", &[], &[])),
            ("gcc", rec_with_run("gcc", &["glibc"], &["glibc"])),
            ("rust", rec_with_run("rust", &[], &["gcc", "glibc"])),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
        let targets: Vec<String> = known.keys().cloned().collect();
        let order = resolve(&targets, &HashSet::new(), &known).unwrap();
        assert_eq!(names(&order), vec!["glibc", "gcc", "rust"]);
    }

    #[test]
    fn runtime_requirements_accept_version_constraints() {
        let known: BTreeMap<String, Recipe> = [
            ("base-files", rec_with_run("base-files", &[], &[])),
            ("app", rec_with_run("app", &[], &["base-files >= 0.4-12"])),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
        let order = resolve(&["app".to_string()], &HashSet::new(), &known).unwrap();
        assert_eq!(names(&order), vec!["base-files", "app"]);
    }

    #[test]
    fn unknown_runtime_names_are_not_errors() {
        // an external or virtual requirement has no recipe: nothing to order
        let known: BTreeMap<String, Recipe> =
            [("app", rec_with_run("app", &[], &["not-a-recipe"]))]
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect();
        let order = resolve(&["app".to_string()], &HashSet::new(), &known).unwrap();
        assert_eq!(names(&order), vec!["app"]);
    }

    #[test]
    fn runtime_cycles_are_tolerated_but_build_cycles_still_fail() {
        let runtime: BTreeMap<String, Recipe> = [
            ("a", rec_with_run("a", &[], &["b"])),
            ("b", rec_with_run("b", &[], &["a"])),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
        let order = resolve(
            &["a".to_string(), "b".to_string()],
            &HashSet::new(),
            &runtime,
        )
        .unwrap();
        assert_eq!(order.len(), 2);

        let build: BTreeMap<String, Recipe> = [
            ("a", rec_with_run("a", &["b"], &[])),
            ("b", rec_with_run("b", &["a"], &[])),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
        assert!(resolve(&["a".to_string()], &HashSet::new(), &build).is_err());
    }

    #[test]
    fn installed_runtime_requirements_are_not_rebuilt() {
        let known: BTreeMap<String, Recipe> = [
            ("gcc", rec_with_run("gcc", &[], &[])),
            ("rust", rec_with_run("rust", &[], &["gcc"])),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
        let installed: HashSet<String> = ["gcc".to_string()].into_iter().collect();
        let order = resolve(&["rust".to_string()], &installed, &known).unwrap();
        assert_eq!(names(&order), vec!["rust"]);
    }

    #[test]
    fn detects_cycles() {
        let known: BTreeMap<String, Recipe> = [("a", rec("a", &["b"])), ("b", rec("b", &["a"]))]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect();

        assert!(resolve(&["a".to_string()], &HashSet::new(), &known).is_err());
    }

    #[test]
    fn missing_dep_errors() {
        let known: BTreeMap<String, Recipe> = [("a", rec("a", &["ghost"]))]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect();
        assert!(resolve(&["a".to_string()], &HashSet::new(), &known).is_err());
    }

    fn names(order: &[Recipe]) -> Vec<&str> {
        order.iter().map(|r| r.package.name.as_str()).collect()
    }

    fn graph(list: &[(&str, &[&str])]) -> BTreeMap<String, Recipe> {
        list.iter()
            .map(|(n, d)| (n.to_string(), rec(n, d)))
            .collect()
    }

    fn set(list: &[&str]) -> HashSet<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn installed_target_still_walks_its_missing_deps() {
        // glibc ships in the rootfs, bison does not: `deps glibc` must name bison
        let known = graph(&[("m4", &[]), ("bison", &["m4"]), ("glibc", &["bison"])]);
        let order = resolve_target("glibc", &set(&["glibc"]), &known).unwrap();
        assert_eq!(names(&order), vec!["m4", "bison", "glibc"]);
    }

    #[test]
    fn installed_target_with_installed_deps_lists_only_itself() {
        let known = graph(&[("bison", &[]), ("glibc", &["bison"])]);
        let order = resolve_target("glibc", &set(&["glibc", "bison"]), &known).unwrap();
        assert_eq!(names(&order), vec!["glibc"]);
    }

    #[test]
    fn dependency_leading_back_to_installed_target_is_not_a_cycle() {
        let known = graph(&[("a", &["b"]), ("b", &["a"])]);
        let order = resolve_target("a", &set(&["a"]), &known).unwrap();
        assert_eq!(names(&order), vec!["b", "a"]);
    }

    #[test]
    fn installed_target_without_recipe_resolves_to_nothing() {
        let known = graph(&[]);
        assert!(resolve_target("seedpkg", &set(&["seedpkg"]), &known)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn uninstalled_target_resolves_like_resolve() {
        let known = graph(&[("gmp", &[]), ("mpfr", &["gmp"])]);
        let order = resolve_target("mpfr", &set(&["gmp"]), &known).unwrap();
        assert_eq!(names(&order), vec!["mpfr"]);
        assert!(resolve_target("ghost", &set(&[]), &known).is_err());
    }

    #[test]
    fn full_tree_resolves_without_cycles() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("recipes");
        if !root.exists() {
            return; // running outside the repo checkout
        }
        let all = crate::recipe::load_all(&root).unwrap();
        assert!(
            all.len() >= 100,
            "expected the full recipe tree, got {}",
            all.len()
        );
        let targets: Vec<String> = all.keys().cloned().collect();
        let order = resolve(&targets, &HashSet::new(), &all).unwrap();
        let mut names: Vec<String> = order.iter().map(|r| r.package.name.clone()).collect();
        names.sort();
        assert_eq!(
            names.len(),
            all.len(),
            "every recipe must appear exactly once"
        );
        let dup = names.iter().zip(names.iter().skip(1)).find(|(a, b)| a == b);
        assert!(dup.is_none(), "duplicate entries in resolution order");
    }
}
