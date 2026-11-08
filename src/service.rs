//! runit service management: `nawa service` inspects and toggles services in
//! the `/etc/sv` tree and the default runsvdir (`/etc/runit/runsvdir/default`).
//!
//! runit semantics used here (matching the runit-services recipe):
//!   * a service is *available* when `/etc/sv/<name>/run` exists
//!   * a service is *enabled* when `/etc/runit/runsvdir/default/<name>` is a
//!     symlink pointing at `/etc/sv/<name>` — runsvdir rescans that directory
//!     every few seconds, so enabling starts the service and disabling stops
//!     it, with no supervisor restart needed
//!   * live status is delegated to `sv status /etc/sv/<name>` when the `sv`
//!     binary is present and a runsv process is actually supervising it;
//!     otherwise the static enabled/disabled state is reported
//!
//! All paths are resolved under `--root`, so the full lifecycle can be
//! exercised against a throwaway directory or an installed-disk image.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

pub fn sv_dir(root: &Path) -> PathBuf {
    root.join("etc/sv")
}

pub fn enabled_dir(root: &Path) -> PathBuf {
    root.join("etc/runit/runsvdir/default")
}

/// Markers for services an admin disabled on purpose: `<root>/etc/runit/disabled/<n>`.
/// Packages ship their default runsvdir links, so without this a package
/// upgrade would silently re-enable a service the admin turned off.
pub fn disabled_dir(root: &Path) -> PathBuf {
    root.join("etc/runit/disabled")
}

/// True when installing `dest` would recreate a runsvdir link the admin
/// disabled: the install step then skips that one file.
pub fn keep_disabled(root: &Path, dest: &Path) -> bool {
    let Some(name) = dest.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    let ed = enabled_dir(root);
    dest.parent() == Some(ed.as_path())
        && validate_name(name).is_ok()
        && disabled_dir(root).join(name).exists()
        && dest.symlink_metadata().is_err()
}

#[derive(Debug, Clone)]
pub struct ServiceInfo {
    pub name: String,
    pub enabled: bool,
    pub has_run: bool,
}

/// Enabled = the runsvdir entry exists and is a symlink (dangling counts:
/// that is exactly what a freshly enabled service is before stage 1 mounts).
pub fn is_enabled(root: &Path, name: &str) -> Result<bool> {
    let link = enabled_dir(root).join(name);
    Ok(link
        .symlink_metadata()
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false))
}

/// All available services, sorted by name.
pub fn list(root: &Path) -> Result<Vec<ServiceInfo>> {
    let sv = sv_dir(root);
    if !sv.is_dir() {
        bail!(
            "no service tree at {} (is runit-services installed?)",
            sv.display()
        );
    }
    let mut names: Vec<String> = std::fs::read_dir(&sv)?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    names.sort();
    let mut out = Vec::with_capacity(names.len());
    for name in names {
        let has_run = sv.join(&name).join("run").exists();
        let enabled = is_enabled(root, &name)?;
        out.push(ServiceInfo { name, enabled, has_run });
    }
    Ok(out)
}

/// Service names are single path components: no separators, no `..`.
fn validate_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name != "."
        && name != ".."
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '@'));
    if !ok {
        bail!("invalid service name `{name}`");
    }
    Ok(())
}

/// Link a service into the default runsvdir. The symlink target is absolute
/// (`/etc/sv/<name>`), matching what the runit-services recipe ships and
/// resolving correctly inside the real system once booted.
pub fn enable(root: &Path, name: &str) -> Result<()> {
    validate_name(name)?;
    let src = sv_dir(root).join(name);
    if !src.is_dir() || !src.join("run").exists() {
        bail!(
            "service `{name}` has no run script under {}",
            sv_dir(root).display()
        );
    }
    let ed = enabled_dir(root);
    std::fs::create_dir_all(&ed).with_context(|| format!("create {}", ed.display()))?;
    let link = ed.join(name);
    if link.symlink_metadata().is_ok() {
        bail!("service `{name}` is already enabled");
    }
    std::os::unix::fs::symlink(format!("/etc/sv/{name}"), &link)
        .with_context(|| format!("symlink {}", link.display()))?;
    // explicit enable cancels an earlier explicit disable
    let _ = std::fs::remove_file(disabled_dir(root).join(name));
    Ok(())
}

