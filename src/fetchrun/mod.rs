//! Ecosystem fetch runner (docs/design/fetch-phase.md §7, §8, §11, §12).
//!
//! For a recipe with `[fetch.go]` / `[fetch.cargo]` this module:
//!
//! 1. plans the fetch round the *downloaded source* (preconditions of §14C:
//!    for Go `go.mod`/`go.sum` exist and no `go.work`/`go.work.sum` anywhere
//!    from the fetch directory up to the workspace root; for Cargo
//!    `Cargo.toml`/`Cargo.lock` exist and the source-tree Cargo config obeys
//!    the §8 v1 rules), detecting vendored layouts (`vendor/modules.txt`,
//!    a `crates-io` replacement by an in-tree `directory` source),
//! 2. computes the content-addressed ecosystem key over the backend's §11
//!    field set (Go: `go.mod`+`go.sum` bytes; Cargo: `Cargo.lock` bytes plus
//!    the `cargo --version` string) and maintains
//!    `<srccache>/fetch/<eco>/<key>/` entries: a hold-open `flock` on
//!    `<key>.lock` serialises concurrent builds, the entry is populated in a
//!    sibling 0777 temporary directory inside a dedicated sandbox profile,
//!    validated (`go mod verify`; `cargo fetch --locked` is its own
//!    validation), marked with a completion marker written last and
//!    published by an atomic rename,
//! 3. copies the published entry into the per-build view `<work>/fetch/<eco>`
//!    (never hard-linked: Cargo may write `.package-cache` even offline, §8)
//!    and returns the build-time environment additions that the whitelist
//!    joins for this build:
//!    * Go (§7): GOMODCACHE, GOPROXY=off, GOFLAGS, GOTOOLCHAIN, GOWORK,
//!      GOCACHE,
//!    * Cargo (§8): CARGO_HOME, CARGO_NET_OFFLINE=true.
//!
//! Vendored projects skip the fetch entirely and build as today
//! (`-mod=vendor`, the in-tree directory source).
//!
//! Environment knobs (all documented in §7/§8/§12):
//!
//! * `NAWA_GOPROXY` — Go module proxy override; `https://` or `file://`
//!   only. A `file://` proxy disables the checksum database (`GOSUMDB=off`),
//!   the hermetic-test clause of the design. A `file://` proxy keeps
//!   resolving through the *filesystem*, so it only reaches a sandboxed
//!   fetch when a test arranges the proxy path inside the sandbox; hermetic
//!   integration tests use `--isolation env`, where the fetch runs as a
//!   plain command. Cargo has no such override: in v1 the source-tree
//!   *registry* is fixed by the scan rules of §8 (redirects are errors), so
//!   a hermetic real `cargo fetch` exercises git dependencies instead.
//! * `NAWA_FETCH_TIMEOUT` — wall clock for one fetch run in seconds
//!   (default 1800). Enforced by the sandbox (it kills the PID namespace)
//!   and polled for plain `none`/`env` runs as a second net.
//! * `NAWA_FETCH_PATH` — **test-only**: prepends a PATH element for the
//!   fetch run. The whitelist PATH is `/usr/bin:/usr/sbin`, but a dev host
//!   keeps its toolchain elsewhere (e.g. `/usr/local/go/bin`,
//!   `~/.cargo/bin`); production toolchains are packages and land in
//!   `/usr/bin`.
//!
//! None of these reach build sections: they are fetch-run-only values.

#[cfg(test)]
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
#[cfg(test)]
use std::path::PathBuf;
#[cfg(test)]
use std::os::unix::process::CommandExt;
#[cfg(test)]
use std::process::Stdio;
#[cfg(test)]
use std::time::Instant;
#[cfg(test)]
use std::time::Duration;

use anyhow::{bail, Context, Result};
#[allow(unused_imports)]
use sha2::{Digest, Sha256};

use crate::buildenv::BuildOpts;
use crate::recipe::Recipe;

use self::cargo::isolated_cargo_paths;


/// How many output lines a failed run quotes.
pub(crate) const FAIL_TAIL_LINES: usize = 30;


