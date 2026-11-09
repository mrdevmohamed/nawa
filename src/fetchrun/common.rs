//! Shared cache, sandbox, and process helpers.

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::{BufRead, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, ExitStatus, Stdio};
use std::time::{Duration, Instant};
use walkdir::WalkDir;

use crate::sandbox::spec::ExtraBind;
use crate::sandbox::{BuildPaths, RootSpec, SandboxSpec};
use super::{FETCH_CACHE_ENCODING, MARKER, FAIL_TAIL_LINES, DEFAULT_TIMEOUT_SECS, FetchRun};

/// sha256 over the length-prefixed key fields (§11): `nawa-fetch-v1`, the
/// ecosystem, then the backend's field set. Length-prefixing makes the
/// encoding unambiguous — no separator can be forged by payload bytes.
pub(crate) fn ecosystem_key(eco: &str, fields: &[&[u8]]) -> Result<String> {
    let mut hasher = Sha256::new();
    // fixed header first: the encoding version, then the ecosystem
    for field in [FETCH_CACHE_ENCODING, eco.as_bytes()] {
        hasher.update((field.len() as u64).to_le_bytes());
        hasher.update(field);
    }
    for field in fields {
        hasher.update((field.len() as u64).to_le_bytes());
        hasher.update(field);
    }
    Ok(hex::encode(hasher.finalize()))
}


/// `<srccache>/fetch/<eco>` — the ecosystem cache root (§11).
pub(crate) fn eco_cache_dir(r: &FetchRun, eco: &str) -> PathBuf {
    r.srccache.join("fetch").join(eco)
}


/// A cache entry is a hit only with its completion marker (§11).
pub(crate) fn entry_complete(entry: &Path) -> bool {
    entry.join(MARKER).is_file()
}



/// §12: `NAWA_FETCH_TIMEOUT`, default 1800. Invalid values fail loud.
pub(crate) fn fetch_timeout_secs() -> Result<u64> {
    match std::env::var("NAWA_FETCH_TIMEOUT") {
        Ok(v) if !v.trim().is_empty() => v
            .trim()
            .parse::<u64>()
            .with_context(|| format!("NAWA_FETCH_TIMEOUT \"{v}\" is not a second count")),
        _ => Ok(DEFAULT_TIMEOUT_SECS),
    }
}


/// The fetch PATH (§7): the whitelist PATH; `NAWA_FETCH_PATH` can prepend
/// a test-only element on dev hosts whose toolchain lives elsewhere.
pub(crate) fn fetch_path() -> String {
    match std::env::var("NAWA_FETCH_PATH") {
        Ok(p) if !p.trim().is_empty() => format!("{}:/usr/bin:/usr/sbin", p.trim()),
        _ => "/usr/bin:/usr/sbin".to_string(),
    }
}


/// Spawn one fetch command in the right flavor: sandboxed (`sandbox`/
/// `closure`) through the §12 fetch spec — configured once, via the same
/// sandbox profile as the populate run and for `cargo --version` alike —
/// or a plain cleared-environment `bash` at `none`/`env` (§12: no
/// isolation, never described as network-isolated).
///
/// * `script` — the shell body (starts with `exec 2>&1`).
/// * `env` — the run's allowlist, already in the flavor's frame.
/// * `workdir` — the fetch directory *in the host frame*; the sandbox sees
///   `<base>/src/<rel>`.
/// * `out_host` — when set and sandboxed, is bound writable at
///   `<base>/fetch-out` (the populate dir, a version probe's scratch…).
pub(crate) fn spawn_fetch_child(
    r: &FetchRun,
    script: &str,
    env: Vec<(String, String)>,
    workdir: &Path,
    out_host: Option<&Path>,
    timeout_secs: u64,
) -> Result<Child> {
    if r.sandboxed {
        let env: BTreeMap<String, String> = env.into_iter().collect();
        let spec = fetch_sandbox_spec(
            r,
            env,
            script,
            workdir,
            out_host.unwrap_or(r.work),
            timeout_secs,
        )?;
        let spec_path = r.work.join("fetch-sandbox-spec.toml");
        std::fs::write(&spec_path, toml::to_string_pretty(&spec)?)
            .with_context(|| format!("write {}", spec_path.display()))?;
        let exe = std::env::current_exe().context("locate the nawa binary")?;
        let (child, sock) = crate::sandbox::spawn_init(&exe, &spec_path)
            .context("spawn nawa sandbox-init for the fetch run")?;
        // install the ranged uid map when we are root, then release the child
        crate::sandbox::parent_sync(&child, sock);
        Ok(child)
    } else {
        std::process::Command::new("bash")
            .args(["-e", "-c", script])
            .current_dir(workdir)
            .env_clear()
            .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .context("failed to spawn the fetch run")
    }
}


