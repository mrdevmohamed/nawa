//! Repository index creation and validation (split from monolithic repo.rs).

pub mod model;
pub mod validate;
pub mod create;
pub mod update;
pub mod pack;

pub(crate) use model::{
    Index, RepoEntry, ARCHIVE_LOCATION_PER_PACKAGE_URL, INDEX_SCHEMA_V2, INDEX_SCHEMA_V3,
    RECIPES_SUFFIX,
};
#[cfg(test)]
pub(crate) use model::{
    RepositoryMeta, ARCHIVE_AUTH_SIGNED_INDEX, ARCHIVE_LOCATION_BASE_URL, INDEX_VALIDITY_SECS,
};
pub(crate) use validate::load_index_bytes;
#[cfg(test)]
pub(crate) use validate::load_index;
pub(crate) use create::create_index_with_locations_and_groups;
#[cfg(test)]
pub(crate) use create::{
    create_index, create_index_with, create_index_with_locations, create_index_with_version,
};
pub(crate) use update::{update_index_files, UpdateIndexFilesInputs};
#[cfg(test)]
pub(crate) use update::{update_index, IndexDelta};
pub(crate) use pack::validate_rev;

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(test)]
    use std::path::{Path, PathBuf};
    use crate::archive;

    #[test]
    fn index_keeps_only_the_newest_version_per_package() {
        let base = std::env::temp_dir().join(format!("nawa-repo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let dir = base.join("repo");
        std::fs::create_dir_all(&dir).unwrap();

        for (ver, rel) in [("1.0", 1), ("2.0", 1), ("1.5", 9)] {
            let st = base.join(format!("st-{ver}-{rel}"));
            std::fs::create_dir_all(&st).unwrap();
            std::fs::write(st.join("f"), b"x").unwrap();
            let pkginfo = format!("pkgname = demo\npkgver = {ver}\npkgrel = {rel}\narch = x86_64\nsize = 1\n");
            archive::create(&st, &pkginfo, &dir.join(format!("demo-{ver}-{rel}.nawa.tar.zst"))).unwrap();
        }

        let out = base.join("index.toml");
        let idx = create_index(&dir, &out).unwrap();
        assert_eq!(idx.packages.len(), 1);
        assert_eq!(idx.packages[0].version, "2.0");
        assert_eq!(idx.packages[0].rel, 1);
        assert_eq!(idx.find("demo").unwrap().version, "2.0");
        let e = idx.find("demo").unwrap();
        assert!(e.size > 0);
        assert_eq!(e.installed_size, 1);
        let old = "[[package]]\nname = \"a\"\nversion = \"1\"\nrel = 1\narch = \"x86_64\"\nfile = \"a.nawa.tar.zst\"\nsha256 = \"00\"\n";
        let parsed: Index = toml::from_str(old).unwrap();
        assert_eq!((parsed.packages[0].size, parsed.packages[0].installed_size), (0, 0));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn v2_index_has_explicit_archive_authentication_metadata() {
        let meta = RepositoryMeta::v2();
        let index = Index {
            generated: Some(10),
            expires: Some(20),
            repository: Some(meta),
            recipes: None,
            groups: None,
            packages: vec![RepoEntry {
                name: "bash".into(),
                version: "5.2".into(),
                rel: 1,
                arch: "x86_64".into(),
                file: "bash-5.2-1.nawa.tar.zst".into(),
                sha256: "a".repeat(64),
                download_url: None,
                size: 123,
                installed_size: 456,
            }],
        };
        index.validate().unwrap();
        let serialized = toml::to_string(&index).unwrap();
        assert!(serialized.contains("schema = 2"));
        assert!(serialized.contains("archive_auth = \"signed-index\""));
        let parsed: Index = toml::from_str(&serialized).unwrap();
        parsed.validate().unwrap();
        assert_eq!(parsed.repository.unwrap().schema, 2);
    }

    #[test]
    fn v3_requires_authenticated_per_package_https_locations() {
        let mut index = Index {
            generated: Some(10),
            expires: Some(20),
            repository: Some(RepositoryMeta::v3_per_package_url()),
            recipes: None,
            groups: None,
            packages: vec![RepoEntry {
                name: "bash".into(),
                version: "5.2".into(),
                rel: 1,
                arch: "x86_64".into(),
                file: "bash-5.2-1.nawa.tar.zst".into(),
                sha256: "a".repeat(64),
                download_url: Some("https://github.com/example/repo/releases/download/packages-0001/sha256-a.nawa.tar.zst".into()),
                size: 123,
                installed_size: 456,
            }],
        };
        index.validate().unwrap();

        index.packages[0].download_url = Some("http://github.com/example/repo/file.nawa.tar.zst".into());
        assert!(index.validate().unwrap_err().to_string().contains("HTTPS"));

        index.packages[0].download_url = Some("https://user:password@github.com/example/repo/file.nawa.tar.zst".into());
        assert!(index.validate().is_err());

        index.packages[0].download_url = None;
        assert!(index.validate().unwrap_err().to_string().contains("download_url"));
    }

    #[test]
    fn repo_create_v3_applies_package_locations_manifest() {
        let base = std::env::temp_dir().join(format!("nawa-repo-v3-locations-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let dir = base.join("repo");
        let staging = base.join("staging");
        let recipes = base.join("recipes");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::create_dir_all(&staging).unwrap();
        std::fs::create_dir_all(recipes.join("base/demo")).unwrap();
        std::fs::write(recipes.join("base/demo/recipe.toml"), "[package]\nname = \"demo\"\nversion = \"1.0\"\n").unwrap();
        std::fs::write(staging.join("bin"), b"bash").unwrap();
        archive::create(
            &staging,
            "pkgname = demo\npkgver = 1.0\npkgrel = 1\narch = x86_64\nsize = 4\n",
            &dir.join("demo-1.0-1.nawa.tar.zst"),
        ).unwrap();

        let locations = base.join("locations.toml");
        std::fs::write(
            &locations,
            r#"schema = 1

[package_urls]
"demo-1.0-1.nawa.tar.zst" = "https://github.com/example/repo/releases/download/packages-0001/sha256-demo.nawa.tar.zst"

[recipe_urls]
"recipes-abc123.recipes.tar.zst" = "https://github.com/example/repo/releases/download/packages-0001/sha256-recipes.tar.zst"
"#,
        ).unwrap();

        let index_path = dir.join("index.toml");
        let index = create_index_with_locations(
            &dir, &index_path, Some((&recipes, "abc123")), INDEX_SCHEMA_V3, Some(&locations)
        ).unwrap();
        assert_eq!(
            index.packages[0].download_url.as_deref(),
            Some("https://github.com/example/repo/releases/download/packages-0001/sha256-demo.nawa.tar.zst")
        );
        assert_eq!(
            index.recipes.as_ref().unwrap().download_url.as_deref(),
            Some("https://github.com/example/repo/releases/download/packages-0001/sha256-recipes.tar.zst")
        );
        let loaded = load_index(&index_path).unwrap();
        assert_eq!(
            loaded.repository.unwrap().archive_location,
            ARCHIVE_LOCATION_PER_PACKAGE_URL
        );
        assert_eq!(
            loaded.recipes.unwrap().download_url.as_deref(),
            Some("https://github.com/example/repo/releases/download/packages-0001/sha256-recipes.tar.zst")
        );
        assert!(create_index_with_version(&dir, &index_path, None, INDEX_SCHEMA_V3).is_err());

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn v2_index_defaults_to_base_url_without_changing_legacy_shape() {
        let meta = RepositoryMeta::v2();
        assert_eq!(meta.archive_location, ARCHIVE_LOCATION_BASE_URL);
        let serialized = toml::to_string(&meta).unwrap();
        assert!(!serialized.contains("archive_location"));
        let parsed: RepositoryMeta = toml::from_str(
            "schema = 2\narchive_auth = \"signed-index\"\nhash_algorithm = \"sha256\"\nindex_signature = \"nawa-sig-v1\"\n"
        ).unwrap();
        assert_eq!(parsed.archive_location, ARCHIVE_LOCATION_BASE_URL);
    }

    #[test]
    fn v2_rejects_unknown_authentication_and_malformed_entries() {
        let mut index = Index {
            generated: Some(10),
            expires: Some(20),
            repository: Some(RepositoryMeta::v2()),
            recipes: None,
            groups: None,
            packages: vec![RepoEntry {
                name: "bash".into(),
                version: "5.2".into(),
                rel: 1,
                arch: "x86_64".into(),
                file: "bash-5.2-1.nawa.tar.zst".into(),
                sha256: "a".repeat(64),
                download_url: None,
                size: 123,
                installed_size: 456,
            }],
        };
        index.repository.as_mut().unwrap().archive_auth = "detached".into();
        assert!(index.validate().unwrap_err().to_string().contains("archive_auth"));
        index.repository.as_mut().unwrap().archive_auth = ARCHIVE_AUTH_SIGNED_INDEX.into();
        index.packages[0].sha256 = "xyz".into();
        assert!(index.validate().unwrap_err().to_string().contains("SHA-256"));
    }

    // ---- repo-update ------------------------------------------------------

    fn mk_archive(root: &Path, dir: &Path, name: &str, ver: &str, rel: u32) {
        let st = root.join(format!("st-{name}-{ver}-{rel}"));
        std::fs::create_dir_all(&st).unwrap();
        std::fs::write(st.join("f"), format!("{name}{ver}{rel}")).unwrap();
        let info = format!("pkgname = {name}\npkgver = {ver}\npkgrel = {rel}\narch = x86_64\nsize = 7\n");
        archive::create(&st, &info, &dir.join(format!("{name}-{ver}-{rel}.nawa.tar.zst"))).unwrap();
    }

    fn scratch(tag: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!("nawa-repo-update-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    /// Only the package set matters for equivalence, not generated/expires.
    fn shape(i: &Index) -> String {
        format!("{:?}", i.packages)
    }

    #[test]
    fn repo_update_equals_repo_create_over_the_same_archives() {
        let base = scratch("equiv");
        let a = base.join("a");
        std::fs::create_dir_all(&a).unwrap();
        for (n, v) in [("demo", "1.0"), ("keep", "1.0"), ("old", "1.0"), ("swap", "1.0")] {
            mk_archive(&base, &a, n, v, 1);
        }
        let base_idx = create_index_with_version(&a, &base.join("a.toml"), None, INDEX_SCHEMA_V2).unwrap();

        // the delta: new swap, new fresh, an OLDER demo (must be ignored), `old` gone
        let new = base.join("new");
        std::fs::create_dir_all(&new).unwrap();
        mk_archive(&base, &new, "swap", "2.0", 1);
        mk_archive(&base, &new, "fresh", "1.0", 1);
        mk_archive(&base, &new, "demo", "0.5", 1);
        let new_idx = create_index_with_version(&new, &base.join("new.toml"), None, INDEX_SCHEMA_V2).unwrap();

        let updated = update_index(
            &base_idx,
            IndexDelta {
                add: new_idx.packages.clone(),
                remove_files: vec!["old-1.0-1.nawa.tar.zst".into()],
                drop_packages: vec!["old".into()],
            },
            None,
            1_000,
        ).unwrap();

        // reference: one directory holding (A - old) + new, indexed from scratch
        let full = base.join("full");
        std::fs::create_dir_all(&full).unwrap();
        for f in std::fs::read_dir(&a).unwrap().chain(std::fs::read_dir(&new).unwrap()) {
            let f = f.unwrap().path();
            if f.extension().map(|x| x == "zst").unwrap_or(false)
                && !f.file_name().unwrap().to_string_lossy().starts_with("old-")
            {
                std::fs::copy(&f, full.join(f.file_name().unwrap())).unwrap();
            }
        }
        let reference = create_index_with_version(&full, &base.join("full.toml"), None, INDEX_SCHEMA_V2).unwrap();
        assert_eq!(shape(&updated), shape(&reference));
        assert_eq!(updated.find("swap").unwrap().version, "2.0");
        assert_eq!(updated.find("demo").unwrap().version, "1.0");
        assert!(updated.find("old").is_none());
        assert_eq!(updated.generated, Some(1_000));
        assert_eq!(updated.expires, Some(1_000 + INDEX_VALIDITY_SECS));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn repo_update_without_a_delta_only_refreshes_the_validity_window() {
        let base = scratch("refresh");
        let a = base.join("a");
        std::fs::create_dir_all(&a).unwrap();
        mk_archive(&base, &a, "demo", "1.0", 1);
        let idx = create_index_with_version(&a, &base.join("a.toml"), None, INDEX_SCHEMA_V2).unwrap();
        let later = idx.generated.unwrap() + 80 * 24 * 3600;
        let refreshed = update_index(&idx, IndexDelta::default(), None, later).unwrap();
        assert_eq!(shape(&refreshed), shape(&idx));
        assert_eq!(refreshed.expires, Some(later + INDEX_VALIDITY_SECS));
        assert!(refreshed.expires > idx.expires);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn repo_update_readding_an_entry_is_a_no_op_and_changing_it_is_an_error() {
        let base = scratch("immutable");
        let a = base.join("a");
        std::fs::create_dir_all(&a).unwrap();
        mk_archive(&base, &a, "demo", "1.0", 1);
        let idx = create_index_with_version(&a, &base.join("a.toml"), None, INDEX_SCHEMA_V2).unwrap();
        let same = update_index(
            &idx,
            IndexDelta { add: idx.packages.clone(), ..Default::default() },
            None, 5,
        ).unwrap();
        assert_eq!(shape(&same), shape(&idx));

        let mut changed = idx.packages.clone();
        changed[0].sha256 = "b".repeat(64);
        let err = update_index(&idx, IndexDelta { add: changed, ..Default::default() }, None, 5).unwrap_err();
        assert!(err.to_string().contains("immutable"), "{err}");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn repo_update_refuses_removals_that_would_lose_a_package() {
        let base = scratch("removal");
        let a = base.join("a");
        std::fs::create_dir_all(&a).unwrap();
        mk_archive(&base, &a, "demo", "1.0", 1);
        let idx = create_index_with_version(&a, &base.join("a.toml"), None, INDEX_SCHEMA_V2).unwrap();

        let lose = IndexDelta { remove_files: vec!["demo-1.0-1.nawa.tar.zst".into()], ..Default::default() };
        let err = update_index(&idx, lose, None, 5).unwrap_err();
        assert!(err.to_string().contains("without an entry"), "{err}");

        // explicit drop is fine; so is removing an archive that was never indexed
        let drop = IndexDelta {
            remove_files: vec!["demo-1.0-1.nawa.tar.zst".into()],
            drop_packages: vec!["demo".into()],
            ..Default::default()
        };
        assert!(update_index(&idx, drop, None, 5).unwrap().packages.is_empty());
        let noop = IndexDelta { remove_files: vec!["demo-0.1-1.nawa.tar.zst".into()], ..Default::default() };
        assert_eq!(shape(&update_index(&idx, noop, None, 5).unwrap()), shape(&idx));

        let typo = IndexDelta { drop_packages: vec!["nope".into()], ..Default::default() };
        assert!(update_index(&idx, typo, None, 5).unwrap_err().to_string().contains("not in the base index"));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn repo_update_removal_can_expose_an_older_archive_when_it_is_added() {
        let base = scratch("expose");
        let a = base.join("a");
        std::fs::create_dir_all(&a).unwrap();
        mk_archive(&base, &a, "demo", "1.0", 1);
        mk_archive(&base, &a, "demo", "2.0", 1);
        let idx = create_index_with_version(&a, &base.join("a.toml"), None, INDEX_SCHEMA_V2).unwrap();
        assert_eq!(idx.find("demo").unwrap().version, "2.0");

        // roll back: 2.0 leaves, the caller supplies the 1.0 entry to take over
        let only_old = base.join("only-old");
        std::fs::create_dir_all(&only_old).unwrap();
        std::fs::copy(a.join("demo-1.0-1.nawa.tar.zst"), only_old.join("demo-1.0-1.nawa.tar.zst")).unwrap();
        let old_entry = create_index_with_version(&only_old, &base.join("o.toml"), None, INDEX_SCHEMA_V2).unwrap();
        let rolled = update_index(
            &idx,
            IndexDelta {
                add: old_entry.packages,
                remove_files: vec!["demo-2.0-1.nawa.tar.zst".into()],
                ..Default::default()
            },
            None, 5,
        ).unwrap();
        assert_eq!(rolled.find("demo").unwrap().version, "1.0");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn repo_update_rejects_v1_bases_and_mismatched_file_names() {
        let base = scratch("reject");
        let a = base.join("a");
        std::fs::create_dir_all(&a).unwrap();
        mk_archive(&base, &a, "demo", "1.0", 1);
        let v1 = create_index(&a, &base.join("v1.toml")).unwrap();
        assert!(update_index(&v1, IndexDelta::default(), None, 5).unwrap_err().to_string().contains("schema-2 or schema-3"));

        let v2 = create_index_with_version(&a, &base.join("v2.toml"), None, INDEX_SCHEMA_V2).unwrap();
        let mut bad = v2.packages[0].clone();
        bad.name = "other".into();
        let err = update_index(&v2, IndexDelta { add: vec![bad], ..Default::default() }, None, 5).unwrap_err();
        assert!(err.to_string().contains("expected"), "{err}");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn repo_update_files_fill_v3_urls_from_locations_and_keep_recipes() {
        let base = scratch("files");
        let a = base.join("a");
        std::fs::create_dir_all(&a).unwrap();
        mk_archive(&base, &a, "demo", "1.0", 1);
        let loc = base.join("loc.toml");
        std::fs::write(&loc, "schema = 1\n[package_urls]\n\"demo-1.0-1.nawa.tar.zst\" = \"https://github.com/e/r/releases/download/packages-0001/demo.nawa.tar.zst\"\n\"extra-1.0-1.nawa.tar.zst\" = \"https://github.com/e/r/releases/download/packages-0001/extra.nawa.tar.zst\"\n").unwrap();
        let idx = create_index_with_locations(&a, &base.join("a.toml"), None, INDEX_SCHEMA_V3, Some(&loc)).unwrap();

        let new = base.join("new");
        std::fs::create_dir_all(&new).unwrap();
        mk_archive(&base, &new, "extra", "1.0", 1);
        let new_idx = create_index_with_locations(&new, &base.join("n.toml"), None, INDEX_SCHEMA_V3, Some(&loc)).unwrap();
        // entries without download_url: the manifest must supply it
        let mut entry = new_idx.packages[0].clone();
        entry.download_url = None;
        let entries = base.join("entries.toml");
        std::fs::write(&entries, toml::to_string(&Index {
            generated: None, expires: None, repository: None, recipes: None, groups: None, packages: vec![entry],
        }).unwrap()).unwrap();

        let out = base.join("out.toml");
        let updated = update_index_files(
            &idx,
            &base,
            &out,
            UpdateIndexFilesInputs {
                entries_path: Some(&entries),
                locations_path: Some(&loc),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(updated.packages.len(), 2);
        assert_eq!(
            updated.find("extra").unwrap().download_url.as_deref(),
            Some("https://github.com/e/r/releases/download/packages-0001/extra.nawa.tar.zst")
        );
        load_index(&out).unwrap();

        // no manifest and no URL in the entry: schema-3 validation refuses
        assert!(update_index_files(
            &idx,
            &base,
            &out,
            UpdateIndexFilesInputs {
                entries_path: Some(&entries),
                ..Default::default()
            },
        )
        .is_err());
        let _ = std::fs::remove_dir_all(&base);
    }
}
