//! The package metadata chain: recipe declaration → archive `.PKGINFO` →
//! authenticated duplication in the signed repository index (design §6.3).
//! The B3 schema is implemented and wired: recipe, archive and signed
//! index v4 adapters all consume this pure domain model, and provider
//! explanations consume authenticated metadata from that chain.
//!
//! The chain carries exactly one value per package, flowing recipe →
//! archive → index; no stage edits or re-interprets it (§6.3: recipe,
//! archive, and signed-index values must not become independent conflicting
//! authorities). A missing field means *unknown*, never "declares nothing"
//! (§6.3(4)–(5)): the empty and unknown states stay distinct in [`Provides`]
//! and [`RunDeps`].
//!
//! Spelling rules shared by all three stages:
//!
//! - a capability entry is `domain=identity`, normalized by
//!   `Capability::parse` (trimmed, lowercased);
//! - a list is a comma-joined single value in the flat `.PKGINFO` format and
//!   a string array in the recipe and index TOML formats — because the flat
//!   format uses `,` as its list separator, an entry may not contain a comma
//!   in *any* spelling (round-trip rule);
//! - an entry is declared at most once; duplicates are an error.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use super::relations::{normalize_requirements, PackageRelations, Requirement};
use crate::domain::system::capability::{Capability, Provides};

/// Normalize a list spelling once, at the boundary where a human or archive
/// declares it (design §6.3, the chain rules in
/// `docs/design/package-metadata-chain.md` §3–4): trim, reject empty entries,
/// reject commas (the flat `.PKGINFO` list separator) and reject duplicates.
/// Returns the entries in declaration order.
pub(crate) fn normalize_list(entries: &[String], field: &str) -> Result<Vec<String>, String> {
    let mut out: Vec<String> = Vec::new();
    for entry in entries {
        let entry = entry.trim();
        if entry.is_empty() {
            return Err(format!("{field} has an empty entry"));
        }
        if entry.contains(',') {
            return Err(format!(
                "{field} entry {entry:?} contains a comma (the .PKGINFO list separator)"
            ));
        }
        if out.iter().any(|e| e == entry) {
            return Err(format!("{field} declares {entry:?} twice"));
        }
        out.push(entry.to_string());
    }
    Ok(out)
}

/// Normalize and validate a `[capabilities] provides` declaration once, at
/// build time: `Capability::parse` lowercases and enforces `domain=identity`,
/// and the list rules above reject commas/duplicates/empties. The returned
/// keys are the exact strings written to `.PKGINFO` and the signed index.
pub(crate) fn normalize_provides(entries: &[String]) -> Result<Vec<String>, String> {
    let entries = normalize_list(entries, "provides")?;
    let mut out = Vec::with_capacity(entries.len());
    for entry in entries {
        let cap = Capability::parse(&entry).map_err(|e| e.to_string())?;
        let key = cap.to_key();
        if out.contains(&key) {
            return Err(format!(
                "provides declares {key:?} twice after normalization"
            ));
        }
        out.push(key);
    }
    Ok(out)
}

/// Normalize a `rundeps` declaration (package names, case-sensitive, trimmed).
pub(crate) fn normalize_rundeps(entries: &[String]) -> Result<Vec<String>, String> {
    normalize_requirements(entries, "rundeps")
}

/// The runtime-dependency declaration as carried by metadata (§6.3(5)): a
/// missing field in legacy metadata is [`RunDeps::Unknown`], **not** an
/// empty list. Entries are package names as declared (`depends.run`), in
/// declaration order; names are case-sensitive and never normalized beyond
/// trimming.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RunDeps {
    /// the field is present: possibly empty
    Declared(Vec<String>),
    /// legacy metadata without the field
    Unknown,
}

/// Normalized package metadata as consumed by the solver (§8.1): provided
/// capabilities and runtime dependencies, independent of which stage of the
/// chain they were read from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PkgMetadata {
    pub(crate) provides: Provides,
    pub(crate) rundeps: RunDeps,
    /// Absent in schema 1–4: unknown conflicts/replacements, never empty.
    pub(crate) relations: Option<PackageRelations>,
}