/// The sandbox spec of one fetch run (§12). Clones the shape of the build
/// spec (same staging binds, same mask) and then hardens it: strict
/// read-only root, read-only src, a writable `/tmp` + home + fetch-out,
/// real /etc/resolv.conf + /etc/hosts copies bound read-only, the fetch
/// profile limits, network always on.
pub(crate) fn fetch_sandbox_spec(
    r: &FetchRun,
    env: BTreeMap<String, String>,
    script: &str,
    workdir_host: &Path,
    out_host: &Path,
    timeout_secs: u64,
) -> Result<SandboxSpec> {
    let base = r.build_inside;
    let scratch = r.work.join("sb");
    let home = scratch.join("home");
    std::fs::create_dir_all(&home)
        .with_context(|| format!("create the fetch scratch home {}", home.display()))?;
    // The fetch identity is dropped to uid/gid 65534 in sandbox mode.
    std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o777))?;

    // host paths to hide in live mode (§4.2), as the build spec does
    let mut mask: Vec<String> = [
        "/mydistro",
        "/opt/seed",
        "/root",
        "/home",
        "/mnt",
        "/var/lib/nawa",
        "/var/cache/nawa",
        "/run",
        // A host may expose binfmt_misc as a nested mount under /proc. The
        // fetch sandbox mounts a fresh procfs; hide this non-fetch interface
        // so strict_readonly_root does not try to remount the now-covered host
        // mount through the fresh procfs path (EINVAL on GitHub runners).
        "/proc/sys/fs/binfmt_misc",
    ]
    .into_iter()
    .map(String::from)
    .collect();
    if let Some(bb) = r.work.parent() {
        let bbs = bb.to_string_lossy().into_owned();
        if bbs != r.build_inside {
            mask.push(bbs);
        }
    }

    // Source-tree commands use the read-only source bind. Cargo fetch uses
    // a dedicated empty cwd outside the source tree and outside CARGO_HOME,
    // so no recipe-parent or user Cargo config can affect registry selection.
    let workdir = if let Ok(rel) = workdir_host.strip_prefix(out_host) {
        format!("{}/fetch-out/{}", base.trim_end_matches('/'), rel.display())
            .trim_end_matches('/')
            .to_string()
    } else if let Ok(rel) = workdir_host.strip_prefix(r.src) {
        format!("{}/src/{}", base.trim_end_matches('/'), rel.display())
            .trim_end_matches('/')
            .to_string()
    } else {
        format!("{}/fetch-cwd", base.trim_end_matches('/'))
    };

    let mut extra_binds = vec![ExtraBind {
        // the temporary entry (or the probe scratch) being written;
        // published after the run for populate outputs
        host: out_host.to_string_lossy().into_owned(),
        inside: format!("{}/fetch-out", base.trim_end_matches('/')),
        readonly: false,
        kind: "dir".into(),
    }];
    if !workdir_host.starts_with(r.src) && !workdir_host.starts_with(out_host) {
        extra_binds.push(ExtraBind {
            host: workdir_host.to_string_lossy().into_owned(),
            inside: format!("{}/fetch-cwd", base.trim_end_matches('/')),
            readonly: false,
            kind: "dir".into(),
        });
    }
    extra_binds.extend(resolver_binds(r)?);

    Ok(SandboxSpec {
        version: 1,
        root: if r.opts.isolation == crate::buildenv::Isolation::Closure {
            RootSpec {
                mode: "dir".into(),
                dir: r.work.join("buildroot").to_string_lossy().into_owned(),
                layers: r
                    .assembled
                    .map(|a| a.layer_dirs.iter().map(|p| p.to_string_lossy().into_owned()).collect())
                    .unwrap_or_default(),
            }
        } else {
            RootSpec::default()
        },
        build: BuildPaths {
            src_host: r.src.to_string_lossy().into_owned(),
            pkg_host: r.work.join("pkg").to_string_lossy().into_owned(),
            home_host: home.to_string_lossy().into_owned(),
            scratch: scratch.to_string_lossy().into_owned(),
        },
        env,
        script: script.to_string(),
        timeout_secs,
        network: true,
        sysfs: false,
        allow_degraded: r.opts.allow_degraded,
        mask,
        build_inside: base.to_string(),
        workdir,
        // §12: drop privileges for the fetch process when we are root
        run_uid: privileged().then_some(65534),
        run_gid: privileged().then_some(65534),
        memory_limit_bytes: 0,     // RLIMIT_AS breaks Go; the timeout is the bound
        process_limit: 4096,       // threads count
        file_size_limit_bytes: 0,  // a module may exceed any small cap
        open_files_limit: 4096,    // parallel downloads
        readonly_paths: vec![format!("{}/src", base.trim_end_matches('/'))],
        extra_binds,
        strict_readonly_root: true,
        // /tmp (fresh tmpfs), the home bind and the populate dir stay
        // writable; strict mode remounts everything else read-only
        writable_paths: vec![
            "/tmp".to_string(),
            format!("{}/home", base.trim_end_matches('/')),
            format!("{}/fetch-out", base.trim_end_matches('/')),
            format!("{}/fetch-cwd", base.trim_end_matches('/')),
        ],
    })
}