/// Fetch-schema version of the Go backend (§10.3): appended to the build id
/// as `fetch-go=<v>` whenever `[fetch.go]` exists, so a change in *how* Nawa
/// fetches invalidates old `--use-cache` sidecars. The lockfile bytes are
/// deliberately NOT an input of the id (§10.3) — the recipe's pinned source
/// digests cover them.
pub(crate) const FETCH_GO_SCHEMA_VERSION: &str = "1";

/// Same for `[fetch.cargo]` → `fetch-cargo=<v>`.
pub(crate) const FETCH_CARGO_SCHEMA_VERSION: &str = "1";


/// The versioned, unambiguous first field of every ecosystem key (§11).
pub(crate) const FETCH_CACHE_ENCODING: &[u8] = b"nawa-fetch-v1";

/// The completion marker written LAST into a cache entry (§11): a directory
/// without it is never a hit, so an interrupted populate can never be read.
pub(crate) const MARKER: &str = "nawa-complete";

/// §12 fetch profile: 30 s would kill `go`; the timeout is the bound.
pub(crate) const DEFAULT_TIMEOUT_SECS: u64 = 1800;

/// Everything `build()` hands to the runner once. Paths are host paths.
pub(crate) struct FetchRun<'a> {
    pub rec: &'a Recipe,
    pub opts: &'a BuildOpts,
    /// The canonical per-build workspace `<builddir>/<name>-<version>-<rel>`.
    pub work: &'a Path,
    /// The workspace root `<work>/src` on the host.
    pub src: &'a Path,
    /// The resolved source cache root (`NAWA_SRCCACHE` or
    /// `<builddir>/distfiles`) — the same value source staging used.
    pub srccache: &'a Path,
    /// Whether build sections run inside the sandbox (`sandbox`/`closure`).
    pub sandboxed: bool,
    /// The build base *inside* the sandbox (pinned `/build`, §4.2); empty
    /// when `!sandboxed`.
    pub build_inside: &'a str,
    /// The HOME value the build whitelist carries (`<base>/home` sandboxed,
    /// `buildenv::SANDBOX_HOME` at `env`): GOCACHE lives under it, so the
    /// Go build cache stays inside the per-build scratch (§7).
    pub whitelist_home: &'a str,
    /// The assembled closure root at `closure` isolation — the fetch
    /// sandbox mounts the same tree (the toolchain lives in it).
    pub assembled: Option<&'a crate::closure::AssembledRoot>,
}


/// Run the ecosystem fetch for one recipe (§7, §8). Returns the build-time
/// environment entries to append to the whitelist (already phrased in the
/// frame the build sees: in-sandbox paths when `sandboxed`). Both tables may
/// coexist; the returned additions concatenate in table order. An empty
/// return means "no backend declared" — the caller checks first.
pub(crate) fn run(r: &FetchRun<'_>) -> Result<Vec<(String, String)>> {
    let mut env = Vec::new();
    if let Some(spec) = r.rec.fetch.go.as_ref() {
        env.extend(run_go(r, spec)?);
    }
    if let Some(spec) = r.rec.fetch.cargo.as_ref() {
        env.extend(run_cargo(r, spec)?);
    }
    Ok(env)
}


