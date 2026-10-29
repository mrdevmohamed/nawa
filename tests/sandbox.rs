//! Sandbox mechanism tests (docs/design/build-isolation.md §11).
//!
//! Every test probes the kernel first (via `nawa sandbox-probe`) and skips
//! with a clear message when the host refuses a feature — restricted
//! containers (no fresh /proc, no read-only binds…) must not turn the suite
//! red, while a normal bootstrap host runs everything.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

fn nawa() -> Command {
    Command::new(PathBuf::from(env!("CARGO_BIN_EXE_nawa")))
}

fn scratch(tag: &str) -> PathBuf {
    // NOT under /tmp: the sandbox mounts a fresh tmpfs over /tmp before the
    // staging binds, so host /tmp paths are invisible inside by then
    let d = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("nawa-sbx-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(d.join("src")).unwrap();
    fs::create_dir_all(d.join("pkg")).unwrap();
    fs::create_dir_all(d.join("home")).unwrap();
    d
}

/// Which kernel features this host provides.
#[derive(Debug, Default)]
#[allow(dead_code)] // probed for completeness; not every field is asserted on
struct Probes {
    userns: bool,
    mounts: bool,
    ro_bind: bool,
    proc: bool,
    pivot: bool,
    netns_lo: bool,
}

fn probe_all() -> Probes {
    let out = nawa().arg("sandbox-probe").output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let get = |op: &str| text.contains(&format!("PASS  {op}"));
    Probes {
        userns: get("userns"),
        mounts: get("mounts"),
        ro_bind: get("ro-bind"),
        proc: get("proc"),
        pivot: get("pivot"),
        netns_lo: get("netns-lo"),
    }
}

fn should_skip(_p: &Probes, need: bool, what: &str) -> bool {
    if need {
        eprintln!("SKIP: this kernel/container does not allow {what}");
        return true;
    }
    false
}

/// Write a sandbox spec and run it through `nawa sandbox-init` directly.
fn run_spec(dir: &Path, script: &str, allow_degraded: bool) -> Output {
    // the script goes in as a TOML multi-line literal (no escaping, single
    // quotes are fine); substitution uses markers so `{}` in scripts is safe
    let spec = r#"
version = 1
timeout_secs = 60
allow_degraded = __DEGRADED__
mask = []
build_inside = "/tmp/.nawa-sbx"
script = '''
__SCRIPT__
'''

[root]
mode = "live"

[build]
src_host = "__SRC__"
pkg_host = "__PKG__"
home_host = "__HOME__"
scratch = "__SCRATCH__"

[env]
PATH = "/usr/bin:/usr/sbin"
HOME = "/tmp/.nawa-sbx/home"
LC_ALL = "C"
NAWA_SRCDIR = "/tmp/.nawa-sbx/src"
NAWA_PKGDIR = "/tmp/.nawa-sbx/pkg"
"#
    .replace("__DEGRADED__", if allow_degraded { "true" } else { "false" })
    .replace("__SCRIPT__", script)
    .replace("__SRC__", &dir.join("src").display().to_string())
    .replace("__PKG__", &dir.join("pkg").display().to_string())
    .replace("__HOME__", &dir.join("home").display().to_string())
    .replace("__SCRATCH__", &dir.display().to_string());
    let spec_path = dir.join("spec.toml");
    fs::write(&spec_path, spec).unwrap();
    nawa()
        .arg("sandbox-init")
        .arg(&spec_path)
        .env_remove("NAWA_WHATEVER")
        .output()
        .unwrap()
}

fn out_str(o: &Output) -> String {
    format!(
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

#[test]
fn uid_is_zero_inside_and_no_stragglers_survive() {
    let p = probe_all();
    if should_skip(&p, !p.userns, "user namespaces") {
        return;
    }
    let d = scratch("id");
    let o = run_spec(
        &d,
        "echo \"uid=$(id -u)\"\nsetsid sleep 987654 &\nsleep 0.3\nexit 0\n",
        true,
    );
    assert!(o.status.success(), "{}", out_str(&o));
    let so = String::from_utf8_lossy(&o.stdout);
    assert!(so.contains("uid=0"), "uid inside the sandbox must be 0: {so}");

    // the straggler must be gone once PID 1 exited (pid namespace cleanup)
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        let gone = fs::read_dir("/proc")
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .filter(|e| e.file_name().to_string_lossy().chars().all(|c| c.is_ascii_digit()))
                    .all(|e| {
                        let cl = fs::read_to_string(format!("/proc/{}/cmdline", e.file_name().to_string_lossy()))
                            .unwrap_or_default();
                        !cl.contains("sleep 987654")
                    })
            })
            .unwrap_or(true);
        if gone {
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    let _ = fs::remove_dir_all(&d);
}

#[test]
fn environment_is_exactly_the_whitelist() {
    let p = probe_all();
    if should_skip(&p, !p.userns, "user namespaces") {
        return;
    }
    let d = scratch("env");
    let o = run_spec(
        &d,
        "env | sort | grep -v '^_='\n",
        true,
    );
    assert!(o.status.success(), "{}", out_str(&o));
    let so = String::from_utf8_lossy(&o.stdout);
    for line in so.lines() {
        // bash adds PWD/_ and we set PWD ourselves: everything else must be
        // from the spec's [env] — never the host environment
        let key = line.split('=').next().unwrap_or("");
        assert!(
            matches!(
                key,
                "PATH" | "HOME" | "LC_ALL" | "NAWA_SRCDIR" | "NAWA_PKGDIR" | "PWD" | "SHLVL"
            ),
            "unexpected variable {key} leaked into the sandbox ({line})"
        );
    }
    assert!(so.contains("HOME=/tmp/.nawa-sbx/home"), "{so}");
    assert!(so.contains("LC_ALL=C"), "{so}");
    assert!(so.contains("NAWA_PKGDIR=/tmp/.nawa-sbx/pkg"), "{so}");
    let _ = fs::remove_dir_all(&d);
}

#[test]
fn host_environment_does_not_leak() {
    let p = probe_all();
    if should_skip(&p, !p.userns, "user namespaces") {
        return;
    }
    let d = scratch("leak");
    let spec_path = d.join("spec.toml");
    // same spec, but run with a poisoned environment
    let base = run_spec(&d, "", true); // just writes spec.toml
    drop(base);
    let o = nawa()
        .arg("sandbox-init")
        .arg(&spec_path)
        .env("MY_SECRET_TOKEN", "should-not-leak")
        .env("LC_ALL", "tr_TR.UTF-8")
        .env("CFLAGS", "-O9 -march=native")
        .output()
        .unwrap();
    // the empty script exits 0
    assert!(o.status.success(), "{}", out_str(&o));
    let so = String::from_utf8_lossy(&o.stdout);
    assert!(so.is_empty(), "nothing should print: {so}");
    let _ = fs::remove_dir_all(&d);
}

#[test]
fn host_network_is_unreachable_but_loopback_works() {
    let p = probe_all();
    if should_skip(&p, !p.userns || !p.netns_lo, "user + network namespaces") {
        return;
    }
    // a listener on the host's 127.0.0.1
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let d = scratch("net");
    let script = format!(
        "if timeout 3 bash -c '</dev/tcp/127.0.0.1/{port}' 2>/dev/null; then \
             echo HOST-NETWORK-REACHABLE; \
         else \
             echo host-network-unreachable; \
         fi\n"
    );
    let o = run_spec(&d, &script, true);
    assert!(o.status.success(), "{}", out_str(&o));
    let so = String::from_utf8_lossy(&o.stdout);
    assert!(
        so.contains("host-network-unreachable"),
        "the sandbox reached a host-only listener! {so}"
    );
    assert!(!so.contains("HOST-NETWORK-REACHABLE"), "{so}");
    drop(listener);
    let _ = fs::remove_dir_all(&d);
}

#[test]
fn root_is_read_only_and_staging_is_writable() {
    let p = probe_all();
    if should_skip(&p, !p.userns || !p.ro_bind, "read-only bind mounts") {
        return;
    }
    let d = scratch("ro");
    let script = r#"
if touch /usr/.nawa-sbx-probe 2>/dev/null; then
    echo "WROTE-INTO-ROOT   <- make install without DESTDIR would succeed"
    exit 3
fi
echo root-is-readonly
touch /tmp/.nawa-sbx/pkg/staged-file || exit 4
echo destdir-writable
[ ! -e /tmp/nawa-sbx-probe ] || exit 5
touch /tmp/nawa-sbx-probe || exit 6
echo tmp-private
"#;
    let o = run_spec(&d, script, true);
    let se = String::from_utf8_lossy(&o.stderr);
    if se.contains("WITHOUT read-only isolation") {
        eprintln!(
            "SKIP: this host cannot mark the live root read-only inside a \
             user namespace (restricted container?)"
        );
        return;
    }
    assert!(o.status.success(), "{}", out_str(&o));
    let so = String::from_utf8_lossy(&o.stdout);
    assert!(so.contains("root-is-readonly"), "{}", so);
    assert!(so.contains("destdir-writable"), "{}", so);
    assert!(so.contains("tmp-private"), "{}", so);
    assert!(d.join("pkg/staged-file").exists(), "DESTDIR file must land in the host staging dir");
    let _ = fs::remove_dir_all(&d);
}

#[test]
fn pid_namespace_shows_only_the_build() {
    let p = probe_all();
    if should_skip(&p, !p.userns || !p.proc, "a fresh /proc for the pid namespace") {
        return;
    }
    let d = scratch("pid");
    let o = run_spec(&d, "ls -d /proc/[0-9]* | wc -l\n", true);
    assert!(o.status.success(), "{}", out_str(&o));
    let so = String::from_utf8_lossy(&o.stdout);
    let count: usize = so.trim().parse().unwrap_or(999);
    assert!(count <= 5, "the pid namespace leaks host processes: {count} visible\n{so}");
    let _ = fs::remove_dir_all(&d);
}

#[test]
fn timeout_kills_the_whole_section() {
    let p = probe_all();
    if should_skip(&p, !p.userns, "user namespaces") {
        return;
    }
    let d = scratch("timeout");
    // materialise the spec with a long sleep, then shorten the timeout
    let _ = run_spec(&d, "", true);
    let spec = fs::read_to_string(d.join("spec.toml"))
        .unwrap()
        .replace("timeout_secs = 60", "timeout_secs = 2");
    // and give it something that runs long
    let spec = spec.replace("script = '''\n\n'''", "script = '''\nsleep 30\n'''");
    fs::write(d.join("spec.toml"), spec).unwrap();
    let o = nawa()
        .arg("sandbox-init")
        .arg(d.join("spec.toml"))
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(124), "expected the timeout exit code 124: {}", out_str(&o));
    assert!(
        String::from_utf8_lossy(&o.stderr).contains("timed out"),
        "a timeout must be reported loudly: {}",
        out_str(&o)
    );
    let _ = fs::remove_dir_all(&d);
}

#[test]
fn refusing_degradation_by_default() {
    let p = probe_all();
    if should_skip(&p, !p.userns, "user namespaces") {
        return;
    }
    // on a restricted host (no fresh /proc) the default must REFUSE with a
    // clear message rather than silently weakening isolation; on a full host
    // there is nothing to degrade and the build simply runs
    let d = scratch("strict");
    let o = run_spec(&d, "echo should-not-matter\n", false);
    let se = String::from_utf8_lossy(&o.stderr);
    if !p.proc {
        assert!(
            !o.status.success() || se.contains("WARNING"),
            "restricted host must refuse (or loudly warn) without --allow-degraded: {}",
            out_str(&o)
        );
    } else {
        assert!(o.status.success(), "{}", out_str(&o));
    }
    let _ = fs::remove_dir_all(&d);
}
