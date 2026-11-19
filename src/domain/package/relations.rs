//! Package relations contract 1, carried by archive metadata and signed index 5.
//! One candidate per package is already selected by source/version policy; this
//! module defines comparisons, not another dependency resolver.

use serde::{Deserialize, Serialize};
use std::cmp::Ordering;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Requirement {
    pub(crate) name: String,
    pub(crate) comparison: Option<(String, String)>,
}

impl Requirement {
    pub(crate) fn parse(raw: &str) -> Result<Self, String> {
        let raw = raw.trim();
        let split = raw.find(['<', '>', '=', '!']);
        let (name, comparison) = match split {
            None => (raw, None),
            Some(at) => {
                let rest = &raw[at..];
                let operator = [">=", "<=", "!=", "=", ">", "<"]
                    .into_iter()
                    .find(|op| rest.starts_with(op))
                    .ok_or_else(|| format!("invalid package requirement {raw:?}"))?;
                let version = rest[operator.len()..].trim();
                let (base, rel) = version.rsplit_once('-').ok_or_else(|| {
                    format!("requirement {raw:?} needs an exact version-rel boundary")
                })?;
                if !valid_version(base) || rel.parse::<u32>().ok().filter(|r| *r > 0).is_none() {
                    return Err(format!("invalid version-rel in requirement {raw:?}"));
                }
                (
                    raw[..at].trim(),
                    Some((operator.to_string(), version.to_string())),
                )
            }
        };
        if !valid_name(name) {
            return Err(format!("invalid package name in requirement {raw:?}"));
        }
        Ok(Self {
            name: name.to_string(),
            comparison,
        })
    }

    pub(crate) fn canonical(&self) -> String {
        match &self.comparison {
            None => self.name.clone(),
            Some((op, v)) => format!("{}{op}{v}", self.name),
        }
    }

    pub(crate) fn matches(&self, version: &str) -> bool {
        let Some((operator, wanted)) = &self.comparison else {
            return true;
        };
        let order = super::version::vercmp(version, wanted);
        match operator.as_str() {
            "=" => order == Ordering::Equal,
            "!=" => order != Ordering::Equal,
            ">" => order == Ordering::Greater,
            "<" => order == Ordering::Less,
            ">=" => order != Ordering::Less,
            "<=" => order != Ordering::Greater,
            _ => false,
        }
    }
}

pub(crate) fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name.as_bytes()[0].is_ascii_alphanumeric()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"+_.-".contains(&b))
}

fn valid_version(version: &str) -> bool {
    !version.is_empty()
        && version.len() <= 128
        && version.as_bytes()[0].is_ascii_alphanumeric()
        && version
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"+_.-~:".contains(&b))
}

pub(crate) fn normalize_requirements(
    values: &[String],
    field: &str,
) -> Result<Vec<String>, String> {
    let mut result = Vec::new();
    for raw in values {
        let value = Requirement::parse(raw)?.canonical();
        if result.contains(&value) {
            return Err(format!(
                "{field} declares {value:?} twice after normalization"
            ));
        }
        result.push(value);
    }
    Ok(result)
}

pub(crate) fn depends_on(raw: &str, name: &str) -> bool {
    Requirement::parse(raw).is_ok_and(|requirement| requirement.name == name)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PackageRelations {
    pub(crate) schema: u32,
    /// Both fields are required. Missing is never an empty declaration.
    pub(crate) conflicts: Vec<String>,
    pub(crate) replaces: Vec<String>,
}

impl PackageRelations {
    pub(crate) fn normalized(&self) -> Result<Self, String> {
        if self.schema != 1 {
            return Err(format!(
                "unsupported package relations schema {}",
                self.schema
            ));
        }
        Ok(Self {
            schema: 1,
            conflicts: normalize_requirements(&self.conflicts, "conflicts")?,
            replaces: normalize_requirements(&self.replaces, "replaces")?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn requirement_comparison_uses_the_shared_version_order() {
        for (raw, have, expected) in [
            ("lib >= 2.0-2", "2.0-1", false),
            ("lib<2.0-1", "1.9-9", true),
            ("lib!=2-1", "2-1", false),
            ("lib=2-1", "2-1", true),
            ("lib", "9-1", true),
        ] {
            assert_eq!(
                Requirement::parse(raw).unwrap().matches(have),
                expected,
                "{raw}"
            );
        }
        assert_eq!(
            Requirement::parse(" lib >= 2-1 ").unwrap().canonical(),
            "lib>=2-1"
        );
    }
    #[test]
    fn malformed_or_ambiguous_relations_are_rejected() {
        for raw in [
            "",
            "../lib",
            "lib||other",
            "lib>=1",
            "lib==1-1",
            "lib!1-1",
            "lib>=1-0",
            "lib>=1-1,other",
            "lib=1-1 <2-1",
        ] {
            assert!(Requirement::parse(raw).is_err(), "{raw}");
        }
        assert!(
            normalize_requirements(&["lib >= 1-1".into(), "lib>=1-1".into()], "rundeps").is_err()
        );
        assert!(toml::from_str::<PackageRelations>("schema=1\nconflicts=[]").is_err());
    }
}
