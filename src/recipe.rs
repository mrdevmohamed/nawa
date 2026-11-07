//! Recipe parsing: every package in the distribution is described by a
//! declarative `recipe.toml` file with shell build sections.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Deserialize)]
pub struct Recipe {
    pub package: Package,
    #[serde(default)]
    pub depends: Depends,
    #[serde(default)]
    pub build: Build,
    #[serde(default)]
    pub hooks: Hooks,
    #[serde(default)]
    pub sandbox: Sandbox,
    /// Per-recipe opt-outs of the post-install slimming passes
    /// (`[slim] strip = false`, ...). See `slim.rs`.
    #[serde(default)]
    pub slim: crate::slim::Slim,
    /// The raw recipe.toml bytes, kept for the deterministic build id
    /// (§7 of docs/design/build-isolation.md). Not part of the TOML.
    #[serde(skip)]
    pub raw: String,
    /// The directory this recipe.toml lives in: the base for recipe-local
    /// assets (fetch-phase.md §5.2). Not part of the TOML; empty when the
    /// recipe was parsed straight from TOML rather than loaded from disk.
    #[serde(skip)]
    pub dir: PathBuf,
}

/// The fail-closed schema marker (fetch-phase.md §19): a recipe that uses
/// any new-schema table must set
/// `source = "local:/nawa-schema-2-requires-a-newer-nawa"`, so an *older*
/// nawa binary stops with a message that names the problem — its legacy
/// staging treats `local:<path>` as a host path and fails when it does not
/// exist, before any download or closure work. The new nawa treats exactly
/// this value as the marker: required with the new tables, rejected
/// without them, and never staged.
pub const SCHEMA_MARKER: &str = "local:/nawa-schema-2-requires-a-newer-nawa";

#[derive(Debug, Clone, Deserialize)]
pub struct Package {
    pub name: String,
    pub version: String,
    #[serde(default = "default_rel")]
    pub rel: u32,
    #[serde(default)]
    pub arch: Vec<String>,
    #[allow(dead_code)] // recipe metadata, kept for repo tooling
    #[serde(default)]
    pub summary: String,
    #[allow(dead_code)] // recipe metadata, kept for repo tooling
    #[serde(default)]
    pub license: Vec<String>,
    /// URL of the source tarball, or `local:<path>` to copy from the host,
    /// or empty for meta packages that only run build commands.
    #[serde(default)]
    pub source: String,
    /// sha256 of the source tarball (required unless `local:` / empty).
    #[serde(default)]
    pub sha256: String,
    /// Extra download URLs for the same tarball, tried in order when
    /// `source` (and its built-in GNU mirrors) fails. `%{version}` & co. are
    /// expanded. They need no trust: the bytes must still match `sha256`.
    #[serde(default)]
    pub mirrors: Vec<String>,
    /// Directory name inside the extracted tarball. Defaults to
    /// `{name}-{version}`. Ignored for `local:` sources.
    #[serde(default)]
    pub source_dir: String,
    /// Pin `SOURCE_DATE_EPOCH` instead of deriving it from the newest
    /// source mtime (docs/design/build-isolation.md §6). Optional.
    #[serde(default)]
    pub source_date_epoch: Option<u64>,
    /// New-schema external sources (fetch-phase.md §4.2). Empty = legacy
    /// recipe: staging keeps using `source` / `sha256` / `mirrors` /
    /// `source_dir` exactly as today. A non-empty array refuses to mix with
    /// the legacy fields and requires `package.source = SCHEMA_MARKER` (§19).
    #[serde(default)]
    pub sources: Vec<SourceSpec>,
    /// Working directory of every build section, relative to the source
    /// workspace root (fetch-phase.md §4.3). Empty = the workspace root.
    /// Valid only together with `[[package.sources]]`.
    #[serde(default)]
    pub workdir: String,
}