impl PkgMetadata {
    /// Revalidate a fully declared archive against the actual installed state.
    /// This checks a selected operation; it does not select packages or solve.
    pub(crate) fn verify_installed(
        &self,
        name: &str,
        installed: &HashMap<String, String>,
    ) -> Result<(), String> {
        let Some(relations) = &self.relations else {
            return Ok(());
        };
        if let RunDeps::Declared(deps) = &self.rundeps {
            for raw in deps {
                let requirement = Requirement::parse(raw)?;
                if requirement.name == name
                    || !installed
                        .get(&requirement.name)
                        .is_some_and(|v| requirement.matches(v))
                {
                    return Err(format!(
                        "installed packages do not satisfy runtime requirement {}",
                        requirement.canonical()
                    ));
                }
            }
        } else {
            return Err("declared relations require known runtime dependencies".into());
        }
        for raw in relations.conflicts.iter().chain(&relations.replaces) {
            let requirement = Requirement::parse(raw)?;
            if requirement.name == name
                || installed
                    .get(&requirement.name)
                    .is_some_and(|v| requirement.matches(v))
            {
                return Err(format!("package relation {} still matches an installed package; an explicitly reviewed replacement plan is required", requirement.canonical()));
            }
        }
        Ok(())
    }
    /// Metadata of a legacy record: an index v1–v3 entry or an archive
    /// written before the field existed (§6.3(4)–(5)). Both fields unknown;
    /// capabilities are never inferred from package names or file presence
    /// (§6.3(4)).
    pub(crate) fn unknown() -> Self {
        PkgMetadata {
            provides: Provides::Unknown,
            rundeps: RunDeps::Unknown,
            relations: None,
        }
    }

    /// Normalize the flat `.PKGINFO` key/value map (as returned by
    /// `archive::parse_pkginfo`). A field present with an empty value is an
    /// explicit empty declaration; a field absent is the unknown state of
    /// [`Self::unknown`]. Strict: the first malformed entry is an error,
    /// not a silent skip.
    pub(crate) fn from_pkginfo_map(info: &HashMap<String, String>) -> Result<Self, String> {
        let provides = match info.get("provides") {
            None => Provides::Unknown,
            Some(raw) => Provides::parse_all(&split_flat_list(raw, "provides")?).map_err(|e| {
                format!(
                    "{e} (.PKGINFO list fields are single comma-joined values: an entry \
                         cannot itself contain a comma)"
                )
            })?,
        };
        let rundeps = match info.get("rundeps") {
            None => RunDeps::Unknown,
            Some(raw) => RunDeps::Declared(normalize_rundeps(&split_flat_list(raw, "rundeps")?)?),
        };
        let relations = match (info.get("relations_schema"), info.get("conflicts"), info.get("replaces")) {
            (None, None, None) => None,
            (Some(schema), Some(conflicts), Some(replaces)) => Some(PackageRelations {
                schema: schema.parse().map_err(|_| "invalid .PKGINFO relations_schema")?,
                conflicts: split_flat_list(conflicts, "conflicts")?, replaces: split_flat_list(replaces, "replaces")?,
            }.normalized()?),
            _ => return Err("incomplete .PKGINFO package relations: schema, conflicts and replaces are all required".into()),
        };
        let metadata = PkgMetadata {
            provides,
            rundeps,
            relations,
        };
        metadata.validate_relations()?;
        Ok(metadata)
    }

    pub(crate) fn validate_relations(&self) -> Result<(), String> {
        if let Some(relations) = &self.relations {
            relations.normalized()?;
        }
        if let RunDeps::Declared(deps) = &self.rundeps {
            for dep in deps {
                if Requirement::parse(dep)?.comparison.is_some() && self.relations.is_none() {
                    return Err("versioned dependencies require an explicit package relations declaration and signed index v5".into());
                }
            }
        }
        Ok(())
    }
}

