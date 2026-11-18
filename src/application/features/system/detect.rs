//! `system/detect`: observe the environment into typed data (design §5).
//!
//! Three kinds of state stay separate (§5): the *observed runtime* (PID 1,
//! execution context), the *installed state* of the target root, and the
//! *desired state* (the effective target profile, §7). [`ObservedSystem`]
//! carries them as four separate fields and never collapses them into one
//! `init-system` value (§5.5).
//!
//! Detection is read-only (§5.4): no file is written, no package is
//! changed, and no service manager (`systemctl`, `sv`, …) is queried. A
//! successful migration is never inferred from the presence of a binary or
//! unit file. `Unknown` is a valid observation and is never silently
//! replaced by a guess (§5.1); every observation carries its source, and
//! contradictions are reported, not resolved (§5.4).
//!
//! Attribution rules (§5.5): host `/proc` describes the managed root only
//! when the root is canonical (`/`). With an explicit other `--root`, only
//! target-root evidence is read and the runtime is `Unknown` — host
//! `/proc` must never be presented as the target's runtime state. In a
//! chroot, `/proc/1` belongs to the outer environment, so the runtime init
//! is `Unknown` there. In a container, PID 1 *is* the init of the
//! environment being managed (own PID namespace), so it is reported with
//! its evidence; the execution context is what a migration gate checks.
//!
//! Every source is an injectable path, so each rule is unit-testable
//! without booting anything and without touching the host.


use std::fs;
use std::path::{Path, PathBuf};

use crate::domain::system::identity::{classify_pid1, identify, InitKind, Pid1};

pub(crate) use crate::domain::system::observation::{
    Confidence, Context, DesiredInit, Evidence, InitCandidate, InstalledInit,
    ObservedSystem, RuntimeInit,
};

/// Read the desired init and retain the validated profile from the same
/// filesystem snapshot. Missing policy is not interpreted as an empty one.
fn desired_state(root: &Path) -> (DesiredInit, Option<crate::domain::system::profile::SystemProfile>) {
    match super::profile::load_effective(root) {
        Ok(effective) => {
            let desired = match effective
                .profile
                .as_ref()
                .and_then(|profile| profile.provider("init-system"))
            {
                Some(identity) => DesiredInit::Declared {
                    identity: identity.to_string(),
                    source: effective
                        .role_sources
                        .get("init-system")
                        .map(|path| path.display().to_string())
                        .unwrap_or_default(),
                },
                None => DesiredInit::NotDeclared,
            };
            (desired, effective.profile)
        }
        Err(error) => (
            DesiredInit::Invalid {
                reason: format!("{error:#}"),
            },
            None,
        ),
    }
}

/// Observe the managed `root` and this process's own environment. `proc` is
/// the proc mount of the process's own environment (normally `/proc`); it
/// is read only when `root` is canonical (`/`) — with an explicit other
/// root, host `/proc` never describes the target (§5.2, §5.5). Read-only.
pub(crate) fn observe(root: &Path, proc: &Path) -> ObservedSystem {
    if !is_canonical(root) {
        // §5.5 target-root/offline rows: report installed/configured
        // candidates only; host /proc is not the target's runtime state
        let (desired, effective_profile) = desired_state(root);
        return ObservedSystem {
            root: root.to_path_buf(),
            context: Context::OfflineRoot {
                root: root.to_path_buf(),
            },
            runtime: RuntimeInit::Unknown {
                reason: "--root targets another root: host /proc is not the target's runtime state"
                    .to_string(),
            },
            installed: installed_init(root),
            desired,
            effective_profile,
        };
    }
    observe_canonical(root, proc)
}