/// §12: Nawa follows the symlink chain of the host's `/etc/resolv.conf`,
/// copies its content together with `/etc/hosts` into `<work>/fetch/etc/`,
/// then binds the copies read-only as `file` binds — binding the host path
/// directly is wrong twice over (no `/etc/resolv.conf` in an assembled
/// closure root; systemd-resolved's path is a symlink into the masked
/// `/run`). Missing files are skipped with a warning.
pub(crate) fn normalize_resolver_path(path: &Path) -> Result<PathBuf> {
    let mut out = PathBuf::from("/");
    for part in path.components() {
        match part {
            std::path::Component::RootDir | std::path::Component::CurDir => {},
            std::path::Component::ParentDir => {
                if !out.pop() { bail!("resolver symlink escapes the sandbox root"); }
            }
            std::path::Component::Normal(name) => out.push(name),
            _ => bail!("unsupported resolver path component in {}", path.display()),
        }
    }
    Ok(out)
}


/// Resolve the final symlink chain using the sandbox root's view of /etc.
/// Absolute symlink targets remain absolute inside that root. A missing final
/// target is allowed: bind_at creates it after /run has been masked.
pub(crate) fn resolver_target_inside(root: &Path, name: &str) -> Result<PathBuf> {
    let mut inside = PathBuf::from("/etc").join(name);
    let mut resolved = false;
    for _ in 0..40 {
        let host_path = if root == Path::new("/") {
            inside.clone()
        } else {
            root.join(inside.strip_prefix("/").context("resolver path is not absolute")?)
        };
        match std::fs::symlink_metadata(&host_path) {
            Ok(md) if md.file_type().is_symlink() => {
                let target = std::fs::read_link(&host_path)
                    .with_context(|| format!("read resolver symlink {}", host_path.display()))?;
                inside = if target.is_absolute() {
                    normalize_resolver_path(&target)?
                } else {
                    normalize_resolver_path(&inside.parent().unwrap_or(Path::new("/")).join(target))?
                };
            }
            Ok(_) => { resolved = true; break; }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => { resolved = true; break; }
            Err(e) => return Err(e).with_context(|| format!("inspect resolver path {}", host_path.display())),
        }
    }
    if !resolved {
        bail!("resolver symlink chain exceeds 40 links inside {}", root.display());
    }
    if root == Path::new("/") {
        if let Ok(real) = std::fs::canonicalize(&inside) {
            inside = real;
        }
    }
    normalize_resolver_path(&inside)
}


pub(crate) fn resolver_binds(r: &FetchRun) -> Result<Vec<ExtraBind>> {
    let etc = r.work.join("fetch").join("etc");
    std::fs::create_dir_all(&etc)
        .with_context(|| format!("create {}", etc.display()))?;
    let root = if r.opts.isolation == crate::buildenv::Isolation::Closure {
        r.work.join("buildroot")
    } else {
        PathBuf::from("/")
    };
    let mut out = Vec::new();
    for name in ["resolv.conf", "hosts"] {
        let host = Path::new("/etc").join(name);
        let Ok(content) = std::fs::read(&host) else {
            crate::nawa_warn!(
                "fetch: {} is missing on the host — DNS for the fetch run may fail if the sandbox root does not provide it",
                host.display()
            );
            continue;
        };
        let copy = etc.join(name);
        std::fs::write(&copy, &content)
            .with_context(|| format!("copy {} to {}", host.display(), copy.display()))?;
        let inside = resolver_target_inside(&root, name)?;
        out.push(ExtraBind {
            host: copy.to_string_lossy().into_owned(),
            inside: inside.to_string_lossy().into_owned(),
            readonly: true,
            kind: "file".into(),
        });
    }
    Ok(out)
}


