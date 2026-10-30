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
    /// The raw recipe.toml bytes, kept for the deterministic build id
    /// (§7 of docs/design/build-isolation.md). Not part of the TOML.
    #[serde(skip)]
    pub raw: String,
}

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
        Ok(recipe)
    }

    fn validate(&self) -> Result<()> {
        if self.package.name.is_empty() {
            bail!("recipe has empty package.name");
        }
        if self.package.version.is_empty() {
            bail!("recipe {} has empty package.version", self.package.name);
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

    /// Expand placeholders inside a build/hook script.
    pub fn expand(&self, script: &str, ctx: &ExpandCtx) -> String {
        script
            .replace("%{name}", &self.package.name)
            .replace("%{version}", &self.package.version)
            .replace("%{rel}", &self.package.rel.to_string())
            .replace("%{arch}", &ctx.arch)
            .replace("%{jobs}", &ctx.jobs.to_string())
            .replace("%{pkgdir}", &ctx.pkgdir.to_string_lossy())
            .replace("%{srcdir}", &ctx.srcdir.to_string_lossy())
    }
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
        };
        let out = rec.expand("make -j%{jobs} DESTDIR=%{pkgdir}", &ctx);
        assert_eq!(out, "make -j8 DESTDIR=/tmp/pkg");
    }
}
