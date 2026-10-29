//! Dependency resolution: DFS over the recipe graph with topological
//! ordering and cycle detection.

use anyhow::{bail, Result};
use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use crate::recipe::Recipe;

/// Resolve build order for `targets` given the set of installed packages.
/// Returns recipes in dependency order (deps first, targets last).
/// Unknown names get a "did you mean" hint built from the known graph.
pub fn resolve(
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
    for dep in &rec.depends.build {
        visit(dep, known, done, visiting, order)?;
    }
    visiting.remove(name);
    done.insert(name.to_string());
    order.push(rec.clone());
    Ok(())
}

/// Load all recipes under `root` and print/return the resolution order.
pub fn resolve_from_recipes(
    recipes_root: &Path,
    targets: &[String],
    installed: &HashSet<String>,
) -> Result<Vec<Recipe>> {
    let known = crate::recipe::load_all(recipes_root)?;
    resolve(targets, installed, &known)
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
            deps_list.iter().map(|d| format!("{:?}", d)).collect::<Vec<_>>().join(", ")
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

        let order =
            resolve(&["gcc".to_string()], &HashSet::new(), &known).unwrap();
        let names: Vec<&str> = order.iter().map(|r| r.package.name.as_str()).collect();
        assert_eq!(names, vec!["gmp", "mpfr", "gcc"]);
    }

    #[test]
    fn detects_cycles() {
        let known: BTreeMap<String, Recipe> = [
            ("a", rec("a", &["b"])),
            ("b", rec("b", &["a"])),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();

        assert!(resolve(&["a".to_string()], &HashSet::new(), &known).is_err());
    }

    #[test]
    fn missing_dep_errors() {
        let known: BTreeMap<String, Recipe> =
            [("a", rec("a", &["ghost"]))].into_iter().map(|(k, v)| (k.to_string(), v)).collect();
        assert!(resolve(&["a".to_string()], &HashSet::new(), &known).is_err());
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
        assert!(all.len() >= 100, "expected the full recipe tree, got {}", all.len());
        let targets: Vec<String> = all.keys().cloned().collect();
        let order = resolve(&targets, &HashSet::new(), &all).unwrap();
        let mut names: Vec<String> = order.iter().map(|r| r.package.name.clone()).collect();
        names.sort();
        assert_eq!(names.len(), all.len(), "every recipe must appear exactly once");
        let dup = names.iter().zip(names.iter().skip(1)).find(|(a, b)| a == b);
        assert!(dup.is_none(), "duplicate entries in resolution order");
    }
}