/// Toolchain identity used by the build cache. Closure builds already hash
/// the exact toolchain package archives in their dependency closure; other
/// isolation modes must pin the executable versions found through the exact
/// PATH used by the fetch runner.
pub(crate) fn build_toolchain_identity(
    rec: &Recipe,
    isolation: crate::buildenv::Isolation,
) -> Result<Vec<(&'static str, String)>> {
    if isolation == crate::buildenv::Isolation::Closure {
        return Ok(Vec::new());
    }
    let mut identity = Vec::new();
    for (enabled, name, args, label) in [
        (rec.fetch.go.is_some(), "go", vec!["version"], "go"),
        (rec.fetch.cargo.is_some(), "cargo", vec!["--version"], "cargo"),
    ] {
        if !enabled {
            continue;
        }
        let output = if label == "cargo" {
            // Cargo can read config from cwd ancestors even for a version
            // probe. Reuse the same isolated cwd/CARGO_HOME policy as fetch.
            let probe_root = std::env::temp_dir().join(format!(
                "nawa-toolchain-id-{}-{}",
                std::process::id(),
                crate::unique::next()
            ));
            std::fs::create_dir(&probe_root)
                .with_context(|| format!("create toolchain identity scratch {}", probe_root.display()))?;
            let (cargo_home, cwd) = match isolated_cargo_paths(&probe_root) {
                Ok(paths) => paths,
                Err(e) => {
                    let _ = std::fs::remove_dir_all(&probe_root);
                    return Err(e);
                }
            };
            let result = std::process::Command::new(name)
                .args(args)
                .current_dir(&cwd)
                .env_clear()
                .env("PATH", fetch_path())
                .env("CARGO_HOME", &cargo_home)
                .output()
                .with_context(|| format!("query {name} version using the fetch PATH"));
            let _ = std::fs::remove_dir_all(&cwd);
            let _ = std::fs::remove_dir_all(&probe_root);
            result?
        } else {
            // Go does not consume Cargo config; use an empty cwd without
            // making unrelated Cargo configuration a prerequisite.
            let cwd = std::env::temp_dir().join(format!(
                "nawa-go-version-{}-{}",
                std::process::id(),
                crate::unique::next()
            ));
            std::fs::create_dir(&cwd)
                .with_context(|| format!("create Go version probe cwd {}", cwd.display()))?;
            let result = std::process::Command::new(name)
                .args(args)
                .current_dir(&cwd)
                .env_clear()
                .env("PATH", fetch_path())
                .output()
                .with_context(|| format!("query {name} version using the fetch PATH"));
            let _ = std::fs::remove_dir_all(&cwd);
            result?
        };
        if !output.status.success() {
            bail!(
                "cannot determine {name} toolchain identity: {name} version exited with {}",
                output.status
            );
        }
        let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if version.is_empty() {
            bail!("{name} version command returned an empty toolchain identity");
        }
        identity.push((label, version));
    }
    Ok(identity)
}


pub mod cargo;
pub mod common;
pub mod go;

#[allow(unused_imports)]
pub(crate) use cargo::{plan_cargo, run_cargo};
#[allow(unused_imports)]
pub(crate) use common::{
    cleanup_failed_populate, copy_into_view, ecosystem_key, eco_cache_dir, entry_complete,
    fetch_path, fetch_sandbox_spec, publish_entry, resolver_target_inside, stream_and_wait,
};
#[allow(unused_imports)]
pub(crate) use go::{plan_go, run_go};

#[cfg(test)]
mod timeout_tests {
    use super::*;
    
