//! Packing helpers.

#[allow(unused_imports)]
use anyhow::{bail, Context, Result};
use std::path::Path;

use super::model::*;
use walkdir;

/// A revision is a commit sha or tag: it ends up in a file name, so keep it
/// to a conservative alphabet.
pub(crate) fn validate_rev(rev: &str) -> Result<()> {
    let ok = !rev.is_empty()
        && rev.len() <= 64
        && !rev.starts_with('.')
        && rev
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if !ok {
        anyhow::bail!("invalid recipes revision `{rev}` (use a commit sha or tag: [A-Za-z0-9._-], max 64)");
    }
    Ok(())
}

/// Pack the recipe tree into recipes-<rev>.recipes.tar.zst and describe it.
/// .git is skipped; symlinks and special files are refused — a recipe tree is plain data.
pub(crate) fn pack_recipes(tree: &Path, out_dir: &Path, rev: &str) -> Result<RecipesEntry> {
    validate_rev(rev)?;
    if !tree.is_dir() {
        anyhow::bail!("recipes directory not found: {}", tree.display());
    }
    let file = format!("recipes-{rev}{RECIPES_SUFFIX}");
    let dest = out_dir.join(&file);
    let part = out_dir.join(format!(".{file}.part"));
    let f = std::fs::File::create(&part).with_context(|| format!("create {}", part.display()))?;
    let enc = zstd::Encoder::new(f, 19)?;
    let mut b = tar::Builder::new(enc);
    b.mode(tar::HeaderMode::Deterministic);
    b.follow_symlinks(false);
    let mut count = 0usize;
    for entry in walkdir::WalkDir::new(tree)
        .min_depth(1)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|e| e.file_name() != ".git")
    {
        let entry = entry?;
        let rel = entry.path().strip_prefix(tree)?;
        let ft = entry.file_type();
        if ft.is_dir() {
            b.append_dir(rel, entry.path())?;
        } else if ft.is_file() {
            b.append_path_with_name(entry.path(), rel)?;
            if entry.file_name() == "recipe.toml" {
                count += 1;
            }
        } else {
            anyhow::bail!("{}: symlinks and special files are not allowed in a recipe tree", entry.path().display());
        }
    }
    if count == 0 {
        anyhow::bail!("{} contains no recipe.toml — not a recipe tree", tree.display());
    }
    b.into_inner()?.finish()?;
    std::fs::rename(&part, &dest).with_context(|| format!("write {}", dest.display()))?;
    let sha256 = crate::fetch::sha256_of_file(&dest)?;
    crate::nawa_info!("index -> recipes {rev} ({count} recipes)");
    Ok(RecipesEntry { rev: rev.to_string(), file, sha256, download_url: None })
}


/// Validate and copy a group catalog into the repository, then return the
/// digest reference that the signed index will authenticate.
pub(crate) fn pack_groups(source: &Path, out_dir: &Path) -> Result<GroupsEntry> {
    let raw = std::fs::read(source)
        .with_context(|| format!("read group catalog {}", source.display()))?;
    let text = std::str::from_utf8(&raw).context("group catalog is not valid UTF-8")?;
    crate::groups::GroupCatalog::from_toml(text)
        .with_context(|| format!("validate group catalog {}", source.display()))?;

    let dest = out_dir.join("groups.toml");
    if source != dest {
        std::fs::write(&dest, &raw)
            .with_context(|| format!("write group catalog {}", dest.display()))?;
    }
    let sha256 = crate::fetch::sha256_of_file(&dest)?;
    let size = std::fs::metadata(&dest)
        .with_context(|| format!("stat group catalog {}", dest.display()))?
        .len();
    if size == 0 {
        bail!("group catalog {} is empty", source.display());
    }
    crate::nawa_success!("group metadata -> {} ({size} bytes)", dest.display());
    Ok(GroupsEntry { schema: 1, file: "groups.toml".into(), sha256, size })
}