/// Build normalized metadata from the optional index-entry fields. `None`
/// (legacy v1–v3) maps to *unknown*, never to an empty declaration; `Some`
/// (schema v4) normalizes the entries exactly once (§7).
pub(crate) fn metadata_from_options(
    rundeps: Option<&Vec<String>>,
    provides: Option<&Vec<String>>,
) -> Result<PkgMetadata, String> {
    if rundeps.is_none() && provides.is_none() {
        return Ok(PkgMetadata::unknown());
    }
    Ok(PkgMetadata {
        relations: None,
        provides: match provides {
            None => Provides::Unknown,
            Some(values) => Provides::parse_all(&normalize_provides(values)?)?,
        },
        rundeps: match rundeps {
            None => RunDeps::Unknown,
            Some(values) => RunDeps::Declared(normalize_rundeps(values)?),
        },
    })
}

pub(crate) fn metadata_from_repository(
    rundeps: Option<&Vec<String>>,
    provides: Option<&Vec<String>>,
    relations: Option<&PackageRelations>,
) -> Result<PkgMetadata, String> {
    let mut metadata = metadata_from_options(rundeps, provides)?;
    metadata.relations = relations.map(PackageRelations::normalized).transpose()?;
    metadata.validate_relations()?;
    Ok(metadata)
}

/// Split one flat-format list value into entries. `.PKGINFO` is a `key =
/// value` format per line; a list field is one single comma-joined value
/// (the build writes `depends.run.join(",")`), and repeated keys collapse,
/// so a list can only be spelled as one line. An empty value is an explicit
/// empty list. Strict: empty slots and duplicate entries are errors, so the
/// normalized model never carries them.
pub(crate) fn split_flat_list(value: &str, field: &'static str) -> Result<Vec<String>, String> {
    if value.trim().is_empty() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in value.split(',') {
        let entry = entry.trim();
        if entry.is_empty() {
            return Err(format!(
                ".PKGINFO {field} {value:?} has an empty entry (entries are comma-separated)"
            ));
        }
        if out.iter().any(|e| e == entry) {
            return Err(format!(
                ".PKGINFO {field} {value:?} declares {entry:?} twice"
            ));
        }
        out.push(entry.to_string());
    }
    Ok(out)
}

/// §6.3(3): a schema-v4 index duplicates the archive's `.PKGINFO` metadata
/// so Nawa can solve without downloading every archive; when the archive is
/// fetched and its digest verified, the duplicated fields must equal the
/// archive metadata. Each declared field compares exactly, as a sorted entry
/// list: declaration order is irrelevant, but a multiset difference is a
/// mismatch (a duplicate on one side only — publication and parsing reject
/// duplicates anyway).
///
/// A legacy index carries no duplicated fields ([`Provides::Unknown`] /
/// [`RunDeps::Unknown`]): there is nothing to compare and this returns `Ok`.
/// The archive's own metadata is still authenticated by the signed digest;
/// this check only guards the v4 duplication against tooling bugs, so a
/// mismatch is refused, never silently resolved by preferring one side.
pub(crate) fn check_chain(index: &PkgMetadata, archive: &PkgMetadata) -> Result<(), String> {
    match (&index.relations, &archive.relations) {
        (None, Some(_)) => return Err("archive declares package relations but the authenticated index leaves them unknown".into()),
        (Some(_), None) => return Err("index declares package relations but the archive .PKGINFO has no relations declaration".into()),
        _ => {}
    }
    if let (Some(expected), Some(actual)) = (&index.relations, &archive.relations) {
        if actual.normalized()? != expected.normalized()? {
            return Err("index and archive .PKGINFO disagree on package relations".into());
        }
    }
    if let Provides::Declared(expected) = &index.provides {
        let found = match &archive.provides {
            Provides::Unknown => {
                return Err(
                    "index declares provides but the archive .PKGINFO has no provides field"
                        .to_string(),
                )
            }
            Provides::Declared(found) => found,
        };
        let mut expected: Vec<String> = expected.iter().map(|c| c.to_key()).collect();
        let mut found: Vec<String> = found.iter().map(|c| c.to_key()).collect();
        expected.sort();
        found.sort();
        if expected != found {
            return Err(format!(
                "index and archive .PKGINFO disagree on provides: index [{}] != archive [{}]",
                expected.join(", "),
                found.join(", ")
            ));
        }
    }
    if let RunDeps::Declared(expected) = &index.rundeps {
        let found = match &archive.rundeps {
            RunDeps::Unknown => {
                return Err(
                    "index declares rundeps but the archive .PKGINFO has no rundeps field"
                        .to_string(),
                )
            }
            RunDeps::Declared(found) => found,
        };
        let mut expected = expected.clone();
        let mut found = found.clone();
        expected.sort();
        found.sort();
        if expected != found {
            return Err(format!(
                "index and archive .PKGINFO disagree on rundeps: index [{}] != archive [{}]",
                expected.join(", "),
                found.join(", ")
            ));
        }
    }
    Ok(())
}

