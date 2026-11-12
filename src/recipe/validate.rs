//! Recipe validation.

use anyhow::{bail, Context, Result};
use std::path::Path;

use super::model::*;

impl Recipe {
    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read recipe {}", path.display()))?;
        let recipe: Recipe =
            toml::from_str(&raw).with_context(|| format!("bad recipe {}", path.display()))?;
        recipe.validate()?;
        let mut recipe = recipe;
        recipe.raw = raw;
        // the recipe's own directory: the base for recipe-local assets
        // (file paths are resolved relative to it, never to the cwd)
        recipe.dir = path.parent().unwrap_or(path).to_path_buf();
        Ok(recipe)
    }

    pub(crate) fn validate(&self) -> Result<()> {
        if self.package.name.is_empty() {
            bail!("recipe has empty package.name");
        }
        if self.package.version.is_empty() {
            bail!("recipe {} has empty package.version", self.package.name);
        }
        // ---- new schema vs legacy (§4.1/§4.2, §19) — checked first, so
        // a mixed recipe reports the real problem instead of a legacy one
        self.validate_schema_marker()?;
        if self.is_new_schema() {
            self.validate_new_schema_sources()?;
        }
        self.validate_assets()?;
        self.validate_fetch_tables()?;
        self.validate_mirrors()?;
        // names/versions become directory and archive file names
        crate::db::check_name(&self.package.name)?;
        if !crate::db::valid_version(&self.package.version) {
            bail!(
                "recipe {} has an invalid package.version `{}`",
                self.package.name,
                self.package.version.escape_debug()
            );
        }
        Ok(())
    }

    /// `[package] workdir` needs `[[package.sources]]`, and the fail-closed
    /// schema marker in `package.source` must match the tables actually used.
    fn validate_schema_marker(&self) -> Result<()> {
        let new_schema = self.is_new_schema();
        let marker = self.package.source == SCHEMA_MARKER;
        if !self.package.workdir.is_empty() && self.package.sources.is_empty() {
            bail!(
                "recipe {} sets [package] workdir, which is valid only together with \
                 [[package.sources]] (fetch-phase.md §4.3)",
                self.package.name
            );
        }
        if new_schema && !marker {
            bail!(
                "recipe {} uses new-schema tables but package.source does not carry the \
                 fail-closed schema marker (fetch-phase.md §19):\n  {}",
                self.package.name,
                SCHEMA_MARKER
            );
        }
        if marker && !new_schema {
            bail!(
                "recipe {} sets the schema marker in package.source without using any \
                 new-schema table — either add [[package.sources]] or drop the marker \
                 (fetch-phase.md §19)",
                self.package.name
            );
        }
        Ok(())
    }

    /// New-schema recipes: no legacy fields mixed in, valid and unique
    /// `[[package.sources]]`, and a reproducible `SOURCE_DATE_EPOCH` input.
    fn validate_new_schema_sources(&self) -> Result<()> {
        if !self.package.sha256.is_empty()
            || !self.package.mirrors.is_empty()
            || !self.package.source_dir.is_empty()
        {
            bail!(
                "recipe {} must not mix the new [[package.sources]] form with legacy \
                 `source`, `sha256`, `mirrors` or `source_dir` (fetch-phase.md §4.2)",
                self.package.name
            );
        }
        let mut seen: Vec<&str> = Vec::new();
        for src in &self.package.sources {
            if !valid_source_name(&src.name) {
                bail!(
                    "recipe {} has an invalid source name `{}` — expected \
                     [a-z0-9][a-z0-9._-]*{{,64}} (fetch-phase.md §4.2)",
                    self.package.name,
                    src.name
                );
            }
            if seen.contains(&src.name.as_str()) {
                bail!(
                    "recipe {} has duplicate [[package.sources]] name `{}`",
                    self.package.name,
                    src.name
                );
            }
            seen.push(&src.name);
            if src.url.trim().is_empty() {
                bail!("recipe {} source `{}` has no url", self.package.name, src.name);
            }
            if !is_lower_hex64(&src.sha256) {
                bail!(
                    "recipe {} source `{}` has an invalid sha256 `{}` — expected exactly \
                     64 lowercase hex characters (fetch-phase.md §4.2)",
                    self.package.name,
                    src.name,
                    src.sha256
                );
            }
            if src.mirrors.iter().any(|m| m.trim().is_empty()) {
                bail!(
                    "recipe {} source `{}` has an empty entry in mirrors",
                    self.package.name,
                    src.name
                );
            }
            check_rel_path(
                "[[package.sources]] into",
                &src.into,
            )
            .with_context(|| {
                format!("recipe {} source `{}`", self.package.name, src.name)
            })?;
        }
        check_rel_path("[package] workdir", &self.package.workdir)?;
        // a build whose every input is a stamped plain file has no
        // reproducible mtime to derive the epoch from (§6)
        if !self.package.sources.is_empty()
            && self
                .package
                .sources
                .iter()
                .all(|s| s.unpack == Some(false))
            && self.package.source_date_epoch.is_none()
        {
            bail!(
                "recipe {} has only `unpack = false` sources and no source_date_epoch — \
                 a plain file's staging time would reach the archive; pin \
                 package.source_date_epoch (fetch-phase.md §6)",
                self.package.name
            );
        }
        // and with no external sources at all there is nothing the epoch
        // could be derived from: assets never feed it (§6 makes them
        // invisible to the derivation on purpose)
        if self.package.sources.is_empty() && self.package.source_date_epoch.is_none() {
            bail!(
                "recipe {} has no external sources to derive SOURCE_DATE_EPOCH from — \
                 assets and fetch views are excluded on purpose; pin \
                 package.source_date_epoch (fetch-phase.md §6)",
                self.package.name
            );
        }
        Ok(())
    }

    /// Declared `[assets] include` paths must stay inside the recipe directory.
    fn validate_assets(&self) -> Result<()> {
        // assets: declared paths must stay inside the recipe directory
        // (never absolute, no `..`, no trailing `/`) — checked at parse time
        // for the same reason as `into`/`workdir` (§5.2, §14)
        if !self.assets.include.is_empty() {
            for p in &self.assets.include {
                let kind = "[assets] include";
                if p.trim().is_empty() {
                    bail!("recipe {} has an empty entry in [assets] include", self.package.name);
                }
                check_rel_path(kind, p).with_context(|| {
                    format!("recipe {}", self.package.name)
                })?;
            }
        }
        Ok(())
    }

    /// `[fetch.go]` / `[fetch.cargo]` (§9).
    fn validate_fetch_tables(&self) -> Result<()> {
        // fetch.go / fetch.cargo (§9): `dir` is parsed like
        // `into`/`workdir`, and the toolchain recipes themselves must not
        // declare their own table (an implicit self-dependency).
        for (table, spec, toolchain, eco) in [
            ("[fetch.go]", self.fetch.go.as_ref(), GO_TOOLCHAIN, "go"),
            ("[fetch.cargo]", self.fetch.cargo.as_ref(), CARGO_TOOLCHAIN, "cargo"),
        ] {
            let Some(spec) = spec else { continue };
            check_rel_path(&format!("{table} dir"), &spec.dir).with_context(|| {
                format!("recipe {}", self.package.name)
            })?;
            if self.package.name == toolchain {
                bail!(
                    "recipe {} is the {eco} toolchain package and must not declare \
                     {table} — the table would depend on itself (fetch-phase.md §9)",
                    self.package.name
                );
            }
        }
        Ok(())
    }

    /// `package.mirrors` need a downloadable `package.source`.
    fn validate_mirrors(&self) -> Result<()> {
        if !self.package.mirrors.is_empty() {
            let src = self.package.source.as_str();
            if src.is_empty() || src.starts_with("local:") {
                bail!(
                    "recipe {} sets package.mirrors but has no downloadable package.source",
                    self.package.name
                );
            }
            if self.package.mirrors.iter().any(|m| m.trim().is_empty()) {
                bail!("recipe {} has an empty entry in package.mirrors", self.package.name);
            }
        }
        Ok(())
    }
}