/// Remove the runsvdir link. Refuses to touch anything that is not a
/// symlink, so an operator's real directory can never be deleted by mistake.
pub fn disable(root: &Path, name: &str) -> Result<()> {
    validate_name(name)?;
    let link = enabled_dir(root).join(name);
    let md = link
        .symlink_metadata()
        .with_context(|| format!("service `{name}` is not enabled"))?;
    if !md.file_type().is_symlink() {
        bail!("refusing to remove {}: not a symlink", link.display());
    }
    std::fs::remove_file(&link)?;
    // remember the choice so a package upgrade does not re-enable it
    let dd = disabled_dir(root);
    std::fs::create_dir_all(&dd).with_context(|| format!("create {}", dd.display()))?;
    std::fs::write(dd.join(name), b"")
        .with_context(|| format!("record disabled state of `{name}`"))?;
    Ok(())
}

/// Live control only makes sense against the running system: runsvdir of a
/// throwaway `--root` is not supervising anything.
fn require_live_root(root: &Path) -> Result<()> {
    if root != Path::new("/") {
        bail!("live service control works on the running system only (got --root {})", root.display());
    }
    Ok(())
}

/// `sv` resolves a bare name against $SVDIR (default /service), which does not
/// exist here, so it must always be given the full runsvdir path.
fn sv_path(root: &Path, name: &str) -> PathBuf {
    enabled_dir(root).join(name)
}

/// runsv has created `supervise/ok` for this service, i.e. it is supervised.
pub fn is_supervised(root: &Path, name: &str) -> bool {
    sv_path(root, name).join("supervise/ok").exists()
}

