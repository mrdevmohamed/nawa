//! Package groups (roadmap Phase 5).
//!
//! A group is metadata that resolves to ordinary package names — never a
//! special archive, never a path around the dependency planner. Group
//! definitions come from authenticated repository metadata; local membership
//! records under `<root>/var/lib/nawa/groups/installed/` remember what the
//! user actually chose (a later remote definition change must not silently
//! alter an installed group).

pub mod membership;

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// Group ids become file names under `var/lib/nawa/groups/installed/`, so
/// they use the same conservative alphabet as package names.
pub fn check_group_id(id: &str) -> Result<()> {
    if !crate::db::valid_name(id) {
        bail!(
            "invalid group id `{}` (allowed: letters, digits and + _ . -)",
            id.escape_debug()
        );
    }
    Ok(())
}

/// One published group definition (authenticated through the repository
/// index reference, roadmap §9.2). Constructed by the group command layer
/// (Phase 5 CLI).
#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupDefinition {
    pub schema: u32,
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// Required by the group definition.
    #[serde(default)]
    pub mandatory: Vec<String>,
    /// Installed by default when installing the group.
    #[serde(default)]
    pub default: Vec<String>,
    /// Available for explicit selection (`--with-optional`).
    #[serde(default)]
    pub optional: Vec<String>,
}

#[allow(dead_code)]
impl GroupDefinition {
    /// Validate the whole definition: schema, ids, package names and the
    /// no-duplicates-within-a-category rule (duplicates ACROSS categories
    /// are normalized: mandatory wins over default wins over optional).
    pub fn validate(&self) -> Result<()> {
        if self.schema != 1 {
            bail!("unsupported group schema {} (expected 1)", self.schema);
        }
        check_group_id(&self.id)?;
        if self.name.trim().is_empty() {
            bail!("group {} has an empty display name", self.id);
        }
        for list_name in ["mandatory", "default", "optional"] {
            let list = match list_name {
                "mandatory" => &self.mandatory,
                "default" => &self.default,
                "optional" => &self.optional,
                _ => unreachable!(),
            };
            let mut seen = BTreeSet::new();
            for p in list {
                crate::db::check_name(p)
                    .with_context(|| format!("group {} has an invalid {list_name} member", self.id))?;
                if !seen.insert(p.clone()) {
                    bail!("group {} lists `{p}` twice in {list_name}", self.id);
                }
            }
        }
        Ok(())
    }

    pub fn from_toml(raw: &str) -> Result<Self> {
        let g: GroupDefinition = toml::from_str(raw).context("corrupt group definition")?;
        g.validate()?;
        Ok(g)
    }

    /// The effective package set for an install: mandatory + default
    /// (+ optional only with `--with-optional`), duplicates normalized.
    pub fn effective_packages(&self, with_optional: bool) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let mut seen = BTreeSet::new();
        let lists: [&[String]; 3] = if with_optional {
            [&self.mandatory, &self.default, &self.optional]
        } else {
            [&self.mandatory, &self.default, &[]]
        };
        for list in lists {
            for p in list {
                if seen.insert(p.clone()) {
                    out.push(p.clone());
                }
            }
        }
        out
    }
}

use anyhow::Context as _;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_and_members_are_validated() {
        assert!(check_group_id("development").is_ok());
        assert!(check_group_id("../evil").is_err());
        assert!(check_group_id("").is_err());

        let good = r#"
schema = 1
id = "development"
name = "Development Tools"
description = "Compilers and build tools"
mandatory = ["gcc", "make"]
default = ["clang", "cmake", "git"]
optional = ["gdb", "ninja"]
"#;
        let g = GroupDefinition::from_toml(good).unwrap();
        assert_eq!(g.effective_packages(false).len(), 5);
        assert_eq!(g.effective_packages(true).len(), 7);

        // duplicate inside one list is rejected
        let dup = good.replace("\"clang\", \"cmake\"", "\"clang\", \"clang\"");
        assert!(GroupDefinition::from_toml(&dup).is_err());
        // path-like member is rejected
        let evil = good.replace("\"gdb\"", "\"../evil\"");
        assert!(GroupDefinition::from_toml(&evil).is_err());
        // bad schema is rejected
        let bad = good.replace("schema = 1", "schema = 2");
        assert!(GroupDefinition::from_toml(&bad).is_err());
    }

    #[test]
    fn cross_category_duplicates_are_normalized() {
        let g = GroupDefinition {
            schema: 1,
            id: "mix".into(),
            name: "Mixed".into(),
            description: String::new(),
            mandatory: vec!["gcc".into()],
            default: vec!["gcc".into(), "git".into()],
            optional: vec!["git".into()],
        };
        g.validate().unwrap();
        assert_eq!(g.effective_packages(false), vec!["gcc".to_string(), "git".to_string()]);
        assert_eq!(g.effective_packages(true), vec!["gcc".to_string(), "git".to_string()]);
    }
}
