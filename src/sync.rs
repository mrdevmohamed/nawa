//! `nawa sync` — fetch the newest recipe tree from the repository.
//!
//! The recipe tree is published inside the (signed) repository index:
//! `[recipes] rev / file / sha256`. Trust therefore needs nothing new — the
//! index goes through the same checks as `nawa upgrade` (trusted key,
//! signature, rollback and freeze protection) and the tarball must match
//! the sha256 the index promised. The tree is unpacked beside the target
//! and swapped in with two renames, so an interrupted sync never leaves a
//! half-old, half-new tree.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

use crate::fetch::{self, RepoRef};
use crate::{repo, upgrade};

pub struct SyncOpts {
    pub force: bool,
    pub allow_unsigned: bool,
}

/// Where the synced revision is recorded (`nawa config show` / debugging).
pub fn rev_file(root: &Path) -> PathBuf {
    root.join("var/lib/nawa/recipes.rev")
}

pub fn sync(root: &Path, repo_ref: &RepoRef, dest: &Path, opts: &SyncOpts) -> Result<()> {
    let open = upgrade::open_repo(root, repo_ref, opts.allow_unsigned, opts.force)?;
    let rec = match &open.index.recipes {
        Some(r) => r.clone(),
        None => bail!(
            "this repository publishes no recipe tree\n(the maintainer adds it with `nawa repo-create --recipes <dir> --recipes-rev <rev>`)"
        ),
    };
    repo::validate_rev(&rec.rev)?;
    upgrade::validate_entry_filename(&rec.file)?;
    if !rec.file.ends_with(repo::RECIPES_SUFFIX) {
        bail!("index lists `{}` as the recipe tree — unexpected file name", rec.file);
    }

    let rev_path = rev_file(root);
    let current = std::fs::read_to_string(&rev_path).ok().map(|s| s.trim().to_string());
    if current.as_deref() == Some(rec.rev.as_str()) && dest.is_dir() {
        crate::nawa_info!("recipes already at {} ({})", rec.rev, dest.display());
        return Ok(());
    }

    // the tarball: located (local repo) or downloaded (remote), then checked
    // against the sha256 in the authenticated index
    let tarball = match &open.base_url {
        None if open.index.repository.as_ref().map(|m| m.archive_location.as_str())
            == Some(repo::ARCHIVE_LOCATION_PER_PACKAGE_URL) =>
        {
            let source_url = upgrade::archive_download_url(
                "", &open.index, &rec.file, rec.download_url.as_deref()
            )?;
            let cache = fetch::remote_cache_dir(root, &source_url);
            std::fs::create_dir_all(&cache)
                .with_context(|| format!("create recipe archive cache {}", cache.display()))?;
            let path = cache.join(&rec.file);
            fetch::download_cached(&source_url, &path, Some(&rec.sha256))?;
            path
        }
        None => open.dir.join(&rec.file),
        Some(url) => {
            let path = open.dir.join(&rec.file);
            let source_url = upgrade::archive_download_url(
                url, &open.index, &rec.file, rec.download_url.as_deref()
            )?;
            fetch::download_cached(&source_url, &path, Some(&rec.sha256))?;
            path
        }
    };
    if !tarball.is_file() {
        bail!("recipe archive {} listed in the index is missing", tarball.display());
    }
    let snapshot_dir = root.join(".nawa");
    let snapshot = fetch::snapshot_file(&tarball, Some(&rec.sha256), &snapshot_dir)?;
    let verified_tarball = snapshot.path();

    let parent = dest.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    let name = dest.file_name().context("recipes path has no name")?.to_string_lossy().to_string();
    let pid = std::process::id();
    let staging = parent.join(format!(".{name}.sync-{pid}"));
    let old = parent.join(format!(".{name}.old-{pid}"));
    let _ = std::fs::remove_dir_all(&staging);
    let _ = std::fs::remove_dir_all(&old);

    let result = (|| -> Result<usize> {
        fetch::extract_source_archive(verified_tarball, &staging)?;
        // an archive without a single recipe must never replace a working tree
        let n = walkdir::WalkDir::new(&staging)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name() == "recipe.toml")
            .count();
        if n == 0 {
            bail!("the downloaded archive contains no recipe.toml — keeping the current tree");
        }
        if dest.exists() {
            std::fs::rename(dest, &old).with_context(|| {
                format!("move aside {} (is it a mount point? is it writable?)", dest.display())
            })?;
        }
        if let Err(e) = std::fs::rename(&staging, dest) {
            if old.exists() {
                let _ = std::fs::rename(&old, dest); // put the old tree back
            }
            return Err(e).with_context(|| format!("install {}", dest.display()));
        }
        Ok(n)
    })();
    let count = match result {
        Ok(n) => n,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(e);
        }
    };
    let _ = std::fs::remove_dir_all(&old);

    if let Some(dir) = rev_path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    let tmp = rev_path.with_extension("rev.part");
    std::fs::write(&tmp, format!("{}\n", rec.rev))?;
    std::fs::rename(&tmp, &rev_path)?;
    crate::nawa_info!("recipes synced to {} ({count} recipes) -> {}", rec.rev, dest.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Env {
        base: PathBuf,
        repo: PathBuf,
        root: PathBuf,
        dest: PathBuf,
    }

    fn env(tag: &str) -> Env {
        let base = std::env::temp_dir().join(format!("nawa-sync-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let e = Env {
            repo: base.join("repo"),
            root: base.join("root"),
            dest: base.join("root/usr/share/nawa/recipes"),
            base,
        };
        std::fs::create_dir_all(&e.repo).unwrap();
        std::fs::create_dir_all(&e.root).unwrap();
        e
    }

    fn tree(base: &Path, name: &str, marker: &str) -> PathBuf {
        let t = base.join(name);
        std::fs::create_dir_all(t.join("base/hello")).unwrap();
        std::fs::write(t.join("base/hello/recipe.toml"), format!("# {marker}\n")).unwrap();
        t
    }

    fn publish(e: &Env, t: &Path, rev: &str) {
        repo::create_index_with(&e.repo, &e.repo.join("index.toml"), Some((t, rev))).unwrap();
    }

    fn run(e: &Env) -> Result<()> {
        sync(&e.root, &RepoRef::Local(e.repo.clone()), &e.dest,
             &SyncOpts { force: false, allow_unsigned: true })
    }

    #[test]
    fn sync_installs_then_updates_the_tree() {
        let e = env("roundtrip");
        publish(&e, &tree(&e.base, "t1", "one"), "aaa111");
        run(&e).unwrap();
        assert_eq!(std::fs::read_to_string(e.dest.join("base/hello/recipe.toml")).unwrap(), "# one\n");
        assert_eq!(std::fs::read_to_string(rev_file(&e.root)).unwrap().trim(), "aaa111");

        publish(&e, &tree(&e.base, "t2", "two"), "bbb222");
        run(&e).unwrap();
        assert_eq!(std::fs::read_to_string(e.dest.join("base/hello/recipe.toml")).unwrap(), "# two\n");
        assert_eq!(std::fs::read_to_string(rev_file(&e.root)).unwrap().trim(), "bbb222");
        // no staging / backup leftovers next to the tree
        let leftovers: Vec<_> = std::fs::read_dir(e.dest.parent().unwrap()).unwrap()
            .filter_map(|x| x.ok()).map(|x| x.file_name().to_string_lossy().to_string())
            .filter(|n| n.starts_with('.')).collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
        let _ = std::fs::remove_dir_all(&e.base);
    }

    #[test]
    fn tampered_archive_is_rejected_and_tree_is_kept() {
        let e = env("tamper");
        publish(&e, &tree(&e.base, "t1", "one"), "aaa111");
        run(&e).unwrap();
        publish(&e, &tree(&e.base, "t2", "two"), "bbb222");
        let tb = e.repo.join("recipes-bbb222.recipes.tar.zst");
        let mut bytes = std::fs::read(&tb).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0xff;
        std::fs::write(&tb, bytes).unwrap();
        let err = run(&e).unwrap_err();
        assert!(format!("{err:#}").contains("sha256 mismatch"), "{err:#}");
        assert_eq!(std::fs::read_to_string(e.dest.join("base/hello/recipe.toml")).unwrap(), "# one\n");
        let _ = std::fs::remove_dir_all(&e.base);
    }

    #[test]
    fn repository_without_recipes_is_an_error() {
        let e = env("norecipes");
        repo::create_index(&e.repo, &e.repo.join("index.toml")).unwrap();
        let err = run(&e).unwrap_err();
        assert!(format!("{err:#}").contains("no recipe tree"), "{err:#}");
        assert!(!e.dest.exists());
        let _ = std::fs::remove_dir_all(&e.base);
    }

    #[test]
    fn same_revision_is_a_noop() {
        let e = env("noop");
        publish(&e, &tree(&e.base, "t1", "one"), "aaa111");
        run(&e).unwrap();
        std::fs::write(e.dest.join("marker"), "x").unwrap();
        run(&e).unwrap();
        assert!(e.dest.join("marker").exists(), "an up-to-date tree must not be replaced");
        let _ = std::fs::remove_dir_all(&e.base);
    }

    #[test]
    fn recipes_archive_does_not_pollute_the_package_index() {
        let e = env("index");
        publish(&e, &tree(&e.base, "t1", "one"), "aaa111");
        let idx = repo::load_index(&e.repo.join("index.toml")).unwrap();
        assert!(idx.packages.is_empty());
        assert_eq!(idx.recipes.unwrap().rev, "aaa111");
        let _ = std::fs::remove_dir_all(&e.base);
    }

    #[test]
    fn unsafe_revisions_are_refused() {
        for bad in ["", "../x", "a/b", ".hidden", "a b"] {
            assert!(repo::validate_rev(bad).is_err(), "{bad}");
        }
        assert!(repo::validate_rev("16fbe42d940a55b21d15ff03406b359af0e1bbcc").is_ok());
    }
    #[test]
    fn trusted_v2_index_authenticates_recipe_tree() {
        let e = env("trusted-v2");
        let (secret, public) = crate::sign::keygen(&e.base.join("publisher-keys")).unwrap();
        crate::sign::trust(&public, &e.root).unwrap();

        let first_tree = tree(&e.base, "v2-one", "v2-one");
        let index_path = e.repo.join("index.toml");
        repo::create_index_with_version(&e.repo, &index_path, Some((&first_tree, "aaa111")), 2).unwrap();
        crate::sign::sign(&index_path, &secret).unwrap();
        run(&e).unwrap();
        assert_eq!(
            std::fs::read_to_string(e.dest.join("base/hello/recipe.toml")).unwrap(),
            "# v2-one\n"
        );

        let second_tree = tree(&e.base, "v2-two", "v2-two");
        repo::create_index_with_version(&e.repo, &index_path, Some((&second_tree, "bbb222")), 2).unwrap();
        crate::sign::sign(&index_path, &secret).unwrap();
        let archive = e.repo.join("recipes-bbb222.recipes.tar.zst");
        let mut bytes = std::fs::read(&archive).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0x40;
        std::fs::write(&archive, bytes).unwrap();

        let err = run(&e).unwrap_err();
        assert!(format!("{err:#}").contains("sha256 mismatch"), "{err:#}");
        assert_eq!(
            std::fs::read_to_string(e.dest.join("base/hello/recipe.toml")).unwrap(),
            "# v2-one\n"
        );
        let _ = std::fs::remove_dir_all(&e.base);
    }

}
