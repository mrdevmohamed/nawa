//! The `closure` isolation level — docs/design/build-isolation.md §5.
//!
//! A build root is assembled from **built archives**, never from whatever
//! happens to be installed in `/`:
//!
//! ```text
//! buildroot(R) = closure( {base-devel} ∪ R.depends.build [∪ R.depends.check] )
//! closure(S)   = S plus, transitively, every member's depends.run
//! ```
//!
//! Nothing else exists in the build root, so a missing declaration fails as
//! `command not found` / `cannot find -lfoo` — *the failure is the audit*.
//! Archives are verified with the same trust policy as `nawa install` before
//! they are unpacked into the root. Assembly strategies (§5.3):
//!
//! * v1 `assemble` — extract every archive into a per-build scratch dir and
//!   delete it afterwards (always available).
//! * v2 `assemble_cached` — each archive is extracted **once** into
//!   `<cache>/layers/<archive-sha256>/` and a build root is an overlayfs
//!   over those layers (Phase 5; needs overlayfs in a user namespace,
//!   kernel ≥ 5.11 — the caller probes first and falls back to v1).

use anyhow::{bail, Context, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::recipe::Recipe;

/// The meta recipe that defines the base toolchain set: after it exists,
/// `depends.build` means "beyond base-devel" (§5.1).
pub const BASE_DEVEL: &str = "base-devel";

/// closure(S) = S plus, transitively, each member's `depends.run`.
pub fn closure_of(
    seeds: &[String],
    known: &BTreeMap<String, Recipe>,
) -> Result<Vec<String>> {
    let mut out: BTreeSet<String> = BTreeSet::new();
    let mut stack: Vec<String> = seeds.to_vec();
    while let Some(name) = stack.pop() {
        if !out.insert(name.clone()) {
            continue;
        }
        let rec = known
            .get(&name)
            .with_context(|| format!("closure member `{name}` has no recipe"))?;
        stack.extend(rec.depends.run.iter().cloned());
    }
    Ok(out.into_iter().collect())
}

/// The packages whose archives make up the build root for `target`
/// (§5.1). `with_check` adds `depends.check` (built with `--check`).
pub fn buildroot(target: &Recipe, known: &BTreeMap<String, Recipe>, with_check: bool) -> Result<Vec<String>> {
    let mut seeds: Vec<String> = Vec::new();
    if target.package.name == BASE_DEVEL {
        // base-devel DEFINES the base layer, so its own build root cannot
        // contain base-devel: that archive is the very thing being built
        // (a chicken-and-egg `dependency archive ... not found`). Use the
        // toolchain set it names (its run deps) as the base instead.
        seeds.extend(target.depends.run.iter().cloned());
    } else {
        seeds.push(BASE_DEVEL.to_string());
    }
    seeds.extend(target.depends.build.iter().cloned());
    if with_check {
        seeds.extend(target.depends.check.iter().cloned());
    }
    closure_of(&seeds, known)
}

/// Where a dependency's archive comes from (§5.2): `<builddir>/out`.
/// A missing archive means "build that dependency first" — exactly what
/// `deps-all` ordering guarantees.
pub fn archive_for(rec: &Recipe, outdir: &Path) -> PathBuf {
    outdir.join(format!("{}.nawa.tar.zst", rec.id()))
}

/// The assembled build root plus the index lint needs.
pub struct AssembledRoot {
    /// Host path of the root directory (owned by this build; deleted after).
    pub dir: PathBuf,
    /// soname (e.g. `libz.so.1`) → owning package, for the run-dep check.
    pub sonames: BTreeMap<String, String>,
    /// Every archive that went in, sorted by path — feeds the build id.
    pub archives: Vec<PathBuf>,
    /// True when the root was mounted from cached layers (v2) instead of
    /// being extracted per build (v1). Purely informational.
    pub layered: bool,
    /// v2 only: host paths of the layers, in overlayfs lowerdir order
    /// (first entry bottom-most). Empty for v1.
    pub layer_dirs: Vec<PathBuf>,
}

/// Extract every closure archive into `dir` (v1: extract-per-build).
/// Signatures are verified with the install-time policy first.
#[allow(clippy::too_many_arguments)]
pub fn assemble(
    members: &[String],
    known: &BTreeMap<String, Recipe>,
    outdir: &Path,
    dir: &Path,
    root: &Path,
    allow_unsigned: bool,
    progress: impl Fn(&str),
) -> Result<AssembledRoot> {
    std::fs::create_dir_all(dir)
        .with_context(|| format!("create build root {}", dir.display()))?;

    let mut sonames: BTreeMap<String, String> = BTreeMap::new();
    let mut archives: Vec<PathBuf> = Vec::new();
    // extract in a stable order: deterministic trees, readable diffs
    let mut members: Vec<&String> = members.iter().collect();
    members.sort();

    for name in members {
        let rec = known
            .get(name.as_str())
            .with_context(|| format!("closure member `{name}` has no recipe"))?;
        let archive = archive_for(rec, outdir);
        if !archive.is_file() {
            bail!(
                "dependency archive for `{}` not found: {}\n\
                 (build it first — `nawa deps-all` prints a valid order)",
                name,
                archive.display()
            );
        }
        crate::sign::verify_for_install(&archive, root, None, allow_unsigned)
            .with_context(|| format!("verify {}", archive.display()))?;
        progress(name);
        extract_and_index(&archive, dir, name, &mut sonames)
            .with_context(|| format!("extract {} into the build root", archive.display()))?;
        archives.push(archive);
    }
    archives.sort();
    Ok(AssembledRoot {
        dir: dir.to_path_buf(),
        sonames,
        archives,
        layered: false,
        layer_dirs: Vec::new(),
    })
}

/// v2 (Phase 5): assemble the build root from the **layer cache** —
/// `<cache>/layers/<sha256-of-archive>/`, one directory per archive,
/// extracted once and reused by every later build whose closure contains
/// it. Returns the layer directories (ordered like the v1 extraction:
/// sorted members, later entries win conflicts) for the sandbox spec;
/// the actual overlayfs mount happens inside the sandbox. The build-root
/// mountpoint `dir` is created empty — the layers land on it at mount time.
///
/// Trust: the cache key is the archive's own sha256, and the archive is
/// signature-verified against the install-time policy on every assembly
/// (cache hits included) — the same guarantee as `nawa install`.
#[allow(clippy::too_many_arguments)]
pub fn assemble_cached(
    members: &[String],
    known: &BTreeMap<String, Recipe>,
    outdir: &Path,
    dir: &Path,
    cache: &Path,
    root: &Path,
    allow_unsigned: bool,
    progress: impl Fn(&str),
) -> Result<AssembledRoot> {
    let layers_dir = cache.join("layers");
    std::fs::create_dir_all(&layers_dir)
        .with_context(|| format!("create the layer cache {}", layers_dir.display()))?;
    // the mountpoint the sandbox will mount the layered root onto
    std::fs::create_dir_all(dir)
        .with_context(|| format!("create build root {}", dir.display()))?;

    let mut ordered: Vec<&String> = members.iter().collect();
    ordered.sort();

    let mut sonames: BTreeMap<String, String> = BTreeMap::new();
    let mut archives: Vec<PathBuf> = Vec::new();
    let mut layer_dirs: Vec<PathBuf> = Vec::new();

    for name in ordered {
        let rec = known
            .get(name.as_str())
            .with_context(|| format!("closure member `{name}` has no recipe"))?;
        let archive = archive_for(rec, outdir);
        if !archive.is_file() {
            bail!(
                "dependency archive for `{}` not found: {}\n\
                 (build it first — `nawa deps-all` prints a valid order)",
                name,
                archive.display()
            );
        }
        crate::sign::verify_for_install(&archive, root, None, allow_unsigned)
            .with_context(|| format!("verify {}", archive.display()))?;
        let sha = file_sha256(&archive)
            .with_context(|| format!("hash {}", archive.display()))?;
        let layer = layers_dir.join(&sha);
        if layer.is_dir() {
            println!("nawa: closure: layer cache hit for {name}");
        } else {
            progress(name);
            // extract next to the final name and rename: a half-written
            // layer must never be picked up by a parallel/failed run
            let tmp = layers_dir.join(format!(".tmp-{}-{}", std::process::id(), &sha[..16.min(sha.len())]));
            let _ = std::fs::remove_dir_all(&tmp);
            std::fs::create_dir_all(&tmp)
                .with_context(|| format!("create {}", tmp.display()))?;
            if let Err(e) = extract_and_index(&archive, &tmp, name, &mut BTreeMap::new()) {
                let _ = std::fs::remove_dir_all(&tmp);
                return Err(e).with_context(|| format!("extract {} into its layer", archive.display()));
            }
            if let Err(e) = std::fs::rename(&tmp, &layer) {
                let _ = std::fs::remove_dir_all(&tmp);
                return Err(anyhow::Error::new(e)
                    .context(format!("publish layer {}", layer.display())));
            }
        }
        index_layer_sonames(&layer, name, &mut sonames)?;
        layer_dirs.push(layer);
        archives.push(archive);
    }
    archives.sort();
    Ok(AssembledRoot {
        dir: dir.to_path_buf(),
        sonames,
        archives,
        layered: true,
        // overlayfs lowerdir order == the member order above: later entries
        // win conflicts, exactly like v1's extract-overwrite order
        layer_dirs,
    })
}

/// sha256 of a file, streamed (archives can be hundreds of MB).
fn file_sha256(p: &Path) -> Result<String> {
    use sha2::Digest;
    let f = std::fs::File::open(p)?;
    let mut r = std::io::BufReader::with_capacity(1 << 20, f);
    let mut h = sha2::Sha256::new();
    std::io::copy(&mut r, &mut h)?;
    Ok(hex::encode(h.finalize()))
}

/// Rebuild the soname → owner index from an already-extracted layer dir
/// (v2 cache hits skip extraction, so the index is rebuilt from disk).
fn index_layer_sonames(
    layer: &Path,
    name: &str,
    sonames: &mut BTreeMap<String, String>,
) -> Result<()> {
    for e in walkdir::WalkDir::new(layer).sort_by_file_name() {
        let e = e?;
        let rel = match e.path().strip_prefix(layer) {
            Ok(r) if !r.as_os_str().is_empty() => r,
            _ => continue,
        };
        let rel_s = rel.to_string_lossy().into_owned();
        if soname_of(&rel_s).is_some() {
            sonames.entry(owner_file_name(&rel_s)).or_insert_with(|| name.to_string());
        }
    }
    Ok(())
}

/// Extract one archive into the build root and index its libraries.
fn extract_and_index(
    archive: &Path,
    dir: &Path,
    name: &str,
    sonames: &mut BTreeMap<String, String>,
) -> Result<()> {
    let f = std::fs::File::open(archive)?;
    let dec = zstd::Decoder::new(f)?;
    let mut ar = tar::Archive::new(dec);
    for entry in ar.entries()? {
        let mut entry = entry?;
        let p = entry.path()?.to_path_buf();
        let Some(rel) = p.strip_prefix(crate::archive::PAYLOAD).ok().filter(|r| !r.as_os_str().is_empty()) else {
            continue;
        };
        crate::archive::extract_entry_into(&mut entry, rel, dir)?;
        // soname → owner index for the lint's run-dependency check: every
        // file directly in a library dir whose name looks like a shared
        // object (libfoo.so[.1[.2...]])
        let rel_s = rel.to_string_lossy().into_owned();
        if soname_of(&rel_s).is_some() {
            sonames.entry(owner_file_name(&rel_s)).or_insert_with(|| name.to_string());
        }
    }
    Ok(())
}

fn owner_file_name(rel: &str) -> String {
    rel.rsplit('/').next().unwrap_or(rel).to_string()
}

/// Is this payload path a shared library that will satisfy `-l`/DT_NEEDED?
/// `/usr/lib*`, `/lib*`, files starting `lib` and containing `.so`.
fn soname_of(rel: &str) -> Option<String> {
    // library dirs are /lib, /lib64 and /usr/lib* — note the first path
    // component of "usr/lib/x" is "usr", so check the prefix, not just it
    let mut comps = rel.splitn(3, '/');
    let (a, b) = (comps.next()?, comps.next()?);
    let is_libdir = match a {
        "lib" | "lib64" => true,
        "usr" => b.starts_with("lib"),
        _ => false,
    };
    if !is_libdir {
        return None;
    }
    let base = owner_file_name(rel);
    if base.starts_with("lib") && base.contains(".so") && !base.ends_with(".a") && !base.ends_with(".la") {
        Some(base)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(name: &str, build: &[&str], run: &[&str]) -> Recipe {
        let toml_src = format!(
            r#"
            [package]
            name = "{name}"
            version = "1.0"

            [depends]
            build = [{}]
            run = [{}]

            [build]
            install = "true"
            "#,
            build.iter().map(|d| format!("{d:?}")).collect::<Vec<_>>().join(", "),
            run.iter().map(|d| format!("{d:?}")).collect::<Vec<_>>().join(", ")
        );
        toml::from_str(&toml_src).unwrap()
    }

    fn map(recs: Vec<Recipe>) -> BTreeMap<String, Recipe> {
        recs.into_iter().map(|r| (r.package.name.clone(), r)).collect()
    }

    #[test]
    fn closure_pulls_run_deps_transitively() {
        let known = map(vec![
            rec("base-devel", &[], &["gcc", "zlib"]),
            rec("gcc", &["mpfr"], &["gmp", "mpfr"]),
            rec("zlib", &[], &[]),
            rec("gmp", &[], &[]),
            rec("mpfr", &[], &["gmp"]),
        ]);
        // closure({base-devel}) = base-devel + its run deps, transitively
        let c = closure_of(&["base-devel".to_string()], &known).unwrap();
        assert_eq!(
            c,
            vec!["base-devel", "gcc", "gmp", "mpfr", "zlib"]
        );
    }

    #[test]
    fn buildroot_is_base_plus_build_and_check_deps() {
        let target: Recipe = toml::from_str(
            r#"
            [package]
            name = "target"
            version = "1.0"

            [depends]
            build = ["openssl"]
            run = ["zlib"]
            check = ["dejagnu"]

            [build]
            install = "true"
            "#,
        )
        .unwrap();
        let known = map(vec![
            rec("base-devel", &[], &[]),
            rec("zlib", &[], &[]),
            rec("openssl", &["zlib"], &["zlib"]),
            rec("dejagnu", &[], &[]),
        ]);
        let without = buildroot(&target, &known, false).unwrap();
        assert_eq!(without, vec!["base-devel", "openssl", "zlib"]);
        let with = buildroot(&target, &known, true).unwrap();
        assert!(with.contains(&"dejagnu".to_string()));
        assert_eq!(with.len(), 4);
    }

    #[test]
    fn base_devel_build_root_excludes_itself() {
        let target: Recipe = toml::from_str(
            r#"
            [package]
            name = "base-devel"
            version = "1"

            [depends]
            run = ["gcc", "zlib"]

            [build]
            install = "true"
            "#,
        )
        .unwrap();
        let known = map(vec![
            rec("base-devel", &[], &["gcc", "zlib"]),
            rec("gcc", &[], &["gmp"]),
            rec("gmp", &[], &[]),
            rec("zlib", &[], &[]),
        ]);
        let root = buildroot(&target, &known, false).unwrap();
        assert_eq!(root, vec!["gcc", "gmp", "zlib"]);
    }

    #[test]
    fn missing_member_is_an_error_not_a_guess() {
        let known = map(vec![]);
        assert!(closure_of(&["ghost".to_string()], &known).is_err());
    }

    #[test]
    fn soname_detection() {
        assert_eq!(
            soname_of("usr/lib/libz.so.1.3"),
            Some("libz.so.1.3".to_string())
        );
        assert_eq!(soname_of("lib64/libssl.so.3"), Some("libssl.so.3".to_string()));
        // static libs, libtool files and non-lib dirs are not sonames
        assert_eq!(soname_of("usr/lib/libz.a"), None);
        assert_eq!(soname_of("usr/lib/libz.la"), None);
        assert_eq!(soname_of("usr/bin/libz.so.1"), None);
        // versioned linker scripts (libfoo.so) still count
        assert_eq!(soname_of("usr/lib/libfoo.so"), Some("libfoo.so".to_string()));
    }
}
