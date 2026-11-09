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
    ///
    /// Single-pass scan (no chained `replace`): a substituted value that
    /// itself contains `%{…}` is never re-expanded, and `%%{name}` escapes
    /// to a literal `%{name}`. Bare `%%` (not followed by `{`) is left as
    /// `%%` so shell parameter expansion (`${var%%pat}`) is not broken.
    pub fn expand(&self, script: &str, ctx: &ExpandCtx) -> String {
        // Lookup table; `None` values (srcroot/assetsdir when not enabled)
        // mean "leave the placeholder literal".
        let srcroot_str = ctx.srcroot.as_ref().map(|p| p.to_string_lossy().into_owned());
        let assetsdir_str = ctx.assetsdir.as_ref().map(|p| p.to_string_lossy().into_owned());
        let pkgdir_str = ctx.pkgdir.to_string_lossy();
        let srcdir_str = ctx.srcdir.to_string_lossy();
        let jobs_str = ctx.jobs.to_string();
        let rel_str = self.package.rel.to_string();

        let mut out = String::with_capacity(script.len());
        let bytes = script.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            // `%%{` escape: emit `%{` literally, skip the extra `%`
            if bytes[i] == b'%' && bytes.get(i + 1) == Some(&b'%') && bytes.get(i + 2) == Some(&b'{') {
                out.push_str("%{");
                i += 2;
                continue;
            }
            // `%{key}` placeholder
            if bytes[i] == b'%' && bytes.get(i + 1) == Some(&b'{') {
                if let Some(end) = script[i + 2..].find('}') {
                    let key = &script[i + 2..i + 2 + end];
                    let value: Option<&str> = match key {
                        "name" => Some(&self.package.name),
                        "version" => Some(&self.package.version),
                        "rel" => Some(&rel_str),
                        "arch" => Some(&ctx.arch),
                        "jobs" => Some(&jobs_str),
                        "pkgdir" => Some(&pkgdir_str),
                        "srcdir" => Some(&srcdir_str),
                        "srcroot" => srcroot_str.as_deref(),
                        "assetsdir" => assetsdir_str.as_deref(),
                        _ => None,
                    };
                    if let Some(v) = value {
                        out.push_str(v);
                    } else {
                        // Unknown key or srcroot/assetsdir not enabled:
                        // leave `%{key}` literally (e.g. `build()` refuses
                        // the raw recipe later; expansion keeps it).
                        out.push_str(&script[i..i + 2 + end + 1]);
                    }
                    i += 2 + end + 1;
                    continue;
                }
            }
            // Regular byte: copy one char (UTF-8 safe: placeholder delimiters
            // are ASCII, so a multi-byte char can be copied byte-wise).
            let ch_len = utf8_len(bytes[i]);
            out.push_str(&script[i..i + ch_len]);
            i += ch_len;
        }
        out
    }
}

/// Length in bytes of the UTF-8 encoded char starting at `b`.
fn utf8_len(b: u8) -> usize {
    if b < 0x80 { 1 } else if b >> 5 == 0b110 { 2 } else if b >> 4 == 0b1110 { 3 } else { 4 }
}