/// The proposed schema-v4 index entry (`[[package]]` in the index document):
/// the schema-v2/v3 entry fields plus the required solving metadata `rundeps`
/// and `provides` (§6.3(3), §6.3(5)). Serialization follows `repo::model`:
/// `download_url` is written only when set, `installed_size` is omitted when
/// 0 (unknown).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct IndexEntryV4 {
    pub(crate) name: String,
    pub(crate) version: String,
    pub(crate) rel: u32,
    pub(crate) arch: String,
    pub(crate) file: String,
    pub(crate) sha256: String,
    /// Absolute HTTPS URL for distributed repositories (as in schema v3);
    /// the per-index `archive_location` rule is unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) download_url: Option<String>,
    /// Download size in bytes; v4 validation requires a real value.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub(crate) size: u64,
    /// Installed size in bytes, from the archive's `size` field; 0 = unknown.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub(crate) installed_size: u64,
    /// Runtime dependencies. Required: an explicit `[]` when the package has
    /// none (§6.3(5) — a missing field is a parse error, never `unknown`).
    pub(crate) rundeps: Vec<String>,
    /// Provided capability identities. Required, same rule as `rundeps`.
    pub(crate) provides: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) relations: Option<PackageRelations>,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

impl IndexEntryV4 {
    /// Parse one `[[package]]` entry from its TOML text and validate it.
    /// Strict: unknown fields are rejected (fail closed on future schema
    /// extensions, like the `[repository]` block), and a missing `rundeps`
    /// or `provides` is an error — never `unknown` (§6.3(5)).
    pub(crate) fn parse_toml(text: &str) -> Result<Self, String> {
        let entry: IndexEntryV4 =
            toml::from_str(text).map_err(|e| format!("invalid schema-v4 package entry: {e}"))?;
        entry.validate()?;
        Ok(entry)
    }

    /// The entry rules, mirroring the schema-v2/v3 entry validation
    /// (`repo::validate`) plus the v4 metadata rules.
    pub(crate) fn validate(&self) -> Result<(), String> {
        if self.name.trim().is_empty()
            || self.version.trim().is_empty()
            || self.arch.trim().is_empty()
        {
            return Err("package entry has an empty name, version, or arch".to_string());
        }
        if self.rel == 0 {
            return Err(format!("package {} has invalid rel 0", self.name));
        }
        if self.file.is_empty() {
            return Err(format!(
                "package {} has an empty archive file name",
                self.name
            ));
        }
        if self.sha256.len() != 64 || !self.sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(format!(
                "package {} has an invalid SHA-256 digest (expected 64 hexadecimal characters)",
                self.name
            ));
        }
        if self.size == 0 {
            return Err(format!("package {} has no archive size", self.name));
        }
        for (field, entries) in [("rundeps", &self.rundeps), ("provides", &self.provides)] {
            for entry in entries {
                if entry.is_empty() {
                    return Err(format!(
                        "package {} has an empty entry in {field}",
                        self.name
                    ));
                }
                if entry.contains(',') {
                    return Err(format!(
                        "package {} has a comma in a {field} entry {entry:?} (entries are \
                         separated by commas; an entry cannot itself contain one)",
                        self.name
                    ));
                }
            }
            if let Some(dup) = first_duplicate(entries) {
                return Err(format!(
                    "package {} declares {dup:?} twice in {field}",
                    self.name
                ));
            }
        }
        Provides::parse_all(&self.provides)
            .map_err(|e| format!("package {} has an invalid provides entry: {e}", self.name))?;
        Ok(())
    }

    /// Normalized metadata of the entry. A v4 entry always declares both
    /// fields — [`Provides::Unknown`] / [`RunDeps::Unknown`] never arise
    /// from a v4 record.
    pub(crate) fn metadata(&self) -> Result<PkgMetadata, String> {
        metadata_from_repository(
            Some(&self.rundeps),
            Some(&self.provides),
            self.relations.as_ref(),
        )
    }
}