/// runsvdir only rescans every ~5 s after a link appears: wait for runsv.
fn wait_supervised(root: &Path, name: &str, secs: u64) -> bool {
    let steps = secs * 5;
    for _ in 0..=steps {
        if is_supervised(root, name) {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    false
}

/// Run `sv <action> <runsvdir>/<name>` with inherited stdio so the user sees
/// runit's own `ok: run: name: (pid N) 2s` / `timeout:` line.
fn sv_run(root: &Path, action: &str, name: &str) -> Result<()> {
    let st = Command::new("sv")
        .arg(action)
        .arg(sv_path(root, name))
        .status()
        .with_context(|| "cannot run `sv` (is the runit package installed?)")?;
    if !st.success() {
        bail!("`sv {action} {name}` failed ({st})");
    }
    Ok(())
}

fn require_enabled_supervised(root: &Path, name: &str) -> Result<()> {
    validate_name(name)?;
    require_live_root(root)?;
    if !is_enabled(root, name)? {
        bail!("service `{name}` is not enabled: run `nawa service enable {name}` first");
    }
    // just enabled: runsvdir rescans every ~5 s before runsv appears
    if !wait_supervised(root, name, 10) {
        bail!("service `{name}` is enabled but runsvdir did not start supervising it within 10 s (is runsvdir running?)");
    }
    Ok(())
}

/// Start a service now (it must be enabled).
pub fn start(root: &Path, name: &str) -> Result<()> {
    require_enabled_supervised(root, name)?;
    sv_run(root, "start", name)
}

/// Stop a service now, without disabling it: it stays down until `start`,
/// `restart` or the next boot.
pub fn stop(root: &Path, name: &str) -> Result<()> {
    require_enabled_supervised(root, name)?;
    sv_run(root, "stop", name)
}

pub fn restart(root: &Path, name: &str) -> Result<()> {
    require_enabled_supervised(root, name)?;
    sv_run(root, "restart", name)
}

/// `enable --now`: link, wait for runsv to pick it up, then wait until it runs.
pub fn enable_now(root: &Path, name: &str) -> Result<()> {
    require_live_root(root)?;
    enable(root, name)?;
    if !wait_supervised(root, name, 10) {
        bail!("`{name}` was enabled but runsvdir did not start supervising it within 10 s");
    }
    sv_run(root, "start", name)
}

/// `disable --now`: stop first (clean TERM + wait), then drop the link.
pub fn disable_now(root: &Path, name: &str) -> Result<()> {
    validate_name(name)?;
    require_live_root(root)?;
    if is_supervised(root, name) {
        sv_run(root, "stop", name)?;
    }
    disable(root, name)
}

/// Live state from `sv status` for an enabled service.
fn live_status(root: &Path, name: &str) -> String {
    if !is_supervised(root, name) {
        return "waiting for runsvdir (not supervised yet)".to_string();
    }
    match Command::new("sv").arg("status").arg(sv_path(root, name)).output() {
        Ok(o) => {
            let mut t = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if t.is_empty() {
                t = String::from_utf8_lossy(&o.stderr).trim().to_string();
            }
            if t.is_empty() { "unknown".to_string() } else { t }
        }
        Err(e) => format!("`sv` unavailable ({e})"),
    }
}

pub fn print_list(root: &Path) -> Result<()> {
    let services = list(root)?;
    if services.is_empty() {
        crate::nawa_info!("no services under {}", sv_dir(root).display());
        return Ok(());
    }
    println!("{:<24} {:<9} RUN-SCRIPT", "SERVICE", "ENABLED");
    for s in &services {
        println!(
            "{:<24} {:<9} {}",
            s.name,
            if s.enabled { "yes" } else { "no" },
            if s.has_run { "yes" } else { "no" }
        );
    }
    Ok(())
}

pub fn print_status(root: &Path, name: Option<&str>) -> Result<()> {
    let services = list(root)?;
    let chosen: Vec<ServiceInfo> = match name {
        Some(n) => match services.iter().find(|s| s.name == n) {
            Some(s) => vec![s.clone()],
            None => bail!("service `{n}` not found under {}", sv_dir(root).display()),
        },
        None => services,
    };
    if chosen.is_empty() {
        crate::nawa_info!("no services under {}", sv_dir(root).display());
        return Ok(());
    }
    for s in &chosen {
        let state = if s.enabled { "enabled" } else { "disabled" };
        let run = if s.has_run { "run-script present" } else { "NO run script" };
        let live = if s.enabled {
            format!("live: {}", live_status(root, &s.name))
        } else {
            "not running (disabled)".to_string()
        };
        println!("{:<24} {state}, {run}, {live}", s.name);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("nawa-svc-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn mk_service(root: &Path, name: &str) {
        let dir = sv_dir(root).join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("run"), "#!/bin/sh\nexec true\n").unwrap();
    }

    #[test]
    fn enable_disable_roundtrip() {
        let root = tmpdir("rt");
        mk_service(&root, "sshd");
        mk_service(&root, "crond");

        let all = list(&root).unwrap();
        assert_eq!(all.len(), 2, "both services discovered");
        assert!(all.iter().all(|s| !s.enabled && s.has_run));

        enable(&root, "sshd").unwrap();
        let link = enabled_dir(&root).join("sshd");
        assert_eq!(
            std::fs::read_link(&link).unwrap(),
            Path::new("/etc/sv/sshd"),
            "runsvdir link must point at the /etc/sv entry"
        );
        assert!(is_enabled(&root, "sshd").unwrap());
        assert!(!is_enabled(&root, "crond").unwrap());

        // double enable is refused; unknown services are refused
        assert!(enable(&root, "sshd").is_err());
        assert!(enable(&root, "ghost").is_err());

        disable(&root, "sshd").unwrap();
        assert!(link.symlink_metadata().is_err(), "link must be gone");
        assert!(disable(&root, "sshd").is_err(), "double disable refused");

        // disabled on purpose: a package install must not recreate the link
        assert!(keep_disabled(&root, &link));
        enable(&root, "sshd").unwrap();
        assert!(!keep_disabled(&root, &link), "enable clears the marker");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn disable_refuses_non_symlinks() {
        let root = tmpdir("safe");
        mk_service(&root, "a");
        let ed = enabled_dir(&root);
        std::fs::create_dir_all(ed.join("b")).unwrap(); // real dir, not a link
        assert!(disable(&root, "b").is_err());
        assert!(ed.join("b").is_dir(), "real directory must be untouched");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn service_without_run_script_cannot_be_enabled() {
        let root = tmpdir("norun");
        std::fs::create_dir_all(sv_dir(&root).join("broken")).unwrap();
        assert!(enable(&root, "broken").is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn live_control_refuses_foreign_root_and_disabled_services() {
        let root = tmpdir("live");
        mk_service(&root, "sshd");
        // a throwaway --root is never supervised by the running runsvdir
        assert!(start(&root, "sshd").is_err());
        assert!(stop(&root, "sshd").is_err());
        assert!(restart(&root, "sshd").is_err());
        assert!(enable_now(&root, "sshd").is_err());
        assert!(!is_enabled(&root, "sshd").unwrap(), "failed --now must not enable");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn list_without_tree_fails() {
        let root = tmpdir("empty");
        assert!(list(&root).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }
}