/// Source names must match `[a-z0-9][a-z0-9._-]*` and are at most 64
/// characters (§4.2). Hand-rolled — the name greps cleanly out of error
/// messages, double-slashes and other path traps are caught here, and no
/// regex engine joins the dependency tree over this.
fn valid_source_name(name: &str) -> bool {
    let head_ok = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit();
    let tail_ok = |c: char| head_ok(c) || matches!(c, '.' | '_' | '-');
    match name.chars().next() {
        Some(c) if head_ok(c) => {
            name.chars().count() <= 64 && name.chars().skip(1).all(tail_ok)
        }
        _ => false,
    }
}

/// Exactly 64 lowercase hex characters (§4.2): the digest doubles as a cache
/// path component, so it must never carry `/` or `..` or any other shape.
pub(crate) fn is_lower_hex64(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// `into` / `workdir` are workspace-relative destinations (§4.2): never
/// absolute, never ending in `/`, never escaping with a `..` component.
/// Empty means the field's default.
fn check_rel_path(kind: &str, value: &str) -> Result<()> {
    if value.is_empty() {
        return Ok(());
    }
    if value.starts_with('/') {
        bail!("{kind} `{value}` must be relative to the source workspace (no leading `/`)");
    }
    if value.ends_with('/') {
        bail!("{kind} `{value}` must not end in `/`");
    }
    for c in value.split('/') {
        if c == ".." {
            bail!("{kind} `{value}` must not escape the source workspace");
        }
    }
    Ok(())
}