fn first_duplicate(entries: &[String]) -> Option<&String> {
    for (i, entry) in entries.iter().enumerate() {
        if entries[..i].contains(entry) {
            return Some(entry);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pkginfo(entries: &[(&str, &str)]) -> HashMap<String, String> {
        entries
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn metadata_of(provides: &[&str], rundeps: &[&str]) -> PkgMetadata {
        let caps: Vec<String> = provides.iter().map(|s| s.to_string()).collect();
        let deps: Vec<String> = rundeps.iter().map(|s| s.to_string()).collect();
        PkgMetadata {
            provides: Provides::parse_all(&caps).unwrap(),
            rundeps: RunDeps::Declared(deps),
            relations: None,
        }
    }

    const SAMPLE_ENTRY: &str = r#"
name = "runit"
version = "2.1.2"
rel = 2
arch = "x86_64"
file = "runit-2.1.2-2-x86_64.nawa.tar.zst"
sha256 = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
size = 12345
installed_size = 67890
rundeps = ["glibc", "mesa"]
provides = ["init-system=runit"]
"#;

    /// Edit the sample entry TOML, failing the test if the edit did not
    /// apply (guards against a stale needle silently testing nothing).
    fn edited(from: &str, to: &str) -> String {
        let out = SAMPLE_ENTRY.replace(from, to);
        assert!(out != SAMPLE_ENTRY, "edit {from:?} did not apply");
        out
    }

    fn without_line(prefix: &str) -> String {
        assert!(SAMPLE_ENTRY.lines().any(|l| l.starts_with(prefix)));
        SAMPLE_ENTRY
            .lines()
            .filter(|l| !l.starts_with(prefix))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn missing_fields_are_unknown() {
        let md = PkgMetadata::from_pkginfo_map(&pkginfo(&[("pkgname", "runit")])).unwrap();
        assert_eq!(md, PkgMetadata::unknown());
        assert_eq!(md.provides, Provides::Unknown);
        assert_eq!(md.rundeps, RunDeps::Unknown);
    }

    #[test]
    fn relation_metadata_is_complete_normalized_and_compared_across_the_chain() {
        let info = pkginfo(&[
            ("rundeps", "lib >= 2-1"),
            ("provides", ""),
            ("relations_schema", "1"),
            ("conflicts", "old < 2-1"),
            ("replaces", "old<2-1"),
        ]);
        let metadata = PkgMetadata::from_pkginfo_map(&info).unwrap();
        assert_eq!(metadata.rundeps, RunDeps::Declared(vec!["lib>=2-1".into()]));
        let mut changed = metadata.clone();
        changed.relations.as_mut().unwrap().conflicts.clear();
        assert!(check_chain(&metadata, &changed).is_err());
        let mut incomplete = info;
        incomplete.remove("replaces");
        assert!(PkgMetadata::from_pkginfo_map(&incomplete).is_err());
        assert!(PkgMetadata::from_pkginfo_map(&pkginfo(&[("rundeps", "lib>=2-1")])).is_err());
    }

    #[test]
    fn empty_values_are_explicit_empty_declarations() {
        let md = PkgMetadata::from_pkginfo_map(&pkginfo(&[
            ("pkgname", "runit"),
            ("provides", ""),
            ("rundeps", ""),
        ]))
        .unwrap();
        assert_eq!(md.provides, Provides::Declared(Vec::new()));
        assert_eq!(md.rundeps, RunDeps::Declared(Vec::new()));
        // §6.3(5): present-but-empty is not the unknown state
        assert_ne!(md, PkgMetadata::unknown());
    }

    #[test]
    fn comma_joined_lists_parse_in_declaration_order() {
        let md = PkgMetadata::from_pkginfo_map(&pkginfo(&[
            ("provides", "session=foo,init-system=runit"),
            ("rundeps", "glibc, mesa"),
        ]))
        .unwrap();
        match md.provides {
            Provides::Declared(caps) => {
                assert_eq!(caps[0].to_key(), "session=foo");
                assert_eq!(caps[1].to_key(), "init-system=runit");
            }
            other => panic!("expected Declared, got {other:?}"),
        }
        assert_eq!(
            md.rundeps,
            RunDeps::Declared(vec!["glibc".to_string(), "mesa".to_string()])
        );
    }

    #[test]
    fn flat_lists_reject_empty_slots_and_duplicates() {
        for bad in [
            ("rundeps", "glibc,,mesa"),
            ("rundeps", "glibc,"),
            ("rundeps", ",glibc"),
            ("rundeps", "glibc,glibc"),
            ("provides", "a=b,,c=d"),
            ("provides", "a=b,a=b"),
        ] {
            let map = pkginfo(&[bad]);
            assert!(PkgMetadata::from_pkginfo_map(&map).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn malformed_capability_entry_errors_with_the_flat_format_rule() {
        let map = pkginfo(&[("provides", "init-system=runit,foo")]);
        let err = PkgMetadata::from_pkginfo_map(&map).unwrap_err();
        // "foo" became its own entry after the comma split
        assert!(err.contains("not `domain=identity`"), "{err}");
        assert!(err.contains("comma"), "{err}");
    }

    #[test]
    fn real_build_output_parses_with_provides_absent() {
        // The current build writes exactly these fields (`build/package.rs`
        // `package_archives`) and no `provides` yet: today's archives must
        // normalize to "rundeps declared, provides unknown".
        let md = PkgMetadata::from_pkginfo_map(&pkginfo(&[
            ("pkgname", "runit"),
            ("pkgver", "2.1.2"),
            ("pkgrel", "2"),
            ("arch", "x86_64"),
            ("rundeps", "glibc,mesa"),
            ("size", "67890"),
        ]))
        .unwrap();
        assert_eq!(md.provides, Provides::Unknown);
        assert_eq!(
            md.rundeps,
            RunDeps::Declared(vec!["glibc".to_string(), "mesa".to_string()])
        );
    }

    #[test]
    fn v4_entry_parses_and_round_trips() {
        let entry = IndexEntryV4::parse_toml(SAMPLE_ENTRY).unwrap();
        assert_eq!(entry.name, "runit");
        assert_eq!(entry.rel, 2);
        assert_eq!(entry.size, 12345);
        assert_eq!(entry.installed_size, 67890);

        let md = entry.metadata().unwrap();
        assert_eq!(
            md.provides,
            Provides::parse_all(&["init-system=runit".to_string()]).unwrap()
        );
        assert_eq!(
            md.rundeps,
            RunDeps::Declared(vec!["glibc".to_string(), "mesa".to_string()])
        );

        let reparsed = IndexEntryV4::parse_toml(&toml::to_string(&entry).unwrap()).unwrap();
        assert_eq!(entry, reparsed);
    }

    #[test]
    fn v4_entry_with_explicitly_empty_metadata_is_declared_not_unknown() {
        let text = edited(r#"rundeps = ["glibc", "mesa"]"#, "rundeps = []")
            .replace(r#"provides = ["init-system=runit"]"#, "provides = []");
        let entry = IndexEntryV4::parse_toml(&text).unwrap();
        let md = entry.metadata().unwrap();
        assert_eq!(md.provides, Provides::Declared(Vec::new()));
        assert_eq!(md.rundeps, RunDeps::Declared(Vec::new()));
    }

    #[test]
    fn v4_entry_requires_the_solving_fields() {
        // §6.3(5): a v4 record missing a solving field is a parse error,
        // never silently `unknown`.
        assert!(IndexEntryV4::parse_toml(&without_line("rundeps"))
            .unwrap_err()
            .contains("missing field `rundeps`"));
        assert!(IndexEntryV4::parse_toml(&without_line("provides"))
            .unwrap_err()
            .contains("missing field `provides`"));
        // `size` defaults to 0 and is then rejected by validation.
        assert!(IndexEntryV4::parse_toml(&without_line("size"))
            .unwrap_err()
            .contains("no archive size"));
    }

    #[test]
    fn legacy_entry_fails_closed_on_the_v4_reader() {
        // A v1–v3 entry (no metadata fields) is not readable as v4: legacy
        // readers keep their own path, and the legacy record normalizes to
        // [`PkgMetadata::unknown`] instead (§6.3(4)).
        let text = without_line("rundeps").replace(r#"provides = ["init-system=runit"]"#, "");
        assert!(IndexEntryV4::parse_toml(&text)
            .unwrap_err()
            .contains("missing field `rundeps`"));
    }

    #[test]
    fn v4_entry_rejects_unknown_fields_and_bad_values() {
        let cases = [
            (
                edited("size = 12345", "size = 12345\npin = true"),
                "unknown field",
            ),
            (edited("rel = 2", "rel = 0"), "invalid rel 0"),
            (
                edited(
                    "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
                    "nope",
                ),
                "SHA-256",
            ),
            (edited("size = 12345", "size = 0"), "no archive size"),
            (
                edited(
                    r#"provides = ["init-system=runit"]"#,
                    r#"provides = ["noequals"]"#,
                ),
                "not `domain=identity`",
            ),
            (
                edited(
                    r#"provides = ["init-system=runit"]"#,
                    r#"provides = ["a=b,extra"]"#,
                ),
                "comma",
            ),
            (
                edited(
                    r#"provides = ["init-system=runit"]"#,
                    r#"provides = ["a=b", "a=b"]"#,
                ),
                "twice",
            ),
            (
                edited(r#"rundeps = ["glibc", "mesa"]"#, r#"rundeps = [""]"#),
                "empty entry",
            ),
        ];
        for (text, expected) in cases {
            let err = IndexEntryV4::parse_toml(&text).unwrap_err();
            assert!(err.contains(expected), "{expected:?} not in {err}");
        }
    }

    #[test]
    fn chain_check_accepts_equal_metadata_in_any_order() {
        let index = metadata_of(&["a=b", "c=d"], &["glibc"]);
        let archive = metadata_of(&["C=D", "A=B"], &["glibc"]);
        assert!(check_chain(&index, &archive).is_ok());
    }

    #[test]
    fn chain_check_reports_field_mismatches() {
        let index = metadata_of(&["init-system=runit"], &["glibc"]);
        let archive = metadata_of(&["init-system=systemd"], &["glibc"]);
        let err = check_chain(&index, &archive).unwrap_err();
        assert!(err.contains("provides"), "{err}");
        assert!(err.contains("init-system=runit"), "{err}");

        let index = metadata_of(&["a=b"], &["glibc"]);
        let archive = metadata_of(&["a=b"], &["glibc", "mesa"]);
        let err = check_chain(&index, &archive).unwrap_err();
        assert!(err.contains("rundeps"), "{err}");
    }

    #[test]
    fn chain_check_is_vacuous_for_a_legacy_index() {
        // A legacy index duplicates nothing: no comparison, and the archive
        // metadata is authenticated by the signed digest itself (§6.3(3)).
        let archive = metadata_of(&["init-system=runit"], &["glibc"]);
        assert!(check_chain(&PkgMetadata::unknown(), &archive).is_ok());
    }

    #[test]
    fn chain_check_rejects_a_v4_index_against_an_archive_without_the_field() {
        // A v4 index promises the field; an archive lacking it is broken.
        let index = metadata_of(&["a=b"], &["glibc"]);
        let err = check_chain(&index, &PkgMetadata::unknown()).unwrap_err();
        assert!(err.contains("no provides field"), "{err}");

        let index = PkgMetadata {
            provides: Provides::Unknown,
            rundeps: RunDeps::Declared(vec!["glibc".to_string()]),
            relations: None,
        };
        let err = check_chain(&index, &PkgMetadata::unknown()).unwrap_err();
        assert!(err.contains("no rundeps field"), "{err}");
    }
}