/// Observe a canonical (live-reachable) root. Private composition of the
/// injectable pieces; `pub(crate)` so the unit tests can drive it with a
/// fake proc and a fake managed root instead of the host.
pub(crate) fn observe_canonical(managed_root: &Path, proc: &Path) -> ObservedSystem {
    let context = detect_context(managed_root, proc);
    let runtime = match &context {
        // a chroot has no PID 1 of its own: /proc/1 belongs to the outer
        // environment and must not be attributed to the managed root
        Context::Chroot { .. } => RuntimeInit::Unknown {
            reason: "this process runs in a chroot: /proc/1 belongs to the outer environment, \
                     not the managed root"
                .to_string(),
        },
        Context::Unknown { reason } => RuntimeInit::Unknown {
            reason: format!(
                "the execution context is unattributable ({reason}), so PID 1 \
                             evidence is not trusted here"
            ),
        },
        // live host and container: /proc/1 is the init of the environment
        // being managed; the context field is what a migration gate checks
        _ => runtime_init(proc),
    };
    let (desired, effective_profile) = desired_state(managed_root);
    ObservedSystem {
        root: managed_root.to_path_buf(),
        context,
        runtime,
        installed: installed_init(managed_root),
        desired,
        effective_profile,
    }
}

/// Canonical means the managed root is the environment's own `/` (§5.5:
/// only then does host /proc describe the managed root).
fn is_canonical(root: &Path) -> bool {
    match root.canonicalize() {
        Ok(resolved) => resolved == Path::new("/"),
        Err(_) => root == Path::new("/"),
    }
}

/// Classify this process's execution context (§5.4: prefer structured,
/// attributable sources; report the unavailable explicitly). `markers_root`
/// is where container marker files live (the canonical root itself).
fn detect_context(markers_root: &Path, proc: &Path) -> Context {
    // container markers: files first, then the PID 1 cgroup
    let mut container = Vec::new();
    for (rel, label) in [
        (".dockerenv", "docker marker file"),
        ("run/.containerenv", "podman marker file"),
    ] {
        let full = markers_root.join(rel);
        if fs::symlink_metadata(&full).is_ok() {
            container.push(Evidence {
                source: label.to_string(),
                value: full.display().to_string(),
            });
        }
    }
    if let Ok(cgroup) = fs::read_to_string(proc.join("1/cgroup")) {
        for marker in ["docker", "lxc", "kubepods", "containerd", "podman"] {
            if cgroup.contains(marker) {
                container.push(Evidence {
                    source: "/proc/1/cgroup".to_string(),
                    value: format!("contains {marker:?}"),
                });
                break;
            }
        }
    }
    if !container.is_empty() {
        return Context::Container {
            evidence: container,
        };
    }

    // chroot: this process's root is not PID 1's root
    match same_root_as_pid1(proc) {
        Ok(true) => Context::LiveHost {
            evidence: vec![
                Evidence {
                    source: "stat /proc/1/root vs /proc/self/root".to_string(),
                    value: "same directory (no chroot boundary)".to_string(),
                },
                Evidence {
                    source: "container markers".to_string(),
                    value: "none found".to_string(),
                },
            ],
        },
        Ok(false) => Context::Chroot {
            evidence: vec![Evidence {
                source: "stat /proc/1/root vs /proc/self/root".to_string(),
                value: "different directories (this process is chrooted relative to PID 1)"
                    .to_string(),
            }],
        },
        Err(err) => Context::Unknown { reason: err },
    }
}

/// Does this process run with the same root directory as PID 1? stat'ing
/// `/proc/1/root` may be denied for an unprivileged caller; that failure is
/// reported as an unattributable context, never as `LiveHost`.
fn same_root_as_pid1(proc: &Path) -> Result<bool, String> {
    use std::os::unix::fs::MetadataExt;
    let pid1 = fs::metadata(proc.join("1/root")).map_err(|e| {
        format!(
            "no readable /proc: cannot stat {}/1/root ({e})",
            proc.display()
        )
    })?;
    let own = fs::metadata(proc.join("self/root")).map_err(|e| {
        format!(
            "no readable /proc: cannot stat {}/self/root ({e})",
            proc.display()
        )
    })?;
    Ok(pid1.dev() == own.dev() && pid1.ino() == own.ino())
}

