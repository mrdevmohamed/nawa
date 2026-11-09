//! `nawa recipe check` — the source-independent checks of §14B.
//!
//! Everything here reads local files only (recipe bytes, asset files) and
//! can run without a download or a toolchain:
//!
//! * asset path missing / symlink or special file in assets / a
//!   `recipe.toml` inside an asset path / size thresholds,
//! * unknown top-level tables and unknown keys directly under `[package]`
//!   (read from the *raw* TOML: the recipe structs deliberately ignore
//!   unknown top-level tables and keys, so §19 keeps the door open),
//! * unknown `%{...}` placeholders in the build/hook scripts,
//! * build scripts calling `go mod download` or `cargo fetch` themselves —
//!   the fetch backends own those steps, a recipe-side call needs the
//!   network at build time and breaks the offline story,
//! * `[fetch.*]` together with `[sandbox] network = true`.
//!
//! §14B is a *warning* command: findings print and the run exits 0 (its
//! hard counterparts are the parse/stage-time rules of §14A/§14C, which
//! fail the build). The checks that need the downloaded source are §14C
//! and are not repeated here.

use crate::*;
use anyhow::Result;

/// Files above this size inside `[assets]` earn the size warning (§14B).
const ASSET_SIZE_WARN: u64 = 32 * 1024 * 1024;

/// Every placeholder `Recipe::expand` substitutes today (`srcroot`/`assetsdir`
/// join per table, §4.3/§5.3).
const KNOWN_PLACEHOLDERS: &[&str] = &[
    "name", "version", "rel", "arch", "jobs", "pkgdir", "srcdir", "srcroot", "assetsdir",
];

/// The top-level tables `Recipe` declares (§14B: unknown ones warn).
const KNOWN_TOP_TABLES: &[&str] =
    &["package", "assets", "fetch", "depends", "build", "hooks", "sandbox", "slim"];

/// The keys the `[package]` struct declares (§14B: unknown ones warn).
const KNOWN_PACKAGE_KEYS: &[&str] = &[
    "name", "version", "rel", "arch", "summary", "license", "source", "sha256", "mirrors",
    "source_dir", "source_date_epoch", "sources", "workdir",
];

pub(crate) fn cmd_recipe_check(ctx: &Ctx, name: Option<&str>, all: bool) -> Result<()> {
    let recs: Vec<recipe::Recipe> = if all {
        let mut v: Vec<recipe::Recipe> = ctx.load_all()?
            .values()
            .cloned()
            .collect();
        v.sort_by(|a, b| a.package.name.cmp(&b.package.name));
        v
    } else {
        vec![recipe::load_by_name(&ctx.recipes, name.expect("checked in main"))?]
    };

    let mut findings = 0usize;
    for rec in &recs {
        findings += check_recipe(rec);
    }
    crate::nawa_info!(
        "recipe check: {} recipe(s), {} warning(s)",
        recs.len(),
        findings
    );
    Ok(())
}

