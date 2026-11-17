//! `query`: read-side package questions. Use cases return the data; the
//! command handlers keep formatting, exit codes and the palette. All of
//! these are read-only over the installed database and the recipes tree —
//! the caller holds the shared db lock taken by the dispatch layer.

use anyhow::Result;
use std::collections::BTreeMap;
use std::path::Path;

use crate::db::{self, Entry};
use crate::recipe::Recipe;

/// What `nawa why <name>` reports about the installed database.
pub(crate) struct WhyData {
    /// the installed entry for `name`, if any
    pub(crate) entry: Option<Entry>,
    /// installed packages whose run dependencies include `name`
    pub(crate) dependents: Vec<String>,
}

/// `nawa why <name>`: the installed entry plus everything that requires it.
pub(crate) fn why(root: &Path, name: &str) -> Result<WhyData> {
    let db = db::Db::new(root);
    let entry = db.entry(name)?;
    let dependents = db
        .installed()?
        .into_iter()
        .filter(|e| e.run.iter().any(|r| r == name))
        .map(|e| e.name)
        .collect();
    Ok(WhyData { entry, dependents })
}

/// The recipe-tree half of `nawa why --tree`: every other recipe that
/// depends on `name`, with the dependency kinds that pull it in.
pub(crate) fn recipe_dependents(
    name: &str,
    known: &BTreeMap<String, Recipe>,
) -> Vec<(String, Vec<&'static str>)> {
    let mut rdeps: Vec<(String, Vec<&'static str>)> = Vec::new();
    for (n, r) in known {
        if n == name {
            continue;
        }
        let mut kinds: Vec<&'static str> = Vec::new();
        if r.build_deps().iter().any(|d| d == name) {
            kinds.push("build");
        }
        if r.depends.run.iter().any(|d| d == name) {
            kinds.push("run");
        }
        if !kinds.is_empty() {
            rdeps.push((n.clone(), kinds));
        }
    }
    rdeps
}

/// `nawa search <term>`: recipes whose name — or, without `--glob`, whose
/// summary — matches case-insensitively. With `--glob` the term is a glob
/// (`*`, `?`) over the name. Returned in recipes-tree order, which the
/// handler prints as-is.
pub(crate) fn search_matches<'a>(
    known: &'a BTreeMap<String, Recipe>,
    term: &str,
    glob: bool,
) -> Vec<(&'a str, &'a Recipe)> {
    let needle = term.to_lowercase();
    known
        .iter()
        .filter(|(name, rec)| {
            let nl = name.to_lowercase();
            if glob {
                crate::glob::glob_match(&needle, &nl)
            } else {
                nl.contains(&needle) || rec.package.summary.to_lowercase().contains(&needle)
            }
        })
        .map(|(name, rec)| (name.as_str(), rec))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(name: &str, ver: &str, summary: &str, build: &[&str], run: &[&str]) -> Recipe {
        let q = |l: &[&str]| {
            l.iter()
                .map(|d| format!("{d:?}"))
                .collect::<Vec<_>>()
                .join(", ")
        };
        toml::from_str(&format!(
            "[package]\nname = \"{name}\"\nversion = \"{ver}\"\nsummary = \"{summary}\"\n\
             [depends]\nbuild = [{}]\nrun = [{}]\n[build]\ninstall = \"true\"\n",
            q(build),
            q(run)
        ))
        .unwrap()
    }

    fn tree(recs: Vec<Recipe>) -> BTreeMap<String, Recipe> {
        recs.into_iter()
            .map(|r| (r.package.name.clone(), r))
            .collect()
    }

    #[test]
    fn search_matches_names_summaries_and_globs() {
        let known = tree(vec![
            rec("curl", "1.0", "command line URL transfers", &[], &[]),
            rec("curlies", "1.0", "unrelated summary", &[], &[]),
            rec("wget", "1.0", "curl-like downloader", &[], &[]),
        ]);

        // substring over the name and the summary, case-insensitive
        let hits = search_matches(&known, "CURL", false);
        assert_eq!(
            hits.iter().map(|(n, _)| *n).collect::<Vec<_>>(),
            ["curl", "curlies", "wget"]
        );
        // summary-only hit
        let hits = search_matches(&known, "downloader", false);
        assert_eq!(hits.iter().map(|(n, _)| *n).collect::<Vec<_>>(), ["wget"]);
        // glob matches the name only
        let hits = search_matches(&known, "curl*", true);
        assert_eq!(
            hits.iter().map(|(n, _)| *n).collect::<Vec<_>>(),
            ["curl", "curlies"]
        );
        let hits = search_matches(&known, "curl", true);
        assert_eq!(hits.iter().map(|(n, _)| *n).collect::<Vec<_>>(), ["curl"]);
        assert!(search_matches(&known, "nothing-matches", false).is_empty());
    }

    #[test]
    fn recipe_dependents_reports_both_kinds_and_skips_self() {
        let known = tree(vec![
            rec("curl", "1.0", "", &[], &[]),
            rec("app", "1.0", "", &["curl"], &["curl"]),
            rec("lib", "1.0", "", &[], &["curl"]),
            rec("other", "1.0", "", &["lib"], &[]),
        ]);
        let rdeps = recipe_dependents("curl", &known);
        assert_eq!(
            rdeps,
            vec![
                ("app".to_string(), vec!["build", "run"]),
                ("lib".to_string(), vec!["run"]),
            ]
        );
    }

    #[test]
    fn why_reads_the_installed_entry_and_its_dependents() {
        let root = crate::testutil::tmpdir("query-why");
        let db = db::Db::new(&root);
        let digest_a = "a".repeat(64);
        let digest_b = "b".repeat(64);
        let digest_c = "c".repeat(64);
        let mut curl = crate::testutil::entry("curl", "1.0", &digest_a);
        curl.reason = "explicit".to_string();
        let mut app = crate::testutil::entry("app", "2.0", &digest_b);
        app.reason = "dependency".to_string();
        app.run = vec!["curl".to_string()];
        let mut other = crate::testutil::entry("other", "3.0", &digest_c);
        other.run = vec!["wget".to_string()];
        for e in [&curl, &app, &other] {
            db.add(e, &[], &std::collections::HashMap::new()).unwrap();
        }

        let data = why(&root, "curl").unwrap();
        assert_eq!(data.entry.as_ref().unwrap().version, "1.0");
        assert_eq!(data.dependents, ["app"]);

        let missing = why(&root, "zlib").unwrap();
        assert!(missing.entry.is_none());
        assert!(missing.dependents.is_empty());
    }
}