/// The runtime init of the environment whose proc this is (§5.1): the
/// `/proc/1/exe` link target and the `/proc/1/comm` value, classified with
/// their sources.
fn runtime_init(proc: &Path) -> RuntimeInit {
    let exe = fs::read_link(proc.join("1/exe"))
        .ok()
        .map(|p| p.to_string_lossy().into_owned());
    let comm = fs::read_to_string(proc.join("1/comm"))
        .ok()
        .map(|s| s.trim().to_string());
    if exe.is_none() && comm.is_none() {
        return RuntimeInit::Unknown {
            reason: format!(
                "no readable PID 1 evidence ({}/1/exe and {}/1/comm are both unreadable)",
                proc.display(),
                proc.display()
            ),
        };
    }
    let evidence = pid1_evidence(&exe, &comm);
    match classify_pid1(exe.as_deref(), comm.as_deref()) {
        Pid1::Confirmed(kind) => RuntimeInit::Observed {
            kind,
            confidence: Confidence::Proven,
            evidence,
        },
        Pid1::Indicated(kind) => RuntimeInit::Observed {
            kind,
            confidence: Confidence::Indicated,
            evidence,
        },
        Pid1::Ambiguous { reason } | Pid1::Contradictory { reason } => {
            RuntimeInit::Unknown { reason }
        }
    }
}

fn pid1_evidence(exe: &Option<String>, comm: &Option<String>) -> Vec<Evidence> {
    let mut evidence = Vec::new();
    if let Some(value) = exe {
        evidence.push(Evidence {
            source: "/proc/1/exe".to_string(),
            value: value.clone(),
        });
    }
    if let Some(value) = comm {
        evidence.push(Evidence {
            source: "/proc/1/comm".to_string(),
            value: value.clone(),
        });
    }
    evidence
}