/// One external source artifact of a new-schema recipe (§4.2). The struct
/// uses `deny_unknown_fields`: unknown keys are a parse-time error, part of
/// the forward-compatibility rules of §19.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceSpec {
    /// Unique within the recipe; `[a-z0-9][a-z0-9._-]*`, at most 64
    /// characters. Names appear in paths (stage trees, build-id key, errors).
    pub name: String,
    /// `https://` URL; `%{version}`, `%{name}`, `%{rel}`, `%{arch}` are
    /// expanded as in legacy recipes.
    pub url: String,
    /// Exactly 64 lowercase hex characters — validated at parse time
    /// because the digest doubles as a cache path component (`<srccache>/
    /// sha256/<digest>`), so it must never carry `/` or `..` (§4.2, §11).
    pub sha256: String,
    /// Fallback URLs for this artifact only, tried after `url` and the
    /// built-in GNU mirrors. They need no trust: the bytes must still match
    /// `sha256`.
    #[serde(default)]
    pub mirrors: Vec<String>,
    /// `true` = must be an archive (error otherwise); `false` = copy the
    /// file as-is into `into`; omitted = detect by content, like today. A
    /// detected plain file is staged like `unpack = false`, so it is stamped
    /// with `SOURCE_DATE_EPOCH` and excluded from its derivation (§6).
    #[serde(default)]
    pub unpack: Option<bool>,
    /// Workspace-relative destination: the extraction directory for
    /// archives (default `.`), the destination file path for plain files
    /// (default: the URL's file name in the workspace root).
    #[serde(default)]
    pub into: String,
    /// Leading archive path components to remove (default 0). Applied
    /// **before** path sanitisation and also to hard-link targets; entries
    /// with fewer components are skipped (GNU tar semantics, §4.2).
    #[serde(default)]
    pub strip_components: u32,
}