    #[test]
    fn timeout_kills_descendant_that_keeps_stdout_open_after_shell_exit() {
        let pid_file = std::env::temp_dir().join(format!(
            "nawa-fetch-descendant-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&pid_file);
        let script = "sleep 30 & echo $! > \"$1\"; exit 0";
        let mut child = std::process::Command::new("bash")
            .args(["-c", script, "nawa-fetch-timeout-test"])
            .arg(&pid_file)
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn fetch process group");

        let started = Instant::now();
        let err = stream_and_wait(&mut child, None, 1, "timeout-test", true)
            .expect_err("the inherited stdout pipe must be bounded by the timeout");
        assert!(err.to_string().contains("NAWA_FETCH_TIMEOUT"), "{err}");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "reader must not wait indefinitely for the descendant"
        );

        let pid: i32 = std::fs::read_to_string(&pid_file)
            .expect("descendant pid file")
            .trim()
            .parse()
            .expect("valid descendant pid");
        let mut terminated = false;
        for _ in 0..40 {
            let alive = unsafe { libc::kill(pid, 0) == 0 };
            let state = std::fs::read_to_string(format!("/proc/{pid}/stat"))
                .ok()
                .and_then(|s| s.split_whitespace().nth(2).map(str::to_string));
            if !alive || state.as_deref() == Some("Z") {
                terminated = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        let _ = std::fs::remove_file(&pid_file);
        assert!(terminated, "the background descendant {pid} survived timeout");
    }
}


#[cfg(test)]
mod resolver_path_tests {
    use super::*;

    fn scratch() -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "nawa-resolver-path-{}-{}",
            std::process::id(),
            crate::unique::next()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("etc")).unwrap();
        root
    }

    #[test]
    fn resolver_symlink_into_run_resolves_even_when_target_is_dangling() {
        let root = scratch();
        std::os::unix::fs::symlink(
            "/run/systemd/resolve/stub-resolv.conf",
            root.join("etc/resolv.conf"),
        )
        .unwrap();
        let inside = resolver_target_inside(&root, "resolv.conf").unwrap();
        assert_eq!(inside, PathBuf::from("/run/systemd/resolve/stub-resolv.conf"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn resolver_relative_symlink_is_normalized_inside_root() {
        let root = scratch();
        std::os::unix::fs::symlink("../run/resolv.conf", root.join("etc/resolv.conf"))
            .unwrap();
        let inside = resolver_target_inside(&root, "resolv.conf").unwrap();
        assert_eq!(inside, PathBuf::from("/run/resolv.conf"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn direct_resolver_file_keeps_etc_destination() {
        let root = scratch();
        std::fs::write(root.join("etc/resolv.conf"), b"nameserver 127.0.0.1\n").unwrap();
        let inside = resolver_target_inside(&root, "resolv.conf").unwrap();
        assert_eq!(inside, PathBuf::from("/etc/resolv.conf"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn resolver_symlink_chain_is_resolved_inside_the_supplied_root() {
        let root = scratch();
        std::fs::create_dir_all(root.join("run/systemd/resolve")).unwrap();
        std::fs::write(root.join("run/systemd/resolve/resolv.conf"), b"nameserver 192.0.2.53\\n").unwrap();
        std::os::unix::fs::symlink("/run/systemd/resolve/resolv.conf", root.join("etc/resolv.conf")).unwrap();
        let inside = resolver_target_inside(&root, "resolv.conf").unwrap();
        assert_eq!(inside, PathBuf::from("/run/systemd/resolve/resolv.conf"));
        assert!(root.join(inside.strip_prefix("/").unwrap()).is_file());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn resolver_symlink_loop_is_rejected_instead_of_silently_accepted() {
        let root = scratch();
        std::os::unix::fs::symlink("hosts", root.join("etc/resolv.conf")).unwrap();
        std::os::unix::fs::symlink("resolv.conf", root.join("etc/hosts")).unwrap();
        let error = resolver_target_inside(&root, "resolv.conf").expect_err("symlink loop must fail");
        assert!(error.to_string().contains("exceeds 40 links"), "{error}");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn resolver_symlink_cannot_escape_root_with_parent_components() {
        let root = scratch();
        std::os::unix::fs::symlink("../../../../etc/passwd", root.join("etc/resolv.conf")).unwrap();
        let error = resolver_target_inside(&root, "resolv.conf").expect_err("must remain inside root");
        assert!(error.to_string().contains("escapes the sandbox root"), "{error}");
        let _ = std::fs::remove_dir_all(root);
    }
}

#[cfg(test)]
mod publish_tests {
    use super::*;

    fn mode_of(p: &Path) -> u32 {
        std::fs::symlink_metadata(p).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn publication_and_the_build_view_keep_the_executable_bit() {
        let base = std::env::temp_dir().join(format!("nawa-publish-exec-{}", std::process::id()));
        cleanup_failed_populate(&base);
        let temp = base.join("temp");
        std::fs::create_dir_all(temp.join("sub")).unwrap();
        std::fs::write(temp.join("sub/run.sh"), b"#!/bin/sh\n").unwrap();
        std::fs::set_permissions(temp.join("sub/run.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(temp.join("plain.txt"), b"data").unwrap();
        std::fs::set_permissions(temp.join("plain.txt"), std::fs::Permissions::from_mode(0o644)).unwrap();

        let entry = base.join("entry");
        publish_entry(&temp, &entry).unwrap();
        assert_eq!(mode_of(&entry.join("sub/run.sh")), 0o555, "executable stays executable");
        assert_eq!(mode_of(&entry.join("plain.txt")), 0o444);
        assert_eq!(mode_of(&entry.join("sub")), 0o555);

        let view = base.join("view");
        std::fs::create_dir_all(&view).unwrap();
        copy_into_view(&entry, &view).unwrap();
        assert_eq!(mode_of(&view.join("sub/run.sh")), 0o755, "the view restores a writable executable");
        assert_eq!(mode_of(&view.join("plain.txt")), 0o644);

        cleanup_failed_populate(&base);
    }
}