pub(crate) fn privileged() -> bool {
    crate::diag::is_root()
}


/// Publication (§11): normalise ownership (the sandbox identity may have
/// written the entry) and strip write permission (dirs 0555, files 0444,
/// symlinks untouched), then write the completion marker LAST, normalise
/// the marked root itself and rename the sibling into place atomically.
/// (Files are 0444, or 0555 when the owner-execute bit was set.)
pub(crate) fn publish_entry(temp: &Path, entry: &Path) -> Result<()> {
    let euid = rustix::process::geteuid().as_raw();
    let egid = rustix::process::getegid().as_raw();
    use std::os::unix::fs::PermissionsExt;

    let normalise = |p: &Path, is_dir: bool| -> Result<()> {
        let md = std::fs::symlink_metadata(p).with_context(|| format!("stat {}", p.display()))?;
        if md.file_type().is_symlink() {
            return Ok(());
        }
        let _ = std::os::unix::fs::chown(p, Some(euid), Some(egid));
        // read-only, but an owner-executable file stays executable (0555):
        // copy_into_view derives the build view's 0755 from this bit, and a
        // vendored script (a `-sys` crate's ./configure, say) must still run
        let executable = !is_dir && md.permissions().mode() & 0o100 != 0;
        let mode = if is_dir || executable { 0o555 } else { 0o444 };
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode))
            .with_context(|| format!("chmod {:o} {}", mode, p.display()))?;
        Ok(())
    };

    // children first, every one of them
    for e in WalkDir::new(temp).into_iter().filter_map(|e| e.ok()) {
        if e.depth() == 0 {
            continue;
        }
        // WalkDir without follow_links yields symlink entries as files;
        // normalise() ignores them like grant_source_write does
        normalise(e.path(), e.file_type().is_dir())?;
    }
    // the marker is written LAST: a reader before it either sees no entry
    // (rename pending) or an incomplete temp dir it must never hit (§11)
    let marker = temp.join(MARKER);
    std::fs::write(&marker, b"").with_context(|| format!("write {}", marker.display()))?;
    normalise(&marker, false)?;
    normalise(temp, true)?;

    // a stale unlocked leftover cannot exist under our lock, but be strict
    // about garbage from a pre-lock crash: it is never a valid entry
    if entry.exists() {
        cleanup_failed_populate(entry);
    }
    std::fs::rename(temp, entry)
        .with_context(|| format!("publish {} -> {}", temp.display(), entry.display()))?;
    Ok(())
}


/// A failed/needs-removal populate dir or stale entry: restore write
/// permission over the tree (publication strips it) so the removal can
/// actually delete children, then remove. Best effort — cleanup that
/// failed to clean is a cache-hygiene smell, not a build failure.
pub(crate) fn cleanup_failed_populate(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755));
    for e in WalkDir::new(dir).into_iter().flatten() {
        if e.file_type().is_symlink() {
            continue;
        }
        let mode = if e.file_type().is_dir() { 0o755 } else { 0o644 };
        let _ = std::fs::set_permissions(e.path(), std::fs::Permissions::from_mode(mode));
    }
    let _ = std::fs::remove_dir_all(dir);
}


/// Copy a published cache entry into the per-build view (§11: never
/// hard-linked — a write through the view must not reach the shared entry).
/// The copy is normalised writable: dirs 0755, files 0644 (a published
/// entry is 0444/0555, and `fs::copy` would carry those bits).
pub(crate) fn copy_into_view(entry: &Path, view: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    for e in WalkDir::new(entry).into_iter().filter_map(|e| e.ok()) {
        let rel = e.path().strip_prefix(entry).unwrap_or(Path::new(""));
        // the publication marker is cache metadata, not module-cache
        // content (§11): it stays in the entry, never in the build view
        if rel == Path::new(MARKER) {
            continue;
        }
        let dst = view.join(rel);
        let ft = e.file_type();
        if ft.is_dir() {
            std::fs::create_dir_all(&dst)
                .with_context(|| format!("create view dir {}", dst.display()))?;
        } else if ft.is_symlink() {
            let link = std::fs::read_link(e.path())
                .with_context(|| format!("read link {}", e.path().display()))?;
            let _ = std::os::unix::fs::symlink(&link, &dst);
        } else {
            std::fs::copy(e.path(), &dst)
                .with_context(|| format!("copy {} -> {}", e.path().display(), dst.display()))?;
            let md = std::fs::symlink_metadata(e.path())?;
            let mode = md.permissions().mode() & 0o7777;
            // the view is normalised writable (§11: the published entry is
            // 0444/0555): dirs and set executables 0755, plain files 0644
            let want = if mode & 0o111 != 0 { 0o755 } else { 0o644 };
            std::fs::set_permissions(&dst, std::fs::Permissions::from_mode(want))
                .with_context(|| format!("mode {}", dst.display()))?;
        }
    }
    Ok(())
}