fn default_rel() -> u32 {
    1
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Depends {
    /// Build dependencies. At the `closure` isolation level this means
    /// "libraries/headers/tools beyond `base-devel`" — see
    /// docs/design/build-isolation.md §5.1; at lower levels everything
    /// installed on the build host is visible either way.
    #[serde(default)]
    pub build: Vec<String>,
    #[serde(default)]
    pub run: Vec<String>,
    /// Tools needed only by the recipe's `check` section (e.g. dejagnu).
    /// Pulled into the build root only when building with `--check`.
    #[serde(default)]
    pub check: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Build {
    #[serde(default)]
    pub prepare: String,
    #[serde(default)]
    pub configure: String,
    #[serde(default)]
    pub compile: String,
    #[serde(default)]
    pub check: String,
    #[serde(default)]
    pub install: String,
}

/// Per-recipe sandbox overrides (docs/design/build-isolation.md §8).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Sandbox {
    /// A recipe that truly needs network during a build section. Requires
    /// `nawa build --allow-network` as well; the archive records `impure = network`.
    #[serde(default)]
    pub network: bool,
    /// Mount `/sys` read-only inside the sandbox (default: hidden).
    #[serde(default)]
    pub sysfs: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Hooks {
    #[serde(default)]
    pub post_install: String,
    #[serde(default)]
    pub pre_remove: String,
    #[serde(default)]
    pub post_remove: String,
}

/// Context used to expand `%{...}` placeholders in build scripts.
pub struct ExpandCtx {
    pub jobs: u32,
    pub pkgdir: PathBuf,
    pub srcdir: PathBuf,
    pub arch: String,
    /// `%{srcroot}` / `NAWA_SRCROOT` — the source workspace root, the parent
    /// of the working directory, where additional `[[package.sources]]`
    /// land (fetch-phase.md §4.3). Expanded by `Recipe::expand` for
    /// new-schema recipes only; `None` keeps legacy recipes' environment
    /// byte-for-byte unchanged.
    pub srcroot: Option<PathBuf>,
    /// `%{assetsdir}` / `NAWA_ASSETSDIR` — the read-only snapshot of the
    /// recipe-declared `[assets]` files (fetch-phase.md §5.3). Same rule
    /// as above: legacy recipes never see the name.
    #[allow(dead_code)] // consumed when %{assetsdir} expansion lands (Phase 2)
    pub assetsdir: Option<PathBuf>,
}

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

    fn validate(&self) -> Result<()> {
        if self.package.name.is_empty() {
            bail!("recipe has empty package.name");
        }
        if self.package.version.is_empty() {
            bail!("recipe {} has empty package.version", self.package.name);
        }
        // ---- new schema vs legacy (§4.1/§4.2, §19) — checked first, so
        // a mixed recipe reports the real problem instead of a legacy one
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
                "recipe {} uses [[package.sources]] but package.source does not carry the \
                 fail-closed schema marker (fetch-phase.md §19):\n  {}",
                self.package.name,
                SCHEMA_MARKER
            );
        }
        if marker && !new_schema {
            bail!(
                "recipe {} sets the schema marker in package.source without using any \
                 new-schema table (fetch-phase.md §19) — either add [[package.sources]] \
                 or drop the marker",
                self.package.name
            );
        }
        if new_schema {
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
            if self
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
        }
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
    /// Phase 1 means `[[package.sources]]` (the `[package] workdir` key
    /// requires them; `[assets]` and `[fetch.*]` come later). On a
    /// new-schema recipe `package.source` carries [`SCHEMA_MARKER`] and the
    /// staging engine is `sources.rs`, not the legacy path in `build()`.
    pub fn is_new_schema(&self) -> bool {
        !self.package.sources.is_empty()
    }

    /// §14C: `%{srcroot}` / `NAWA_SRCROOT` used by a recipe that does not
    /// enable the new schema is a hard stage-time error, not a silent
    /// expansion — legacy recipes keep their environment exactly as today,
    /// so the names never enter it. Scanned over the raw recipe bytes, so
    /// build sections *and* hooks are covered.
    pub fn check_schema_placeholders(&self) -> Result<()> {
        if self.is_new_schema() {
            return Ok(());
        }
        for needle in ["%{srcroot}", "NAWA_SRCROOT"] {
            if self.raw.contains(needle) {
                bail!(
                    "recipe {} uses `{needle}` but does not declare [[package.sources]] — \
                     the workspace-root placeholder only exists for new-schema recipes \
                     (fetch-phase.md §4.3); enable the schema or drop the reference",
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
        // implicit fetch toolchains are appended here when the new schema
        // lands; until then this is exactly the declared list
        self.depends.build.clone()
    }

    /// Expand placeholders inside a build/hook script.
    ///
    /// `%{srcroot}` expands only on new-schema recipes (§4.3): for legacy
    /// recipes the placeholder is left alone — `build()` already refuses any
    /// raw recipe that references it (§14C) — so their environment and
    /// scripts stay byte-for-byte unchanged.
    pub fn expand(&self, script: &str, ctx: &ExpandCtx) -> String {
        let out = script
            .replace("%{name}", &self.package.name)
            .replace("%{version}", &self.package.version)
            .replace("%{rel}", &self.package.rel.to_string())
            .replace("%{arch}", &ctx.arch)
            .replace("%{jobs}", &ctx.jobs.to_string())
            .replace("%{pkgdir}", &ctx.pkgdir.to_string_lossy())
            .replace("%{srcdir}", &ctx.srcdir.to_string_lossy());
        match &ctx.srcroot {
            Some(root) => out.replace("%{srcroot}", &root.to_string_lossy()),
            None => out,
        }
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
pub fn is_lower_hex64(s: &str) -> bool {
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

/// Discover every recipe under a recipes root (`recipes/**/recipe.toml`).
pub fn load_all(root: &Path) -> Result<BTreeMap<String, Recipe>> {
    let mut map = BTreeMap::new();
    for entry in walkdir::WalkDir::new(root)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name() == "recipe.toml")
    {
        let rec = Recipe::load(entry.path())?;
        let name = rec.package.name.clone();
        if map.insert(name.clone(), rec).is_some() {
            // name the package that duplicated, not whatever happens to be
            // the alphabetically-last key of the map so far
            bail!("duplicate recipe name under {}: `{}`", root.display(), name);
        }
    }
    Ok(map)
}

/// Load one recipe by package name, searching `recipes/**/recipe.toml`.
/// A miss suggests close recipe names — "did you mean `gzip`?" beats a bare
/// "not found" every time.
pub fn load_by_name(root: &Path, name: &str) -> Result<Recipe> {
    Ok(locate(root, name)?.0)
}

/// Load one recipe by name and also report where its `recipe.toml` lives
/// (single walk — `load_by_name` and `find_recipe_path` share this).
pub fn locate(root: &Path, name: &str) -> Result<(Recipe, PathBuf)> {
    for entry in walkdir::WalkDir::new(root)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name() == "recipe.toml")
    {
        let rec = Recipe::load(entry.path())?;
        if rec.package.name == name {
            return Ok((rec, entry.path().to_path_buf()));
        }
    }
    // miss: offer the closest names we know about (best effort — a tree that
    // fails to parse still reports the plain "not found" below)
    let names: Vec<String> = load_all(root)
        .map(|m| m.into_keys().collect())
        .unwrap_or_default();
    bail!(
        "recipe not found: `{}` (searched {}){}",
        name,
        root.display(),
        crate::suggest::hint(name, &names)
    )
}

/// Where a package's `recipe.toml` lives, for `nawa info`.
pub fn find_recipe_path(root: &Path, name: &str) -> Result<PathBuf> {
    Ok(locate(root, name)?.1)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
        [package]
        name = "gzip"
        version = "1.13"
        rel = 2
        arch = ["x86_64"]
        summary = "GNU compression utility"
        license = ["GPL-3.0"]
        source = "https://ftp.gnu.org/gnu/gzip/gzip-%{version}.tar.xz"
        sha256 = "abc"

        [depends]
        build = []
        run = []

        [build]
        configure = "./configure --prefix=/usr"
        compile = "make -j%{jobs}"
        install = "make DESTDIR=%{pkgdir} install"

        [sandbox]
        network = false
    "#;

    #[test]
    fn mirrors_default_to_empty_and_parse() {
        let rec: Recipe = toml::from_str(SAMPLE).unwrap();
        assert!(rec.package.mirrors.is_empty());
        let with = SAMPLE.replace(
            "sha256 = \"abc\"",
            "sha256 = \"abc\"\n        mirrors = [\"https://m1.example/gzip-%{version}.tar.xz\"]",
        );
        let rec: Recipe = toml::from_str(&with).unwrap();
        rec.validate().unwrap();
        assert_eq!(rec.package.mirrors, vec!["https://m1.example/gzip-%{version}.tar.xz"]);
    }

    #[test]
    fn mirrors_need_a_downloadable_source() {
        let bad = SAMPLE
            .replace(
                "source = \"https://ftp.gnu.org/gnu/gzip/gzip-%{version}.tar.xz\"",
                "source = \"local:/src/gzip\"\n        mirrors = [\"https://m1.example/x.tar.xz\"]",
            );
        let rec: Recipe = toml::from_str(&bad).unwrap();
        let err = rec.validate().unwrap_err().to_string();
        assert!(err.contains("package.mirrors"), "got: {err}");
    }

    #[test]
    fn load_records_the_recipe_directory() {
        // Recipe::load remembers where recipe.toml lives (the base for
        // recipe-local assets, fetch-phase.md §5.2); byte-parsing does not
        let dir = std::env::temp_dir().join(format!("nawa-recipe-dir-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("recipe.toml");
        std::fs::write(&path, SAMPLE).unwrap();
        let rec = Recipe::load(&path).unwrap();
        assert_eq!(rec.dir, dir);
        assert!(!rec.raw.is_empty(), "raw bytes remembered too");
        let parsed: Recipe = toml::from_str(SAMPLE).unwrap();
        assert!(parsed.dir.as_os_str().is_empty());
        assert!(parsed.raw.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn build_deps_is_the_declared_list_for_legacy_recipes() {
        let rec: Recipe = toml::from_str(SAMPLE).unwrap();
        assert!(
            rec.build_deps().is_empty(),
            "legacy recipes must not gain implicit deps yet"
        );
        let with = SAMPLE.replace("build = []", "build = [\"zlib\"]");
        let rec: Recipe = toml::from_str(&with).unwrap();
        assert_eq!(rec.build_deps(), vec!["zlib".to_string()]);
    }

    #[test]
    fn parses_sample_recipe() {
        let rec: Recipe = toml::from_str(SAMPLE).unwrap();
        assert_eq!(rec.package.name, "gzip");
        assert_eq!(rec.package.rel, 2);
        assert_eq!(rec.id(), "gzip-1.13-2");
        assert_eq!(rec.arch(), "x86_64");
        assert_eq!(rec.depends.check, Vec::<String>::new());
        assert!(!rec.sandbox.network);
        assert_eq!(rec.package.source_date_epoch, None);
    }

    #[test]
    fn parses_isolation_surface() {
        let rec: Recipe = toml::from_str(
            r#"
            [package]
            name = "probe"
            version = "1"
            source_date_epoch = 1700000000

            [depends]
            build = ["zlib"]
            check = ["dejagnu"]

            [sandbox]
            network = true
            sysfs = true

            [build]
            install = "true"
            "#,
        )
        .unwrap();
        assert_eq!(rec.package.source_date_epoch, Some(1_700_000_000));
        assert_eq!(rec.depends.check, vec!["dejagnu".to_string()]);
        assert!(rec.sandbox.network);
        assert!(rec.sandbox.sysfs);
    }

    #[test]
    fn expands_placeholders() {
        let rec: Recipe = toml::from_str(SAMPLE).unwrap();
        let ctx = ExpandCtx {
            jobs: 8,
            pkgdir: PathBuf::from("/tmp/pkg"),
            srcdir: PathBuf::from("/tmp/src"),
            arch: "x86_64".into(),
            srcroot: None,
            assetsdir: None,
        };
        let out = rec.expand("make -j%{jobs} DESTDIR=%{pkgdir}", &ctx);
        assert_eq!(out, "make -j8 DESTDIR=/tmp/pkg");
        // %{srcroot} without the schema: left alone at expansion, refused at
        // build time (§14C). Recipe::load fills `raw`, which the check scans.
        let out = rec.expand("cd %{srcroot}/patches", &ctx);
        assert_eq!(out, "cd %{srcroot}/patches");
        let mut rec: Recipe = toml::from_str(SAMPLE).unwrap();
        rec.raw = SAMPLE.to_string();
        rec.raw.push_str("\ncompile = \"cd %{srcroot}/patches\"");
        let err = rec.check_schema_placeholders().unwrap_err().to_string();
        assert!(err.contains("%{srcroot}"), "got: {err}");
        // the environment name is caught the same way, in any field
        let mut rec: Recipe = toml::from_str(SAMPLE).unwrap();
        rec.raw = "install = \"ls $NAWA_SRCROOT\"".to_string();
        let err = rec.check_schema_placeholders().unwrap_err().to_string();
        assert!(err.contains("NAWA_SRCROOT"), "got: {err}");
        // ...and a new-schema recipe passes
        let mut rec: Recipe = toml::from_str(NEW_SCHEMA).unwrap();
        rec.raw = NEW_SCHEMA.to_string();
        rec.check_schema_placeholders().unwrap();
    }

    // ---- new-schema [[package.sources]] (fetch-phase.md §4.2) ---------------

    /// A valid new-schema recipe: marker + two archives differing in shape.
    const NEW_SCHEMA: &str = r#"
        [package]
        name = "example"
        version = "1.0.0"
        rel = 1
        source = "local:/nawa-schema-2-requires-a-newer-nawa"
        workdir = "kernel"

        [[package.sources]]
        name = "main"
        url = "https://example.org/example-%{version}.tar.xz"
        sha256 = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
        into = "kernel"
        strip_components = 1

        [[package.sources]]
        name = "bootstrap-data"
        url = "https://example.org/example-bootstrap.dat"
        sha256 = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210"
        unpack = false
        into = "data/bootstrap.dat"
    "#;

    fn new_schema_ctx() -> ExpandCtx {
        ExpandCtx {
            jobs: 4,
            pkgdir: PathBuf::from("/pkg"),
            srcdir: PathBuf::from("/src/kernel"),
            arch: "x86_64".into(),
            srcroot: Some(PathBuf::from("/src")),
            assetsdir: None,
        }
    }

    #[test]
    fn parses_sources_workdir_and_marker() {
        let rec: Recipe = toml::from_str(NEW_SCHEMA).unwrap();
        rec.validate().unwrap();
        assert!(rec.is_new_schema());
        assert_eq!(rec.package.sources.len(), 2);
        assert_eq!(rec.package.sources[0].name, "main");
        assert_eq!(rec.package.sources[0].strip_components, 1);
        assert_eq!(rec.package.sources[0].into, "kernel");
        assert_eq!(rec.package.sources[0].unpack, None);
        assert_eq!(rec.package.sources[1].unpack, Some(false));
        assert_eq!(rec.package.workdir, "kernel");
        assert!(rec.package.mirrors.is_empty());
        // health check: deny_unknown_fields rejects a typo'd key (§19)
        let typo = NEW_SCHEMA.replace("into = \"kernel\"", "intto = \"kernel\"");
        assert!(toml::from_str::<Recipe>(&typo).is_err());
    }

    #[test]
    fn expands_srcroot_for_new_schema_but_not_for_legacy() {
        let rec: Recipe = toml::from_str(NEW_SCHEMA).unwrap();
        assert_eq!(
            rec.expand("cd %{srcroot}/patches && cd %{srcdir}", &new_schema_ctx()),
            "cd /src/patches && cd /src/kernel"
        );
        let ctx = new_schema_ctx();
        let with = NEW_SCHEMA.replace("workdir = \"kernel\"", "workdir = \"\"");
        let rec: Recipe = toml::from_str(&with).unwrap();
        // default workdir = the workspace root: %{srcdir} == %{srcroot}
        let ctx = ExpandCtx {
            srcdir: ctx.srcroot.clone().unwrap(),
            ..new_schema_ctx()
        };
        let out = rec.expand("p=%{srcdir} r=%{srcroot}", &ctx);
        assert_eq!(out, "p=/src r=/src");
        let legacy: Recipe = toml::from_str(SAMPLE).unwrap();
        let legacy_ctx = ExpandCtx {
            jobs: 8,
            pkgdir: PathBuf::from("/tmp/pkg"),
            srcdir: PathBuf::from("/tmp/src"),
            arch: "x86_64".into(),
            srcroot: None,
            assetsdir: None,
        };
        let out = legacy.expand("cd %{srcdir}", &legacy_ctx);
        assert_eq!(out, "cd /tmp/src");
        assert!(legacy_ctx.srcroot.is_none() && legacy_ctx.assetsdir.is_none());
    }

    #[test]
    fn marker_is_required_with_new_tables() {
        let missing = NEW_SCHEMA.replace(
            "source = \"local:/nawa-schema-2-requires-a-newer-nawa\"\n        ",
            "",
        );
        let rec: Recipe = toml::from_str(&missing).unwrap();
        let err = rec.validate().unwrap_err().to_string();
        assert!(err.contains("marker"), "got: {err}");
    }

    #[test]
    fn marker_is_rejected_without_new_tables() {
        // the marker value on an otherwise-legacy recipe is refused: it can
        // only appear together with the new tables (§19)
        let bad = SAMPLE.replace(
            "source = \"https://ftp.gnu.org/gnu/gzip/gzip-%{version}.tar.xz\"",
            "source = \"local:/nawa-schema-2-requires-a-newer-nawa\"",
        );
        let rec: Recipe = toml::from_str(&bad).unwrap();
        let err = rec.validate().unwrap_err().to_string();
        assert!(err.contains("new-schema table"), "got: {err}");
    }

    #[test]
    fn legacy_mirrors_and_source_dir_may_not_mix_with_sources() {
        for (label, patch) in [
            (
                "sha256",
                "sha256 = \"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\"",
            ),
            ("source_dir", "source_dir = \"kernel\""),
            (
                "mirrors",
                "mirrors = [\"https://example.org/mirror.tar.xz\"]",
            ),
        ] {
            let mixed = NEW_SCHEMA.replace(
                "workdir = \"kernel\"",
                format!("workdir = \"kernel\"\n        {patch}").as_str(),
            );
            let rec: Recipe = toml::from_str(&mixed).unwrap_or_else(|e| {
                panic!("{label}: {e}")
            });
            let err = rec.validate().unwrap_err().to_string();
            assert!(
                err.contains("mix") && err.contains(label),
                "{label}: got: {err}"
            );
        }
    }

    #[test]
    fn duplicate_and_bad_source_names_are_rejected() {
        let dup = format!(
            "{NEW_SCHEMA}\n\n        [[package.sources]]\n        name = \"main\"\n        \
             url = \"https://example.org/x\"\n        \
             sha256 = \"{}\"\n",
            "0".repeat(64)
        );
        let rec: Recipe = toml::from_str(&dup).unwrap();
        let err = rec.validate().unwrap_err().to_string();
        assert!(err.contains("duplicate"), "got: {err}");

        for bad in [
            "UPPER",    // case
            "",         // empty
            "a/b",      // path separator
            "-a",       // head not [a-z0-9]
            "_a",       // head not [a-z0-9]
            "a b",      // space
            "a-b-c!",   // non-name char
        ] {
            let bad_name = NEW_SCHEMA.replace("name = \"main\"", &format!("name = \"{bad}\""));
            let rec: Recipe = toml::from_str(&bad_name).unwrap();
            let err = rec.validate().unwrap_err().to_string();
            assert!(err.contains("invalid source name"), "{bad:?}: got: {err}");
        }
        // the 64-character cap
        let too_long = format!("a{}", "b".repeat(64));
        let bad_name = NEW_SCHEMA.replace("name = \"main\"", &format!("name = \"{too_long}\""));
        let rec: Recipe = toml::from_str(&bad_name).unwrap();
        let err = rec.validate().unwrap_err().to_string();
        assert!(err.contains("invalid source name"), "got: {err}");
        // valid names pass: ., _, - and digits are allowed after the first
        for good in ["a", "main", "bootstrap-2.data", "x_y.z-1"] {
            let ok_name = NEW_SCHEMA.replace("name = \"main\"", &format!("name = \"{good}\""));
            let rec: Recipe = toml::from_str(&ok_name).unwrap();
            rec.validate().unwrap_or_else(|e| panic!("`{good}` should be valid: {e}"));
        }
    }

    #[test]
    fn sha256_must_be_64_lowercase_hex() {
        for bad in [
            "ABCDEF",
            "0123456789abcde",
            "0g23456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        ] {
            let mixed = NEW_SCHEMA.replace(
                "sha256 = \"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\"",
                &format!("sha256 = \"{bad}\""),
            );
            let rec: Recipe = toml::from_str(&mixed).unwrap();
            let err = rec.validate().unwrap_err().to_string();
            assert!(err.contains("64 lowercase hex"), "{bad}: {err}");
        }
    }

    #[test]
    fn into_and_workdir_must_be_relative_and_safe() {
        for into in ["/abs", "../escape", "a/../b", "trailing/"] {
            let mixed = NEW_SCHEMA.replace(
                "into = \"kernel\"",
                &format!("into = \"{into}\""),
            );
            let rec: Recipe = toml::from_str(&mixed).unwrap();
            let err = rec.validate().unwrap_err();
            // the long-form error reports the failing field through the
            // context chain (`recipe X source `main`: ...`)
            assert!(format!("{err:?}").contains("into"), "{into}: {err:?}");
        }
        for workdir in ["/usr", "../x", "a/../b", "end/"] {
            let mixed = NEW_SCHEMA.replace(
                "workdir = \"kernel\"",
                &format!("workdir = \"{workdir}\""),
            );
            let rec: Recipe = toml::from_str(&mixed).unwrap();
            let err = rec.validate().unwrap_err().to_string();
            assert!({ let e = format!("{err:#}"); e.contains("workdir") }, "{workdir}: {err}");
        }
    }

    #[test]
    fn workdir_requires_sources() {
        let no_sources = SAMPLE.replace(
            "source = \"https://ftp.gnu.org/gnu/gzip/gzip-%{version}.tar.xz\"\n        sha256 = \"abc\"",
            "workdir = \"kernel\"",
        );
        let rec: Recipe = toml::from_str(&no_sources).unwrap();
        let err = rec.validate().unwrap_err().to_string();
        assert!(
            err.contains("workdir") && err.contains("[[package.sources]]"),
            "got: {err}"
        );
    }

    #[test]
    fn all_plain_sources_without_pinned_epoch_are_rejected() {
        // both sources become plain files: the first archive loses its
        // archive marker entirely
        let plain = NEW_SCHEMA
            .replace(
                "into = \"kernel\"\n        strip_components = 1",
                "unpack = false",
            )
            .replace(
                "unpack = false\n        into = \"data/bootstrap.dat\"",
                "unpack = false",
            );
        let rec: Recipe = toml::from_str(&plain).unwrap();
        assert_eq!(rec.package.sources.len(), 2);
        assert!(rec
            .package
            .sources
            .iter()
            .all(|s| s.unpack == Some(false)));
        let err = rec.validate().unwrap_err().to_string();
        assert!(err.contains("unpack = false"), "got: {err}");
        // ...and pinning it makes the recipe valid
        let pinned = plain
            .replace("rel = 1", "rel = 1\n        source_date_epoch = 1700000000");
        let rec: Recipe = toml::from_str(&pinned).unwrap();
        rec.validate().unwrap();
    }
}