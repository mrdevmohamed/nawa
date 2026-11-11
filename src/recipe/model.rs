//! Recipe parsing: every package in the distribution is described by a
//! declarative `recipe.toml` file with shell build sections.

use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Recipe {
    pub package: Package,
    /// Recipe-local assets (fetch-phase.md §5). A top-level table, like
    /// `[sandbox]`; any use makes the recipe a new-schema recipe whose
    /// `package.source` must carry the schema marker (§19), like
    /// `[[package.sources]]`.
    #[serde(default)]
    pub assets: AssetSpec,
    /// Recipe-local ecosystem fetch declarations (fetch-phase.md §7–§9).
    /// A top-level table (`[fetch.go]` / `[fetch.cargo]`); any use makes the
    /// recipe a new-schema recipe whose `package.source` must carry the
    /// schema marker (§19).
    #[serde(default)]
    pub fetch: Fetch,
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
pub(crate) const SCHEMA_MARKER: &str = "local:/nawa-schema-2-requires-a-newer-nawa";

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Package {
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
pub(crate) struct SourceSpec {
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

/// Recipe-local assets (fetch-phase.md §5.2): files that live beside the
/// recipe — patches, config fragments, scripts — snapshotted into
/// `<work>/assets` and exposed read-only as `%{assetsdir}` (§5.3). They are
/// declared, versioned and published *with* the recipe tree (the signed
/// tarball already covers them), so they need no checksum in the recipe.
/// `deny_unknown_fields`: a misspelled key is a parse-time error (§9).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AssetSpec {
    #[serde(default)]
    pub include: Vec<String>,
}

/// Ecosystem fetch declarations (fetch-phase.md §7–§9): `[fetch.go]` and/or
/// `[fetch.cargo]` declare the directory whose `go.mod`/`Cargo.toml` Nawa
/// runs the ecosystem fetch for. A top-level table like `[sandbox]`; any use
/// makes the recipe a new-schema recipe whose `package.source` must carry
/// the schema marker (§19). `deny_unknown_fields`: `[fetch.cago]` is a
/// parse-time error (§9).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Fetch {
    #[serde(default)]
    pub go: Option<FetchSpec>,
    #[serde(default)]
    pub cargo: Option<FetchSpec>,
}

impl Fetch {
    /// Whether the recipe declares any fetch backend.
    pub fn any(&self) -> bool {
        self.go.is_some() || self.cargo.is_some()
    }
}

/// One backend declaration (`[fetch.go]` / `[fetch.cargo]`, §9).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FetchSpec {
    /// The workspace-relative directory holding `go.mod`/`go.sum` (or
    /// `Cargo.toml`/`Cargo.lock`). Defaults to the workspace root (`.`).
    /// Never absolute, never borrowing `..` — parsed like `into`/`workdir`.
    #[serde(default = "default_fetch_dir")]
    pub dir: String,
}

fn default_fetch_dir() -> String {
    ".".to_string()
}

/// Implicit package providing the Go toolchain, added by `[fetch.go]`
/// (fetch-phase.md §9). The toolchain names live here in **one** place and
/// are kept here in one place and must match the package names published
/// by faseela-repo. Go is `golang`; Rust is `rust`.
pub(crate) const GO_TOOLCHAIN: &str = "golang";
/// Implicit package providing the Rust toolchain, added by `[fetch.cargo]`.
pub(crate) const CARGO_TOOLCHAIN: &str = "rust";
/// The closure must provide a CA bundle for the fetch toolchain's TLS (§9);
/// the name must be verified against faseela-repo before release.
pub(crate) const CA_BUNDLE: &str = "ca-certificates";

fn default_rel() -> u32 {
    1
}

#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct Depends {
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
pub(crate) struct Build {
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
pub(crate) struct Sandbox {
    /// A recipe that truly needs network during a build section. Requires
    /// `nawa build --allow-network` as well; the archive records `impure = network`.
    #[serde(default)]
    pub network: bool,
    /// Mount `/sys` read-only inside the sandbox (default: hidden).
    #[serde(default)]
    pub sysfs: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct Hooks {
    #[serde(default)]
    pub post_install: String,
    #[serde(default)]
    pub pre_remove: String,
    #[serde(default)]
    pub post_remove: String,
    /// Run once after a package-changing Nawa transaction while this package
    /// remains installed. This is intended for idempotent system cache
    /// rebuilds such as the desktop MIME application database.
    #[serde(default)]
    pub post_transaction: String,
}

/// Context used to expand `%{...}` placeholders in build scripts.
pub(crate) struct ExpandCtx {
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
    /// as above: recipes without `[assets]` never see the name.
    pub assetsdir: Option<PathBuf>,
}