/// Stream a fetch child's combined stdout (both run flavors exec
/// `exec 2>&1` first) to the console — and to `<work>/<label>.log` when a
/// log path is given (populate runs; the version probe stays silent) —
/// quoting the last lines on failure. Enforces the wall clock by polling:
/// a plain run has no supervisor, and a sandboxed run gets the same second
/// net (the sandbox itself kills the PID namespace, §12).
pub(crate) fn stream_and_wait(
    child: &mut Child,
    log_path: Option<&Path>,
    timeout_secs: u64,
    label: &str,
    kill_process_group: bool,
) -> Result<ExitStatus> {
    let mut log = match log_path {
        Some(p) => Some(std::fs::File::create(p).with_context(|| format!("create {}", p.display()))?),
        None => None,
    };

    // Keep the reader independent so a descendant retaining stdout cannot
    // make join() block beyond the fetch timeout after the shell exits.
    let stdout: ChildStdout = child.stdout.take().context("no stdout pipe")?;
    let tail = std::sync::Arc::new(std::sync::Mutex::new(
        std::collections::VecDeque::<String>::with_capacity(FAIL_TAIL_LINES + 1),
    ));
    let tail_writer = std::sync::Arc::clone(&tail);
    let (reader_done_tx, reader_done_rx) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        let mut r = std::io::BufReader::new(stdout);
        let mut buf = Vec::new();
        while r.read_until(b'\n', &mut buf).unwrap_or(0) > 0 {
            if let Some(f) = log.as_mut() { let _ = f.write_all(&buf); }
            {
                let mut out = std::io::stdout().lock();
                let _ = out.write_all(&buf);
                let _ = out.flush();
            }
            let mut t = tail_writer.lock().unwrap();
            if t.len() == FAIL_TAIL_LINES { t.pop_front(); }
            t.push_back(String::from_utf8_lossy(&buf).trim_end().to_string());
            buf.clear();
        }
        let _ = reader_done_tx.send(());
    });

    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    loop {
        if let Some(status) = child.try_wait().context("waiting for the fetch run")? {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if reader_done_rx.recv_timeout(remaining).is_err() {
                terminate_fetch_process(child, kill_process_group);
                let _ = reader.join();
                let t: Vec<String> = tail.lock().unwrap().drain(..).collect();
                bail!("{label} exceeded NAWA_FETCH_TIMEOUT ({timeout_secs}s); a descendant retained stdout and the process tree was killed\n--- last output ---\n{}", t.join("\n"));
            }
            reader.join().map_err(|_| anyhow::Error::msg("fetch log reader crashed"))?;
            let t: Vec<String> = tail.lock().unwrap().drain(..).collect();
            if !status.success() {
                bail!("{label} failed with status {status}\n--- last output ---\n{}", t.join("\n"));
            }
            return Ok(status);
        }
        if Instant::now() >= deadline {
            terminate_fetch_process(child, kill_process_group);
            let _ = reader.join();
            let t: Vec<String> = tail.lock().unwrap().drain(..).collect();
            bail!("{label} exceeded NAWA_FETCH_TIMEOUT ({timeout_secs}s) and was killed\n--- last output ---\n{}", t.join("\n"));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}


/// Kill the plain fetch process group, including Go/Cargo descendants. Sandboxed
/// runs are supervised by the PID namespace and must not signal the caller.
pub(crate) fn terminate_fetch_process(child: &mut Child, kill_process_group: bool) {
    if kill_process_group {
        unsafe { libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL); }
    } else {
        let _ = child.kill();
    }
    let _ = child.wait();
}

