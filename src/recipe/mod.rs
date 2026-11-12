//! Recipe parsing (split from monolithic recipe.rs).

pub mod model;
pub mod validate;
pub mod expand;
pub mod load;

pub(crate) use model::{Recipe, SourceSpec, FetchSpec, ExpandCtx};
#[cfg(test)]
pub(crate) use model::{CARGO_TOOLCHAIN, CA_BUNDLE, GO_TOOLCHAIN, SCHEMA_MARKER};
pub(crate) use validate::is_lower_hex64;
pub(crate) use load::{load_all, load_by_name, locate};

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

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

    /// A minimal new-schema recipe with the given extra table text. The
    /// sha256 is exactly 64 lowercase hex characters (parse-time checked).
    fn new_schema_with_fetch(table: &str, name: &str) -> Recipe {
        toml::from_str(&format!(
            r#"
            [package]
            name = "{name}"
            version = "1.0"
            rel = 1
            arch = ["x86_64"]
            summary = "t"
            license = ["MIT"]
            source = "{marker}"
            source_date_epoch = 1

            [[package.sources]]
            name = "src"
            url = "https://example/src.tar"
            sha256 = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
            {table}
            "#,
            marker = SCHEMA_MARKER,
        ))
        .unwrap()
    }

    #[test]
    fn fetch_table_is_new_schema_and_implies_toolchain_deps() {
        let rec = new_schema_with_fetch("[fetch.go]\ndir = \"cmd/server\"", "hello");
        assert!(rec.is_new_schema());
        assert_eq!(rec.fetch.go.as_ref().unwrap().dir, "cmd/server");
        let deps = rec.build_deps();
        assert!(
            deps.iter().any(|d| d == GO_TOOLCHAIN),
            "[fetch.go] implies the Go toolchain (§9)"
        );
        assert!(deps.iter().any(|d| d == CA_BUNDLE));
        // both tables in one recipe (§9)
        let both = new_schema_with_fetch("[fetch.go]\n\n[fetch.cargo]\ndir = \"lib\"", "both");
        let deps = both.build_deps();
        assert!(deps.iter().any(|d| d == CARGO_TOOLCHAIN));
        assert!(deps.iter().any(|d| d == GO_TOOLCHAIN));
        // implicit deps dedup against the declared list
        let rec = new_schema_with_fetch(
            &format!("[fetch.go]\n\n[depends]\nbuild = [\"{GO_TOOLCHAIN}\"]\n"),
            "dedup",
        );
        assert_eq!(
            rec.build_deps().iter().filter(|d| **d == GO_TOOLCHAIN).count(),
            1,
            "a declared toolchain dep must not be duplicated"
        );
    }

    #[test]
    fn fetch_dir_is_parsed_like_into_and_workdir() {
        for bad in ["../x", "/abs", "trailing/"] {
            let rec = new_schema_with_fetch(&format!("[fetch.go]\ndir = \"{bad}\""), "dir");
            // to_string() shows only the "recipe X" context; the debug form
            // carries the caused-by chain with the real message
            let err = format!("{:?}", rec.validate().unwrap_err());
            assert!(
                err.contains("fetch"),
                "`{bad}` must be rejected as a fetch dir, got: {err}"
            );
        }
    }

    #[test]
    fn fetch_unknown_keys_and_tables_are_parse_errors() {
        let err = toml::from_str::<Recipe>(
            r#"
            [package]
            name = "x"
            version = "1"
            rel = 1
            source = "https://e/x.tar"
            sha256 = "1"
            [fetch.cago]
            dir = "."
        "#,
        )
        .expect_err("[fetch.cago] must be a parse-time error (§9)");
        assert!(err.to_string().contains("cago"), "got: {err}");
        let err = toml::from_str::<Recipe>(
            r#"
            [package]
            name = "x"
            version = "1"
            rel = 1
            source = "https://e/x.tar"
            sha256 = "1"
            [fetch.go]
            dr = "."
        "#,
        )
        .expect_err("unknown key under [fetch.go] must be a parse error");
        assert!(err.to_string().contains("dr"), "got: {err}");
    }

    #[test]
    fn toolchain_recipes_must_not_declare_their_own_table() {
        let rec = new_schema_with_fetch("[fetch.cargo]\ndir = \".\"", CARGO_TOOLCHAIN);
        let err = rec.validate().unwrap_err().to_string();
        assert!(
            err.contains("self-dependency") || err.contains("itself"),
            "the Rust toolchain must not declare [fetch.cargo], got: {err}"
        );
        let rec = new_schema_with_fetch("[fetch.go]\ndir = \".\"", GO_TOOLCHAIN);
        assert!(rec.validate().is_err(), "[fetch.go] in the Go toolchain recipe");
    }

    #[test]
    fn fetch_defaults_dir_to_the_workspace_root() {
        let rec = new_schema_with_fetch("[fetch.go]\n[fetch.cargo]\ndir = \"lib\"", "def");
        let go = rec.fetch.go.as_ref().unwrap();
        assert_eq!(go.dir, ".");
        let cargo = rec.fetch.cargo.as_ref().unwrap();
        assert_eq!(cargo.dir, "lib");
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

    // ---- [assets] (fetch-phase.md §5.2) -----------------------------------

    /// The `[assets]` table parses and denies misspelled keys (§9/§14: a
    /// typo'd `include` must not silently stage nothing).
    #[test]
    fn assets_parse_and_deny_unknown_fields() {
        let with = NEW_SCHEMA
            .replace("    \"#;", "")
            .replace("</沉默>", "")
            + r#"
        [assets]
        include = ["patches/fix-build.patch", "config/x86_64.fragment"]
    "#;
        let rec: Recipe = toml::from_str(&with).unwrap();
        assert_eq!(rec.assets.include.len(), 2);
        assert_eq!(rec.assets.include[1], "config/x86_64.fragment");
        // deny_unknown_fields: a typo'd key is a parse error
        assert!(toml::from_str::<Recipe>(&with.replace("include =", "incude =")).is_err());
        // ...and so is a scalar `assets` (string instead of table)
        assert!(toml::from_str::<Recipe>(&with.replace(
            "[assets]\n        include = [\"patches/fix-build.patch\", \"config/x86_64.fragment\"]",
            "assets = \"patches\""
        )).is_err());
    }

    /// An `[assets]` table alone is a new-schema table: the marker is
    /// required (§19), and a markered recipe without any new table is
    /// rejected — including assets-only, where the table is the whole point.
    #[test]
    fn assets_only_recipe_needs_marker_and_is_new_schema() {
        let assets_only = r#"
        [package]
        name = "assets-only"
        version = "1.0.0"
        rel = 1
        source = "local:/nawa-schema-2-requires-a-newer-nawa"
        source_date_epoch = 1700000000

        [assets]
        include = ["kernel.config"]
    "#;
        let rec: Recipe = toml::from_str(assets_only).unwrap();
        assert!(rec.is_new_schema());
        rec.validate().unwrap();

        // same recipe without the marker is refused
        let unmarked = assets_only.replace(SCHEMA_MARKER, "local:/nowhere");
        let rec: Recipe = toml::from_str(&unmarked).unwrap();
        assert!(rec.is_new_schema());
        let err = rec.validate().unwrap_err().to_string();
        assert!(err.contains("schema marker"), "got: {err}");

        // ...and an assets-only recipe with the marker but no epoch is
        // refused too (§6: assets never feed the epoch)
        let no_epoch = assets_only.replace("source_date_epoch = 1700000000\n", "");
        let rec: Recipe = toml::from_str(&no_epoch).unwrap();
        assert!(rec.package.source_date_epoch.is_none());
        let err = rec.validate().unwrap_err().to_string();
        assert!(
            err.contains("derive SOURCE_DATE_EPOCH"),
            "assets-only without epoch: {err}"
        );
        // percent-expansion of %{srcroot} is refused for it too (no sources)
        let mut rec: Recipe = toml::from_str(&no_epoch).unwrap();
        rec.raw = no_epoch.to_string();
        rec.raw.push_str("\nconfigure = \"ls %{srcroot}\"");
        let err = rec.check_schema_placeholders().unwrap_err().to_string();
        assert!(err.contains("%{srcroot}"), "got: {err}");
    }

    /// `[assets]` include entries must be relative, non-`..`, non-trailing-slash
    /// paths (§5.2, same rules as `into`/`workdir`).
    #[test]
    fn assets_include_paths_are_validated() {
        for bad in ["", "  ", "/abs/config", "../escape", "kernel.config/"] {
            let bad_line = format!("include = [\"{bad}\"]");
            let with = NEW_SCHEMA.replace("    \"#;", "") + format!(
                "\n        [assets]\n        {bad_line}\n    "
            ).as_str();
            let rec: Recipe = toml::from_str(&with).unwrap();
            let err = format!("{:#}", rec.validate().unwrap_err());
            assert!(
                err.contains("[assets] include"),
                "{bad:?}: {err}"
            );
        }
    }

    /// §14C per-name: a recipe that declares `[[package.sources]]` but no
    /// `[assets]` must be refused when its bytes reference `%{assetsdir}`
    /// or `NAWA_ASSETSDIR` — and the reverse combination too.
    #[test]
    fn assets_placeholders_are_enabled_per_name() {
        // sources but no assets: %{assetsdir} is refused
        let mut rec: Recipe = toml::from_str(NEW_SCHEMA).unwrap();
        rec.raw = NEW_SCHEMA.to_string();
        rec.raw.push_str("\nconfigure = \"cp %{assetsdir}/conf .\"");
        let err = rec.check_schema_placeholders().unwrap_err().to_string();
        assert!(err.contains("%{assetsdir}"), "got: {err}");
        // the env name, same refusal
        rec.raw = "install = \"ls $NAWA_ASSETSDIR\"".to_string();
        let rec: Recipe = toml::from_str(NEW_SCHEMA).unwrap();
        let mut rec = rec;
        rec.raw = format!("{}install = \"ls $NAWA_ASSETSDIR\"", "\n");
        let err = rec.check_schema_placeholders().unwrap_err().to_string();
        assert!(err.contains("NAWA_ASSETSDIR"), "got: {err}");
        // assets but no sources: %{srcroot} is refused (see above) and the
        // combined recipe passes
        let both = NEW_SCHEMA.replace("    \"#;", "") + r#"
        [assets]
        include = ["config/x86_64.fragment"]
    "#;
        let mut rec: Recipe = toml::from_str(&both).unwrap();
        rec.raw = both.to_string();
        rec.check_schema_placeholders().unwrap();
    }

    #[test]
    fn fetch_toolchain_names_match_faseela_recipe_ids() {
        // Verified against the current recipe paths in mrdevmohamed/faseela-repo.
        assert_eq!(GO_TOOLCHAIN, "golang");
        assert_eq!(CARGO_TOOLCHAIN, "rust");
        assert_eq!(CA_BUNDLE, "ca-certificates");
    }

    #[test]
    fn fetch_backends_add_toolchain_and_ca_bundle_to_build_dependencies() {
        let raw = r#"
[package]
name = "fetch-deps-test"
version = "1.0"
rel = 1
arch = ["x86_64"]
source = "local:/nawa-schema-2-requires-a-newer-nawa"

[fetch.go]
dir = "go"

[fetch.cargo]
dir = "rust"
"#;
        let rec: Recipe = toml::from_str(raw).unwrap();
        let deps = rec.build_deps();
        assert!(deps.contains(&"golang".to_string()), "{deps:?}");
        assert!(deps.contains(&"rust".to_string()), "{deps:?}");
        assert!(deps.contains(&"ca-certificates".to_string()), "{deps:?}");
    }
}