/// The staged init candidates of a target root (§5.2). Paths are checked
/// with `symlink_metadata` (a broken symlink is still installed intent) and
/// symlink targets are classified by their text — never resolved through
/// the filesystem, so detection never leaves the target root.
fn installed_init(root: &Path) -> InstalledInit {
    let mut candidates = Vec::new();

    // provider payloads shipped by the packages themselves
    for (rel, kind) in [
        ("usr/lib/systemd/systemd", InitKind::Systemd),
        ("lib/systemd/systemd", InitKind::Systemd),
        ("etc/runit/2", InitKind::Runit),
        ("usr/bin/runit", InitKind::Runit),
        ("sbin/runit-init", InitKind::Runit),
    ] {
        let full = root.join(rel);
        if fs::symlink_metadata(&full).is_ok() {
            candidates.push(InitCandidate {
                kind,
                path: full,
                evidence: "target-root path present".to_string(),
            });
        }
    }

    // the generic `init` entry point: evidence of installed intent, never
    // proof of the running PID 1 (§5.5)
    for rel in ["sbin/init", "usr/sbin/init", "usr/bin/init"] {
        let full = root.join(rel);
        let Ok(meta) = fs::symlink_metadata(&full) else {
            continue;
        };
        let (kind, evidence) = if meta.file_type().is_symlink() {
            match fs::read_link(&full) {
                Ok(target) => {
                    let text = target.to_string_lossy();
                    let kind = identify(&text).unwrap_or(InitKind::Other("init".to_string()));
                    let generic = if matches!(kind, InitKind::Other(ref n) if n == "init") {
                        " (generic target)"
                    } else {
                        ""
                    };
                    (
                        kind,
                        format!("target-root init entry point, symlink → {text}{generic}"),
                    )
                }
                Err(_) => (
                    InitKind::Other("init".to_string()),
                    "target-root init entry point, unresolvable symlink".to_string(),
                ),
            }
        } else {
            (
                InitKind::Other("init".to_string()),
                "target-root init entry point (not a symlink; identity not attributable from \
                 the path alone)"
                    .to_string(),
            )
        };
        candidates.push(InitCandidate {
            kind,
            path: full,
            evidence,
        });
    }

    InstalledInit { candidates }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn dir(base: &Path, rel: &str) -> PathBuf {
        let p = base.join(rel);
        fs::create_dir_all(&p).unwrap();
        p
    }

    fn file(base: &Path, rel: &str, contents: &str) -> PathBuf {
        let p = base.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, contents).unwrap();
        p
    }

    /// A fake /proc: PID 1 with the given exe/comm evidence plus `pid1_root`
    /// and `self_root` symlinks for the chroot check.
    fn fake_proc(
        base: &Path,
        exe: Option<&str>,
        comm: Option<&str>,
        pid1_root: &Path,
        self_root: &Path,
    ) -> PathBuf {
        let proc = dir(base, "proc");
        dir(base, "proc/1");
        dir(base, "proc/self");
        if let Some(exe) = exe {
            symlink(exe, proc.join("1/exe")).unwrap();
        }
        if let Some(comm) = comm {
            file(base, "proc/1/comm", &format!("{comm}\n"));
        }
        symlink(pid1_root, proc.join("1/root")).unwrap();
        symlink(self_root, proc.join("self/root")).unwrap();
        proc
    }

    fn expect_context(observed: &ObservedSystem) -> &Context {
        &observed.context
    }

    #[test]
    fn live_systemd_host_is_confirmed_from_two_sources() {
        let base = crate::testutil::tmpdir("detect-systemd");
        let root = dir(&base, "rootdir");
        file(&base, "rootdir/usr/lib/systemd/systemd", "");
        let same = dir(&base, "oneslash");
        let exe = format!("{}/usr/lib/systemd/systemd", base.display());
        let proc = fake_proc(&base, Some(&exe), Some("systemd"), &same, &same);

        let observed = observe_canonical(&root, &proc);
        assert_eq!(observed.root, root);
        assert_eq!(observed.desired, DesiredInit::NotDeclared);
        match expect_context(&observed) {
            Context::LiveHost { evidence } => assert!(
                evidence.iter().any(|e| e.source.contains("/proc/1/root")),
                "expected the root-stat evidence, got {evidence:?}"
            ),
            other => panic!("expected LiveHost, got {other:?}"),
        }
        match &observed.runtime {
            RuntimeInit::Observed {
                kind,
                confidence,
                evidence,
            } => {
                assert_eq!(*kind, InitKind::Systemd);
                assert_eq!(*confidence, Confidence::Proven);
                assert!(
                    evidence
                        .iter()
                        .any(|e| e.source == "/proc/1/exe" && e.value.ends_with("systemd")),
                    "expected the exe evidence, got {evidence:?}"
                );
                assert!(
                    evidence
                        .iter()
                        .any(|e| e.source == "/proc/1/comm" && e.value == "systemd"),
                    "expected the comm evidence, got {evidence:?}"
                );
            }
            other => panic!("expected an observed runtime init, got {other:?}"),
        }
        assert!(observed
            .installed
            .candidates
            .iter()
            .any(|c| c.kind == InitKind::Systemd && c.path.ends_with("usr/lib/systemd/systemd")));
    }

    #[test]
    fn live_runit_host_is_confirmed_and_staged_candidates_are_found() {
        let base = crate::testutil::tmpdir("detect-runit");
        let root = dir(&base, "rootdir");
        file(&base, "rootdir/etc/runit/2", "#!/bin/sh\n");
        file(&base, "rootdir/usr/bin/runit", "");
        let same = dir(&base, "oneslash");
        let exe = format!("{}/usr/bin/runit", base.display());
        let proc = fake_proc(&base, Some(&exe), Some("runit"), &same, &same);

        let observed = observe_canonical(&root, &proc);
        match &observed.runtime {
            RuntimeInit::Observed {
                kind, confidence, ..
            } => {
                assert_eq!(*kind, InitKind::Runit);
                assert_eq!(*confidence, Confidence::Proven);
            }
            other => panic!("expected an observed runit init, got {other:?}"),
        }
        for rel in ["etc/runit/2", "usr/bin/runit"] {
            assert!(
                observed
                    .installed
                    .candidates
                    .iter()
                    .any(|c| c.kind == InitKind::Runit && c.path.ends_with(rel)),
                "missing candidate {rel} in {:?}",
                observed.installed.candidates
            );
        }
    }

    #[test]
    fn one_identifying_source_gives_indicated_confidence() {
        let base = crate::testutil::tmpdir("detect-indicated");
        let root = dir(&base, "rootdir");
        let same = dir(&base, "oneslash");
        let exe = format!("{}/usr/lib/systemd/systemd", base.display());
        // no comm file: the exe alone identifies
        let proc = fake_proc(&base, Some(&exe), None, &same, &same);

        let observed = observe_canonical(&root, &proc);
        match &observed.runtime {
            RuntimeInit::Observed {
                kind,
                confidence,
                evidence,
            } => {
                assert_eq!(*kind, InitKind::Systemd);
                assert_eq!(*confidence, Confidence::Indicated);
                assert_eq!(evidence.len(), 1);
                assert_eq!(evidence[0].source, "/proc/1/exe");
            }
            other => panic!("expected an observed init, got {other:?}"),
        }
    }

    #[test]
    fn generic_only_evidence_is_unknown_not_a_guess() {
        let base = crate::testutil::tmpdir("detect-generic");
        let root = dir(&base, "rootdir");
        let same = dir(&base, "oneslash");
        let proc = fake_proc(&base, Some("/sbin/init"), Some("init"), &same, &same);

        let observed = observe_canonical(&root, &proc);
        assert!(matches!(observed.context, Context::LiveHost { .. }));
        match &observed.runtime {
            RuntimeInit::Unknown { reason } => {
                assert!(
                    reason.contains("generically"),
                    "unexpected reason: {reason}"
                );
            }
            other => panic!("expected Unknown, got {other:?}"),
        }
    }

    #[test]
    fn contradictory_pid1_evidence_is_reported_not_resolved() {
        let base = crate::testutil::tmpdir("detect-contradiction");
        let root = dir(&base, "rootdir");
        let same = dir(&base, "oneslash");
        let exe = format!("{}/usr/lib/systemd/systemd", base.display());
        let proc = fake_proc(&base, Some(&exe), Some("runit"), &same, &same);

        let observed = observe_canonical(&root, &proc);
        match &observed.runtime {
            RuntimeInit::Unknown { reason } => {
                assert!(reason.contains("disagrees"), "unexpected reason: {reason}");
                assert!(reason.contains("systemd") && reason.contains("runit"));
            }
            other => panic!("expected Unknown, got {other:?}"),
        }
    }

    #[test]
    fn a_dockerenv_marker_file_makes_the_context_a_container() {
        let base = crate::testutil::tmpdir("detect-docker-file");
        let root = dir(&base, "rootdir");
        file(&base, "rootdir/.dockerenv", "");
        let same = dir(&base, "oneslash");
        let exe = format!("{}/usr/lib/systemd/systemd", base.display());
        let proc = fake_proc(&base, Some(&exe), Some("systemd"), &same, &same);

        let observed = observe_canonical(&root, &proc);
        match expect_context(&observed) {
            Context::Container { evidence } => assert!(
                evidence.iter().any(|e| e.value.ends_with(".dockerenv")),
                "expected the marker-file evidence, got {evidence:?}"
            ),
            other => panic!("expected Container, got {other:?}"),
        }
        // PID 1 of the container's own namespace is attributable and reported
        assert!(matches!(
            observed.runtime,
            RuntimeInit::Observed {
                kind: InitKind::Systemd,
                ..
            }
        ));
    }

    #[test]
    fn a_cgroup_marker_makes_the_context_a_container() {
        let base = crate::testutil::tmpdir("detect-cgroup");
        let root = dir(&base, "rootdir");
        let same = dir(&base, "oneslash");
        let proc = fake_proc(&base, Some("/usr/bin/runit"), Some("runit"), &same, &same);
        file(&base, "proc/1/cgroup", "12:pids:/docker/abc123\n0::/\n");

        let observed = observe_canonical(&root, &proc);
        match expect_context(&observed) {
            Context::Container { evidence } => assert!(
                evidence
                    .iter()
                    .any(|e| e.source == "/proc/1/cgroup" && e.value.contains("docker")),
                "expected the cgroup evidence, got {evidence:?}"
            ),
            other => panic!("expected Container, got {other:?}"),
        }
    }

    #[test]
    fn a_chrooted_process_cannot_observe_the_managed_roots_runtime() {
        let base = crate::testutil::tmpdir("detect-chroot");
        let root = dir(&base, "rootdir");
        file(&base, "rootdir/usr/lib/systemd/systemd", "");
        let outer = dir(&base, "outer-root");
        let inner = dir(&base, "inner-root");
        let exe = format!("{}/usr/lib/systemd/systemd", base.display());
        let proc = fake_proc(&base, Some(&exe), Some("systemd"), &outer, &inner);

        let observed = observe_canonical(&root, &proc);
        assert!(matches!(observed.context, Context::Chroot { .. }));
        match &observed.runtime {
            RuntimeInit::Unknown { reason } => {
                assert!(reason.contains("chroot"), "unexpected reason: {reason}");
            }
            other => panic!("expected Unknown, got {other:?}"),
        }
        // installed state is still readable (§5.2)
        assert!(observed
            .installed
            .candidates
            .iter()
            .any(|c| c.kind == InitKind::Systemd));
    }

    #[test]
    fn an_unreadable_proc_leaves_both_context_and_runtime_unknown() {
        let base = crate::testutil::tmpdir("detect-noproc");
        let root = dir(&base, "rootdir");
        let proc = base.join("no-such-proc");

        let observed = observe_canonical(&root, &proc);
        match expect_context(&observed) {
            Context::Unknown { reason } => {
                assert!(
                    reason.contains("no readable /proc"),
                    "unexpected reason: {reason}"
                );
            }
            other => panic!("expected Unknown, got {other:?}"),
        }
        assert!(matches!(observed.runtime, RuntimeInit::Unknown { .. }));
    }

    #[test]
    fn an_explicit_other_root_never_reads_host_proc() {
        let base = crate::testutil::tmpdir("detect-offline");
        let root = dir(&base, "rootdir");
        file(&base, "rootdir/etc/runit/2", "#!/bin/sh\n");
        // a proc tree with systemd evidence that must NOT be consulted
        let same = dir(&base, "oneslash");
        let proc = fake_proc(
            &base,
            Some("/usr/lib/systemd/systemd"),
            Some("systemd"),
            &same,
            &same,
        );

        let observed = observe(&root, &proc);
        assert_eq!(observed.root, root);
        assert_eq!(
            observed.context,
            Context::OfflineRoot { root: root.clone() }
        );
        match &observed.runtime {
            RuntimeInit::Unknown { reason } => {
                assert!(
                    reason.contains("--root") && reason.contains("not the target's runtime state"),
                    "unexpected reason: {reason}"
                );
            }
            other => panic!("expected Unknown, got {other:?}"),
        }
        // installed state comes from the target root
        assert!(observed
            .installed
            .candidates
            .iter()
            .any(|c| c.kind == InitKind::Runit && c.path.ends_with("etc/runit/2")));
    }

    #[test]
    fn a_canonical_root_argument_is_treated_as_the_live_reachable_root() {
        let base = crate::testutil::tmpdir("detect-canonical");
        // observe() with the canonical "/" takes the observe_canonical path;
        // drive it through the same code with a fake managed root
        assert!(is_canonical(Path::new("/")));
        assert!(!is_canonical(&base));
        // a missing path falls back to the literal comparison
        assert!(!is_canonical(&base.join("no-such-dir")));
    }

    #[test]
    fn an_empty_target_root_stages_no_candidates() {
        let base = crate::testutil::tmpdir("detect-empty-root");
        let root = dir(&base, "rootdir");
        let installed = installed_init(&root);
        assert!(
            installed.candidates.is_empty(),
            "got {:?}",
            installed.candidates
        );
    }

    #[test]
    fn two_staged_providers_are_both_reported() {
        // §6.2: side-by-side providers during a staged transition; B2's
        // coexistence scenario — reported, not resolved
        let base = crate::testutil::tmpdir("detect-both");
        let root = dir(&base, "rootdir");
        file(&base, "rootdir/usr/lib/systemd/systemd", "");
        file(&base, "rootdir/etc/runit/2", "#!/bin/sh\n");

        let installed = installed_init(&root);
        let kinds: Vec<&InitKind> = installed.candidates.iter().map(|c| &c.kind).collect();
        assert!(kinds.contains(&&InitKind::Systemd));
        assert!(kinds.contains(&&InitKind::Runit));
    }

    #[test]
    fn an_init_symlink_is_classified_by_its_target_text() {
        let base = crate::testutil::tmpdir("detect-init-link");
        let root = dir(&base, "rootdir");
        fs::create_dir_all(root.join("sbin")).unwrap();
        symlink("../lib/systemd/systemd", root.join("sbin/init")).unwrap();
        fs::create_dir_all(root.join("usr/bin")).unwrap();
        symlink("../bin/runit", root.join("usr/bin/init")).unwrap();

        let installed = installed_init(&root);
        let systemd = installed
            .candidates
            .iter()
            .find(|c| c.path.ends_with("sbin/init"))
            .expect("sbin/init candidate");
        assert_eq!(systemd.kind, InitKind::Systemd);
        assert!(systemd
            .evidence
            .contains("symlink → ../lib/systemd/systemd"));
        let runit = installed
            .candidates
            .iter()
            .find(|c| c.path.ends_with("usr/bin/init"))
            .expect("usr/bin/init candidate");
        assert_eq!(runit.kind, InitKind::Runit);
    }

    #[test]
    fn a_plain_init_file_is_installed_intent_with_an_unattributed_identity() {
        let base = crate::testutil::tmpdir("detect-init-file");
        let root = dir(&base, "rootdir");
        file(&base, "rootdir/usr/bin/init", "\u{7f}ELF..");

        let installed = installed_init(&root);
        assert_eq!(installed.candidates.len(), 1);
        let candidate = &installed.candidates[0];
        assert_eq!(candidate.kind, InitKind::Other("init".to_string()));
        assert!(
            candidate.evidence.contains("not a symlink"),
            "got {}",
            candidate.evidence
        );
    }

    #[test]
    fn a_broken_init_symlink_still_counts_as_installed_intent() {
        let base = crate::testutil::tmpdir("detect-init-broken");
        let root = dir(&base, "rootdir");
        fs::create_dir_all(root.join("sbin")).unwrap();
        symlink("../lib/systemd/systemd", root.join("sbin/init")).unwrap();
        // the target is missing: still evidence of intent (§5.2)

        let installed = installed_init(&root);
        assert_eq!(installed.candidates.len(), 1);
        assert_eq!(installed.candidates[0].kind, InitKind::Systemd);
    }

    #[test]
    fn the_target_profile_declares_the_desired_init_with_its_source() {
        // §5.5 desired state: the effective target profile's init-system
        // role, attributed to the file that declared it (§5.1)
        let base = crate::testutil::tmpdir("detect-profile-declared");
        let root = dir(&base, "rootdir");
        file(
            &base,
            "rootdir/etc/nawa/system-profile.toml",
            "schema = 1\n\n[providers]\ninit-system = \"systemd\"\n",
        );
        // the offline branch never reads `proc`, so a plain path suffices
        let observed = observe(&root, Path::new("/proc"));
        match &observed.desired {
            DesiredInit::Declared { identity, source } => {
                assert_eq!(identity, "systemd");
                assert_eq!(
                    source,
                    &root
                        .join("etc/nawa/system-profile.toml")
                        .display()
                        .to_string()
                );
            }
            other => panic!("expected Declared, got {other:?}"),
        }
    }

    #[test]
    fn an_invalid_target_profile_is_reported_not_guessed_around() {
        // §7 fail-closed: a broken profile is `Invalid` with the file
        // named, never silently read as "no decision"
        let base = crate::testutil::tmpdir("detect-profile-invalid");
        let root = dir(&base, "rootdir");
        file(
            &base,
            "rootdir/etc/nawa/system-profile.toml",
            "schema = 1\n\nfuture-key = true\n",
        );
        let desired = desired_init(&root);
        match &desired {
            DesiredInit::Invalid { reason } => {
                assert!(
                    reason.contains("etc/nawa/system-profile.toml"),
                    "the error must name the file, got: {reason}"
                );
                assert!(reason.contains("future-key"), "got: {reason}");
            }
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    #[test]
    fn a_target_profile_without_an_init_role_leaves_the_desired_init_undeclared() {
        // a profile that says nothing about init-system is not a decision
        let base = crate::testutil::tmpdir("detect-profile-silent");
        let root = dir(&base, "rootdir");
        file(
            &base,
            "rootdir/usr/share/nawa/system-profile.toml",
            "schema = 1\n\n[providers]\nsession-manager = \"elogind\"\n",
        );
        assert_eq!(desired_init(&root), DesiredInit::NotDeclared);
    }
}
