//! Post-install hooks and root-constrained path resolution.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::archive;
use crate::db::{Db, Entry};
use crate::interrupt;

pub(crate) fn hooks_path(db: &Db, name: &str) -> PathBuf {
    db.local_dir().join(name).join("hooks")
}

pub(crate) fn hook_status_path(db: &Db, name: &str) -> PathBuf {
    db.local_dir().join(name).join("hook-status")
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct HookStatus {
    pub(crate) version: String,
    pub(crate) label: String,
    pub(crate) status: String,
}

pub(crate) fn save_hook_status(db: &Db, name: &str, state: &HookStatus) -> Result<()> {
    let path = hook_status_path(db, name);
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, toml::to_string(state)?)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Return true when the installed package has a post-install hook that has
/// not been confirmed complete for its currently installed version.
///
/// Missing status files are treated as pending for one-time migration from
/// Nawa versions that stored hooks but did not persist their completion state.
pub(crate) fn hook_needs_retry(root: &Path, name: &str) -> Result<bool> {
    let db = Db::new(root);
    let entry = match db.entry(name)? {
        Some(entry) => entry,
        None => return Ok(false),
    };
    if load_hooks(&db, name).post_install.trim().is_empty() {
        return Ok(false);
    }
    let state = std::fs::read_to_string(hook_status_path(&db, name))
        .ok()
        .and_then(|raw| toml::from_str::<HookStatus>(&raw).ok());
    Ok(!matches!(
        state,
        Some(s) if s.version == entry.version_full() && s.status == "completed"
    ))
}

/// Retry only the saved post-install hook; package payload files are not
/// downloaded, extracted, replaced, or re-registered.
pub(crate) fn retry_pending_hook(name: &str, root: &Path) -> Result<()> {
    let db = Db::new(root);
    let entry = db.entry(name)?.ok_or_else(|| anyhow::anyhow!("package {name} is not installed"))?;
    if !hook_needs_retry(root, name)? {
        return Ok(());
    }
    let hooks = load_hooks(&db, name);
    if hooks.post_install.trim().is_empty() {
        return Ok(());
    }
    let prior = std::fs::read_to_string(hook_status_path(&db, name))
        .ok()
        .and_then(|raw| toml::from_str::<HookStatus>(&raw).ok());
    let label = prior
        .filter(|s| s.version == entry.version_full())
        .map(|s| s.label)
        .filter(|s| s == "post_install" || s == "post_upgrade")
        .unwrap_or_else(|| "post_install".to_string());
    run_tracked_hook(&db, &entry, &label, &hooks.post_install, root)
}

pub(crate) fn run_tracked_hook(db: &Db, entry: &Entry, label: &str, script: &str, root: &Path) -> Result<()> {
    let mut state = HookStatus {
        version: entry.version_full(),
        label: label.to_string(),
        status: "pending".to_string(),
    };
    save_hook_status(db, &entry.name, &state)
        .with_context(|| format!("could not mark {label} hook pending for {}", entry.name))?;
    match run_hook(label, script, root) {
        Ok(()) => {
            state.status = "completed".to_string();
            save_hook_status(db, &entry.name, &state)
                .with_context(|| format!("could not mark {label} hook complete for {}", entry.name))?;
            Ok(())
        }
        Err(error) => {
            let failure_status = if crate::interrupt::is_interrupted() {
                "interrupted"
            } else {
                "failed"
            };
            state.status = failure_status.to_string();
            if let Err(status_error) = save_hook_status(db, &entry.name, &state) {
                return Err(error.context(format!(
                    "also failed to persist hook failure state: {status_error:#}"
                )));
            }
            Err(error)
        }
    }
}

/// Persist a package's hooks next to its db entry (needed at remove time).
pub(crate) fn save_hooks(db: &Db, name: &str, hooks: &archive::Hooks) -> Result<()> {
    let p = hooks_path(db, name);
    if hooks.is_empty() {
        let _ = std::fs::remove_file(&p);
        return Ok(());
    }
    let tmp = p.with_extension("tmp");
    std::fs::write(&tmp, toml::to_string(hooks)?)?;
    std::fs::rename(&tmp, &p)?;
    Ok(())
}

pub(crate) fn load_hooks(db: &Db, name: &str) -> archive::Hooks {
    std::fs::read_to_string(hooks_path(db, name))
        .ok()
        .and_then(|s| toml::from_str(&s).ok())
        .unwrap_or_default()
}

/// Run every distinct installed-package post-transaction hook once, after a
/// package-changing Nawa command has released its transaction lock. Package
/// triggers are distribution-neutral: a package can use them to rebuild
/// shared system databases without making every other recipe repeat the same
/// post-install command.
///
/// The exclusive lock prevents another Nawa process from removing a trigger
/// provider while its script is running. Hooks execute in the target root
/// using the same live-system/chroot rules as ordinary package hooks.
pub(crate) fn run_post_transaction_hooks(root: &Path) -> Result<()> {
    let _lock = crate::lock::acquire(root, crate::lock::Mode::Exclusive)?;
    let db = Db::new(root);
    let mut scripts = std::collections::BTreeMap::<String, String>::new();

    for entry in db.installed()? {
        let script = load_hooks(&db, &entry.name).post_transaction;
        if !script.trim().is_empty() {
            scripts.entry(script).or_insert(entry.name);
        }
    }

    let mut failures = Vec::new();
    for (script, package) in scripts {
        if let Err(error) = run_hook("post_transaction", &script, root) {
            failures.push(format!("{package}: {error:#}"));
        }
    }

    if failures.is_empty() {
        Ok(())
    } else {
        Err(anyhow::anyhow!(
            "one or more post-transaction hooks failed: {}",
            failures.join("; ")
        ))
    }
}

/// Does `path` exist as seen from inside a chroot at `root`? Symlinks are
/// resolved relative to `root` (an absolute target like `/usr/bin` means
/// `root/usr/bin`, not the host's), unlike `root.join(path).exists()`, which
/// follows absolute links out of the root and can report a shell that the
/// chroot does not actually have. `resolve_in_root` returns the real host
/// path of the final target (what to open to read it), `None` if any link
/// in the chain is dangling.
pub(crate) fn resolve_in_root(root: &Path, path: &str) -> Option<PathBuf> {
    let mut cur = root.to_path_buf();
    let mut todo: Vec<std::ffi::OsString> = Path::new(path)
        .components()
        .rev()
        .filter_map(|c| match c {
            std::path::Component::Normal(n) => Some(n.to_os_string()),
            _ => None,
        })
        .collect();
    let mut hops = 0;
    while let Some(part) = todo.pop() {
        cur.push(&part);
        match std::fs::symlink_metadata(&cur) {
            Err(_) => return None,
            Ok(md) if md.file_type().is_symlink() => {
                hops += 1;
                if hops > 40 {
                    return None;
                }
                let target = match std::fs::read_link(&cur) {
                    Ok(t) => t,
                    Err(_) => return None,
                };
                cur.pop();
                if target.is_absolute() {
                    cur = root.to_path_buf();
                }
                for c in target.components().rev() {
                    match c {
                        std::path::Component::Normal(n) => todo.push(n.to_os_string()),
                        std::path::Component::ParentDir => todo.push("..".into()),
                        _ => {}
                    }
                }
            }
            Ok(_) => {
                if part == ".." {
                    cur.pop();
                    cur.pop();
                }
            }
        }
    }
    Some(cur)
}

/// [`resolve_in_root`] as a yes/no question.
pub(crate) fn exists_in_root(root: &Path, path: &str) -> bool {
    resolve_in_root(root, path).is_some()
}

/// Hooks run either on the live system (`--root /`) or, when nawa runs as
/// root and the target has a shell, inside a chroot of the target. Anything
/// else is skipped: executing a package's script on the host while
/// installing into another root would run it on the wrong filesystem.
/// (No /proc or /dev is mounted in the chroot.)
/// A hook that exited non-zero. When the user pressed Ctrl+C the hook was
/// almost certainly killed with us (same terminal process group): say so,
/// because by then the package itself is already completely installed.
pub(crate) fn hook_failed(label: &str) -> anyhow::Error {
    if interrupt::is_interrupted() {
        anyhow::anyhow!(
            "{label} hook interrupted - the package files are installed and registered, \
             but the hook did not finish; install the package again to re-run it"
        )
    } else {
        anyhow::anyhow!("{label} hook failed")
    }
}

pub(crate) fn run_hook(label: &str, script: &str, root: &Path) -> Result<()> {
    if script.trim().is_empty() {
        return Ok(());
    }
    let live = std::fs::canonicalize(root)
        .map(|p| p == Path::new("/"))
        .unwrap_or(false);
    let status = if live {
        crate::nawa_info!("running {label} hook");
        // Installing glibc over a seed with a different glibc swaps the
        // loader (/usr/lib64/ld-linux-x86-64.so.2) BEFORE /etc/ld.so.cache is
        // regenerated, so the hook's own bash would run on the new loader with
        // the OLD libc from the cache ("__vdso_gettimeofday: invalid mode for
        // dlopen()"). Put /usr/lib first so loader and libc always match.
        let ld_path = match std::env::var_os("LD_LIBRARY_PATH") {
            Some(v) if !v.is_empty() => {
                let mut p = std::ffi::OsString::from("/usr/lib:");
                p.push(v);
                p
            }
            _ => std::ffi::OsString::from("/usr/lib"),
        };
        Command::new("bash")
            .args(["-e", "-c", script])
            .env("LD_LIBRARY_PATH", ld_path)
            .env("NAWA_ROOT", root)
            .env(crate::lock::HELD_ENV, "1") // we hold the db lock; nested nawa must not wait for it
            .status()?
    } else {
        let shell = ["/bin/bash", "/bin/sh"]
            .into_iter()
            .find(|sh| exists_in_root(root, sh));
        match shell {
            Some(sh) if crate::diag::is_root() => {
                crate::nawa_info!("running {label} hook in chroot {}", root.display());
                let status = chroot_command(root, sh, script)?;
                if !status.success() {
                    return Err(hook_failed(label));
                }
                return Ok(());
            }
            _ => {
                crate::nawa_note!("skipping {label} hook (--root is not / and no chroot possible)");
                return Ok(());
            }
        }
    };
    if !status.success() {
        return Err(hook_failed(label));
    }
    Ok(())
}

/// Run `sh -e -c script` inside a chroot of `root` using chroot(2) + chdir(2)
/// in the child's pre-exec hook — no dependence on the external coreutils
/// `chroot` binary (which is exactly what gets upgraded/installed by the
/// hooks this runs). (No /proc or /dev is mounted, same as before.)
pub(crate) fn chroot_command(root: &Path, sh: &str, script: &str) -> Result<std::process::ExitStatus> {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::process::CommandExt;
    let root_c = std::ffi::CString::new(root.as_os_str().as_bytes())
        .map_err(|_| anyhow::anyhow!("chroot path contains NUL: {}", root.display()))?;
    unsafe {
        Command::new(sh)
            .args(["-e", "-c", script])
            .env("NAWA_ROOT", "/")
            .env(crate::lock::HELD_ENV, "1") // we hold the db lock; nested nawa must not wait
            .pre_exec(move || {
                rustix::process::chroot(&root_c).map_err(std::io::Error::from)?;
                rustix::process::chdir(c"/").map_err(std::io::Error::from)?;
                Ok(())
            })
            .status()
            .context("spawn chrooted hook")
    }
}

