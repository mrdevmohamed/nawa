//! Placeholder expansion.

use anyhow::{bail, Result};

use super::model::*;

impl Recipe {

    /// Fully qualified version string: `name-version-rel`.
    pub fn id(&self) -> String {
        format!("{}-{}-{}", self.package.name, self.package.version, self.package.rel)
    }

    pub fn arch(&self) -> String {
        self.package
            .arch
            .first()
            .cloned()
            .unwrap_or_else(|| "x86_64".to_string())
    }

    /// Whether the recipe uses new-schema tables (fetch-phase.md §4, §19).
    /// In this revision: `[[package.sources]]`, `[assets]` and `[fetch.*]`.
    /// On a new-schema recipe
    /// `package.source` carries [`SCHEMA_MARKER`], `[[package.sources]]`
    /// takes the staging engine in `sources.rs` instead of the legacy path,
    /// and `[assets]` takes `assets.rs`.
    pub fn is_new_schema(&self) -> bool {
        !self.package.sources.is_empty()
            || !self.assets.include.is_empty()
            || self.fetch.any()
    }

    /// §14C: `%{srcroot}` / `NAWA_SRCROOT` and `%{assetsdir}` /
    /// `NAWA_ASSETSDIR` used by a recipe that does not enable the
    /// corresponding table are hard build-time errors, not silent
    /// expansions — legacy recipes keep their environment exactly as
    /// today, so the names never enter it. Scanned over the raw recipe
    /// bytes, so build sections *and* hooks are covered; a test that wants
    /// to check the environment names must spell them through shell string
    /// concatenation, or it trips this scan itself.
    pub fn check_schema_placeholders(&self) -> Result<()> {
        let enabled = [
            ("%{srcroot}", !self.package.sources.is_empty()),
            ("NAWA_SRCROOT", !self.package.sources.is_empty()),
            ("%{assetsdir}", !self.assets.include.is_empty()),
            ("NAWA_ASSETSDIR", !self.assets.include.is_empty()),
        ];
        for (needle, on) in enabled {
            if on {
                continue;
            }
            let is_srcroot = needle == "%{srcroot}" || needle == "NAWA_SRCROOT";
            let why = if is_srcroot {
                "the workspace-root placeholder only exists for recipes with \
                 [[package.sources]] (fetch-phase.md §4.3)"
            } else {
                "the assets snapshot placeholder only exists for recipes that declare \
                 [assets] (fetch-phase.md §5.3)"
            };
            if self.raw.contains(needle) {
                bail!(
                    "recipe {} uses `{needle}` but does not enable it — {why}; \
                     enable the table or drop the reference",
                    self.package.name
                );
            }
        }
        Ok(())
    }

    /// The build dependency list: the declared `depends.build` plus every
    /// implicit dependency the recipe's own tables require (e.g. the Go/Rust
    /// toolchains implied by a `[fetch.*]` table — fetch-phase.md §9),
    /// deduplicated.
    ///
    /// Every consumer of build dependencies — dependency resolution
    /// (`deps.rs`), closure assembly (`closure.rs`), install planning
    /// (`plan.rs`) and the `info`/`why`/`deps` queries — reads them through
    /// this one method, so an implicit dependency becomes visible everywhere
    /// at once instead of only in the build root.
    pub fn build_deps(&self) -> Vec<String> {
        let mut v = self.depends.build.clone();
        let push = |name: &str, v: &mut Vec<String>| {
            if !v.iter().any(|d| d == name) {
                v.push(name.to_string());
            }
        };
        // `[fetch.go]`/`[fetch.cargo]` imply their toolchain package; a
        // fetch table implies TLS support in the closure (§9). Declared
        // entries win — a recipe may already list the toolchain itself.
        if self.fetch.go.is_some() {
            push(GO_TOOLCHAIN, &mut v);
        }
        if self.fetch.cargo.is_some() {
            push(CARGO_TOOLCHAIN, &mut v);
        }
        if self.fetch.any() {
            push(CA_BUNDLE, &mut v);
        }
        v
    }

    /// Expand placeholders inside a build/hook script.
    ///
    /// `%{srcroot}` and `%{assetsdir}` expand only on recipes that enable
    /// the corresponding table (§4.3/§5.3): for every other recipe the
    /// placeholder is left alone — `build()` already refuses any raw recipe
    /// that references it (§14C) — so their environment and scripts stay
    /// byte-for-byte unchanged.
    pub fn expand(&self, script: &str, ctx: &ExpandCtx) -> String {
        let out = script
            .replace("%{name}", &self.package.name)
            .replace("%{version}", &self.package.version)
            .replace("%{rel}", &self.package.rel.to_string())
            .replace("%{arch}", &ctx.arch)
            .replace("%{jobs}", &ctx.jobs.to_string())
            .replace("%{pkgdir}", &ctx.pkgdir.to_string_lossy())
            .replace("%{srcdir}", &ctx.srcdir.to_string_lossy());
        let out = match &ctx.srcroot {
            Some(root) => out.replace("%{srcroot}", &root.to_string_lossy()),
            None => out,
        };
        match &ctx.assetsdir {
            Some(dir) => out.replace("%{assetsdir}", &dir.to_string_lossy()),
            None => out,
        }
    }
}