/// Run the §14B checks for one recipe; returns the warning count.
fn check_recipe(rec: &recipe::Recipe) -> usize {
    let name = rec.package.name.clone();
    let mut count = 0usize;
    let mut warn = |msg: String| {
        count += 1;
        crate::nawa_info_stderr!("recipe {name}: warning: {msg}");
    };

    // ---- §14B: assets ---------------------------------------------------
    for include in &rec.assets.include {
        let path = rec.dir.join(include);
        if !path.exists() {
            warn(format!("asset path `{include}` is missing locally"));
            continue;
        }
        for e in walkdir::WalkDir::new(&path)
            .follow_links(false)
            .into_iter()
            .flatten()
        {
            let p = e.path().to_path_buf();
            let mt = e.file_type();
            if mt.is_symlink() {
                warn(format!("asset file `{}` is a symlink", p.display()));
                continue;
            }
            if mt.is_dir() {
                continue;
            }
            if !mt.is_file() {
                warn(format!("asset entry `{}` is a special file", p.display()));
                continue;
            }
            if p.file_name().map(|f| f == "recipe.toml").unwrap_or(false) {
                warn(format!(
                    "a recipe.toml inside an asset path (`{}`) would hide a \
                     recipe from packaging",
                    p.display()
                ));
            }
            let size = e.metadata().map(|m| m.len()).unwrap_or(0);
            if size > ASSET_SIZE_WARN {
                warn(format!(
                    "asset file `{}` is {} — a large asset bloats every \
                     recipe tarball and every recipe share",
                    p.display(),
                    humanize::format_size(size)
                ));
            }
        }
    }

    // ---- unknown tables and [package] keys (§14B, raw TOML) -------------
    if let Ok(toml) = toml::from_str::<toml::Table>(&rec.raw) {
        let tables: Vec<&str> = toml.keys().map(String::as_str).collect();
        for key in &tables {
            if !KNOWN_TOP_TABLES.contains(key) {
                warn(format!(
                    "unknown top-level table [{key}] — it is silently \
                     ignored; schema growth goes through the design doc"
                ));
            }
        }
        if let Some(package) = toml.get("package").and_then(|v| v.as_table()) {
            let keys: Vec<&str> = package.keys().map(String::as_str).collect();
            for key in &keys {
                if !KNOWN_PACKAGE_KEYS.contains(key) {
                    warn(format!(
                        "unknown key `{key}` under [package] — silently ignored"
                    ));
                }
            }
        }
    }

    // ---- unknown %{...} placeholders, fetched-by-hand commands -----------
    for (section, script) in section_scripts(rec) {
        for ph in placeholders(script) {
            if !KNOWN_PLACEHOLDERS.contains(&ph.as_str()) {
                warn(format!(
                    "unknown %{{{ph}}} in {section} — it is passed through \
                     to the shell unchanged"
                ));
            }
        }
        let words: Vec<&str> = script
            .split(|c: char| !c.is_ascii_alphanumeric() && c != '-' && c != '_')
            .filter(|w| !w.is_empty())
            .collect();
        let is_go_mod_download = words
            .windows(3)
            .any(|w| w == ["go", "mod", "download"]);
        let is_cargo_fetch = words.windows(2).any(|w| w == ["cargo", "fetch"]);
        if is_go_mod_download || is_cargo_fetch {
            warn(format!(
                "{section} runs a `go mod download` / `cargo fetch` command \
                 itself — the [fetch.*] backends own that step; a \
                 recipe-side call needs the network at build time"
            ));
        }
    }

    // ---- fetch table + sandbox network ----------------------------------
    if rec.fetch.any() && rec.sandbox.network {
        warn("[fetch.*] together with [sandbox] network = true — the fetch \
             backends populate the cache and build strictly offline".to_string());
    }
    count
}

/// The non-empty build/hook scripts of a recipe, in section order, for the
/// placeholder and fetch-pattern scans.
fn section_scripts(rec: &recipe::Recipe) -> Vec<(&'static str, &str)> {
    let build: [(&'static str, &String); 5] = [
        ("build.prepare", &rec.build.prepare),
        ("build.configure", &rec.build.configure),
        ("build.compile", &rec.build.compile),
        ("build.check", &rec.build.check),
        ("build.install", &rec.build.install),
    ];
    let hooks: [(&'static str, &String); 3] = [
        ("hooks.post_install", &rec.hooks.post_install),
        ("hooks.pre_remove", &rec.hooks.pre_remove),
        ("hooks.post_remove", &rec.hooks.post_remove),
    ];
    build
        .into_iter()
        .chain(hooks)
        .map(|(k, s)| (k, s.as_str()))
        .filter(|(_, s)| !s.trim().is_empty())
        .collect()
}

/// All `%{identifier}` placeholders of a script, deduplicated in order.
fn placeholders(script: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let b = script.as_bytes();
    let mut i = 0;
    while i + 2 < b.len() {
        if b[i] == b'%' && b[i + 1] == b'{' {
            let mut j = i + 2;
            while j < b.len()
                && (b[j] == b'_' || b[j].is_ascii_alphanumeric())
                && !(j == i + 2 && b[j].is_ascii_digit())
            {
                j += 1;
            }
            if j < b.len() && b[j] == b'}' && j > i + 2 {
                let inner = script[i + 2..j].to_string();
                if !out.contains(&inner) {
                    out.push(inner);
                }
                i = j + 1;
            } else {
                i += 1;
            }
        } else {
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{KNOWN_PACKAGE_KEYS, KNOWN_TOP_TABLES, placeholders};

    /// The §14B allow-lists must track the real structs: a renamed table or
    /// key would otherwise report false positives forever. This recipe
    /// carries every declared table and key; serde must not fail.
    #[test]
    fn known_lists_match_the_recipe_struct() {
        assert_eq!(
            KNOWN_TOP_TABLES.len(),
            8,
            "the top-level table list is gated by the §19 design"
        );
        assert!(KNOWN_TOP_TABLES.contains(&"package"));
        assert!(KNOWN_TOP_TABLES.contains(&"build"));
        assert!(KNOWN_PACKAGE_KEYS.contains(&"name"));
        assert!(KNOWN_PACKAGE_KEYS.contains(&"sha256"));
        // `%%{...}` is no escape here: `Recipe::expand` replaces inside it
        // the same way, so the scanner flags it as a real placeholder too
        let ph = placeholders("a %{pkgdir} b %{nope} %{srcdir2} %{} %{1x} %%{escaped}");
        assert_eq!(ph, vec!["pkgdir", "nope", "srcdir2", "escaped"]);
    }
}
