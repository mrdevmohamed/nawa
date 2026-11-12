//! Ordering helpers.

use std::collections::{BTreeMap, HashSet};

use crate::deps;
use crate::recipe;

pub(crate) fn order_with_fallback(
    names: &[String],
    installed: &HashSet<String>,
    tree: &BTreeMap<String, recipe::Recipe>,
) -> Vec<String> {
    let planned: HashSet<&String> = names.iter().collect();
    let satisfied: HashSet<String> =
        installed.iter().filter(|n| !planned.contains(*n)).cloned().collect();
    let known: Vec<String> = names.iter().filter(|n| tree.contains_key(*n)).cloned().collect();
    let mut ordered: Vec<String> = match deps::resolve(&known, &satisfied, tree) {
        Ok(order) => order
            .into_iter()
            .map(|r| r.package.name)
            .filter(|n| planned.contains(n))
            .collect(),
        Err(_) => {
            crate::nawa_warn!("recipe graph incomplete for this plan — alphabetical order");
            let mut k = known.clone();
            k.sort();
            k
        }
    };
    let mut unknown: Vec<String> =
        names.iter().filter(|n| !tree.contains_key(*n)).cloned().collect();
    unknown.sort();
    ordered.extend(unknown);
    ordered
}

