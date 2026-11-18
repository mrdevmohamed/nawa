//! Live-host preflight, read-only plan and explicit apply authorization for
//! the built-in Faseela migration (design §10.4 stages 1–5, §12.2, §12.4).
//!
//! The migration definition is *built into Nawa*: a typed, versioned value
//! bound to the allowlisted built-in handler (§10.1–§10.2). Repository
//! metadata can never supply a handler path, a shell command or a migration
//! id here. The definition is therefore not fetched from a repository.
//!
//! `plan` is read-only (it works on an offline root and persists nothing).
//! `apply` performs the real live-host preflight — canonical live root, no
//! chroot/container, root privileges, observed source provider and declared
//! target provider — and consumes an explicit, migration-specific approval
//! (§12.4: a generic `--yes` is never enough). The coordinator is not invoked
//! while the distribution-owned boot/recovery contract remains blocked.
//!
//! **Current production status:** the built-in handler's boot/initramfs side
//! of the transition is still blocked on external Faseela/SAQ decisions
//! (B1/B4/B5/B6/B7 — see `docs/implementation-progress.md`). `apply` refuses
//! at that boundary with the exact blocker; it never fabricates a successful
//! migration, never creates a durable record on a machine it cannot make
//! bootable, and never reboots. This is the honest fail-closed behavior until
//! the external contract is agreed.

use std::path::Path;

use anyhow::{bail, Context, Result};

use crate::application::features::system::detect::{self, Context as ExecContext, RuntimeInit};
use crate::domain::system::capability::Capability;
use crate::domain::system::identity::InitKind;
use crate::domain::system::migration::MigrationDefinition;
use crate::domain::system::observation::Confidence;

/// Expose the built-in domain migration definition to the application
/// coordinator. The value is not read from a repository; its handler binding
/// and reboot requirement are validated by the domain model.
pub(crate) fn builtin_definition() -> MigrationDefinition {
    crate::domain::system::migration::builtin_faseela_runit_to_systemd()
}

/// The read-only plan: what was observed, what was declared and what the
/// approved migration would do. Typed data; the handler renders it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MigrationPlan {
    pub(crate) definition: MigrationDefinition,
    pub(crate) observed: detect::ObservedSystem,
    /// boot/initramfs actions are blockers until the external contract lands
    pub(crate) boot_actions: Vec<String>,
    /// honest rollback limits (§10.5)
    pub(crate) rollback_limits: Vec<String>,
}

/// Build a read-only plan for the built-in migration under `root`. No record
/// is written and nothing is executed; this works on an offline root.
pub(crate) fn plan(root: &Path, proc: &Path) -> MigrationPlan {
    let definition = builtin_definition();
    let observed = detect::observe(root, proc);
    MigrationPlan {
        definition,
        observed,
        boot_actions: vec![
            "retain the old init provider and boot artifacts until verification".to_string(),
            "prepare boot/initramfs so the target init is selected explicitly".to_string(),
            "reboot required — Nawa never reboots automatically".to_string(),
        ],
        rollback_limits: vec![
            "package-level rollback may restore installed packages; boot/initramfs changes need a documented recovery route".to_string(),
            "no automatic rollback of arbitrary root filesystem changes is promised".to_string(),
        ],
    }
}

/// Live-host preflight (§10.4 stage 2). All refusals happen before any
/// mutation, record or lock. Returns the observation so the caller can show
/// exactly which facts were checked.
pub(crate) fn preflight(root: &Path, proc: &Path) -> Result<detect::ObservedSystem> {
    // §12.2: apply is a live-host operation. An explicit non-canonical --root
    // is an offline target even if a fake `/proc` would say otherwise.
    let canonical = match root.canonicalize() {
        Ok(resolved) => resolved == Path::new("/"),
        Err(_) => root == Path::new("/"),
    };
    if !canonical {
        bail!(
            "system migrate apply is refused for the offline root {} — planning is read-only, apply is live-host only",
            root.display()
        );
    }
    preflight_inner(root, proc, true)
}

/// The injectable-proc / test-privilege variant: every observation and policy
/// rule is the production one except the process's real uid check.
#[cfg(test)]
fn preflight_for_tests(root: &Path, proc: &Path) -> Result<detect::ObservedSystem> {
    preflight_inner(root, proc, false)
}

fn preflight_inner(root: &Path, proc: &Path, require_root: bool) -> Result<detect::ObservedSystem> {
    let observed = detect::observe_canonical(root, proc);
    match &observed.context {
        ExecContext::LiveHost { .. } => {}
        ExecContext::Container { .. } => bail!(
            "system migrate apply is refused in a container: host PID 1 is not the verified Faseela host"
        ),
        ExecContext::Chroot { .. } => bail!(
            "system migrate apply is refused in a chroot: /proc/1 belongs to the outer environment"
        ),
        ExecContext::OfflineRoot { root } => bail!(
            "system migrate apply is refused for the offline root {} — planning is read-only, apply is live-host only",
            root.display()
        ),
        ExecContext::Unknown { reason } => bail!(
            "system migrate apply is refused: the execution context cannot be verified ({reason})"
        ),
    }
    if require_root && crate::adapters::sys::geteuid() != 0 {
        bail!("system migrate apply requires root privileges for boot/init changes");
    }
    let definition = builtin_definition();
    let source = InitKind::from_capability(&definition.from)
        .expect("the built-in definition selects a known init");
    match &observed.runtime {
        // Same bar as the upgrade preview (`upgrade/system_transition.rs`):
        // a PID 1 identity backed by a single evidence source is only
        // indicated, never proven, and must not authorize a live-host change.
        RuntimeInit::Observed {
            kind,
            confidence: Confidence::Indicated,
            ..
        } => bail!(
            "the running init looks like {} but is only indicated by one evidence source; \
             a proven PID 1 identity is required before applying a migration",
            kind.capability_identity()
        ),
        RuntimeInit::Observed { kind, .. } if *kind == source => {}
        RuntimeInit::Observed { kind, .. } => bail!(
            "the running init is {} but this migration requires {} — refusing to apply a stale plan (§10.4 stage 5)",
            kind.capability_identity(),
            source.capability_identity()
        ),
        RuntimeInit::Unknown { reason } => bail!(
            "the running init cannot be verified ({reason}) — refusing to apply a migration to an unknown host"
        ),
    }
    // The desired state must be declared by an authenticated local profile;
    // an absent or invalid profile is never guessed into a decision (§7).
    match &observed.desired {
        detect::DesiredInit::Declared { identity, .. }
            if identity == &definition.to.identity => {}
        detect::DesiredInit::Declared { identity, .. } => bail!(
            "the target profile selects init-system={identity} but this migration targets {} — refusing to apply a stale plan",
            definition.to.to_key()
        ),
        detect::DesiredInit::NotDeclared => bail!(
            "no system profile declares the target init — migration apply requires an explicit desired state (§7)"
        ),
        detect::DesiredInit::Invalid { reason } => bail!(
            "the system profile is invalid ({reason}) — refusing to apply against a broken desired state"
        ),
    }
    Ok(observed)
}

/// Explicit, migration-specific authorization (§12.4). A generic `--yes`
/// cannot construct this: the caller must name the exact migration id, source
/// and target and acknowledge the reboot and recovery requirements.
#[derive(Debug)]
pub(crate) struct MigrationApproval {
    _private: (),
}

impl MigrationApproval {
    pub(crate) fn explicit(
        id: &str,
        from: &Capability,
        to: &Capability,
        acknowledge_reboot: bool,
        acknowledge_recovery: bool,
    ) -> Result<Self> {
        let definition = builtin_definition();
        if id != definition.id || *from != definition.from || *to != definition.to {
            bail!(
                "the migration authorization does not match the built-in definition ({}: {} → {}) — refusing (§12.4)",
                definition.id,
                definition.from.to_key(),
                definition.to.to_key()
            );
        }
        if !acknowledge_reboot || !acknowledge_recovery {
            bail!("migration authorization must acknowledge the reboot boundary and the documented recovery path (§12.4)");
        }
        Ok(MigrationApproval { _private: () })
    }
}

/// Blockers are evaluated against the default branches. Companion changes
/// are merged; installed-root and real disk-boot acceptance remain separate
/// requirements and are not inferred from successful CI.
const HANDLER_BLOCKERS: &[&str] = &[
    "B1: Faseela PR #59 and SAQ PR #4 are merged; PR #60 adds the installed-disk BIOS+UEFI harness and release-workflow integration, but a real release-ISO run with both firmware logs proving /proc/1/comm=systemd is still required",
    "B2: exact /usr/bin/init co-ownership is migration-scoped and tested; retain runit-services through reboot and remove it only after verified completion. The post-boot finalizer and package-removal step are not wired",
    "B4: Faseela Repo PR #203 is merged and base-files ships /usr/share/nawa/system-profile.toml; loader/default and administrator override tests pass, but installed BIOS/UEFI image acceptance remains pending",
    "B5: the migration-specific boot snapshot adapter exists with manifest-last publication, SHA-256 validation, an explicit file allowlist and idempotent restore tests. It is not wired into coordinator/recovery; BIOS sectors, EFI NVRAM and the external Live-ISO rescue contract remain unresolved",
    "B6: Faseela PR #60 adds the BIOS+UEFI installed-disk harness and release-workflow integration, with dry-run/installer-guard CI tests. Real release-ISO execution and snapshot-driven interrupted-install, failed-boot and restore acceptance remain pending",
    "B7: Faseela Repo PR #203 is merged; recipe CI rejects direct service-state mutations in privileged hooks and transaction-trigger sources, and Faseela applies vendor presets during image provisioning. Installed-package runtime acceptance remains required; the static guard is not a shell sandbox",
];

/// How an apply attempt ended before any package mutation.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ApplyRefusal {
    /// the host passed preflight and authorization, but the built-in boot
    /// handler is not yet safe to run (external blockers)
    HandlerBlocked(Vec<&'static str>),
}

/// Apply the built-in migration. Live-host preflight and explicit approval
/// are enforced first; no durable record, gate or package change is created.
/// The boot/initramfs handler is currently blocked on external decisions, so
/// this returns a documented refusal instead of pretending to migrate.
pub(crate) fn apply(
    root: &Path,
    proc: &Path,
    approval: MigrationApproval,
) -> Result<ApplyRefusal> {
    let _ = approval; // consumed: one authorization authorizes one attempt
    preflight(root, proc).context("migration apply preflight")?;
    Ok(ApplyRefusal::HandlerBlocked(HANDLER_BLOCKERS.to_vec()))
}

/// Test-only variant that skips only the real-uid check (a fake `/proc` is
/// always used in tests; the host's actual privilege is irrelevant there).
#[cfg(test)]
fn apply_for_tests(root: &Path, proc: &Path, approval: MigrationApproval) -> Result<ApplyRefusal> {
    let _ = approval;
    preflight_for_tests(root, proc).context("migration apply preflight")?;
    Ok(ApplyRefusal::HandlerBlocked(HANDLER_BLOCKERS.to_vec()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn ctx_root(tag: &str) -> std::path::PathBuf {
        crate::testutil::tmpdir(tag)
    }

    fn fake_proc(base: &Path, exe: &str, comm: &str, root: &Path) -> std::path::PathBuf {
        let proc = base.join("proc");
        std::fs::create_dir_all(proc.join("1")).unwrap();
        std::fs::create_dir_all(proc.join("self")).unwrap();
        symlink(exe, proc.join("1/exe")).unwrap();
        std::fs::write(proc.join("1/comm"), format!("{comm}\n")).unwrap();
        symlink(root, proc.join("1/root")).unwrap();
        symlink(root, proc.join("self/root")).unwrap();
        proc
    }

    fn seed_profile(root: &Path) {
        let path = root.join("usr/share/nawa/system-profile.toml");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            "schema = 1\n[providers]\ninit-system = \"systemd\"\n",
        )
        .unwrap();
    }

    #[test]
    fn the_builtin_definition_is_typed_and_bound_to_the_rebooting_handler() {
        let d = builtin_definition();
        assert_eq!(d.id, "faseela-runit-to-systemd-v1");
        assert_eq!(d.from.to_key(), "init-system=runit");
        assert_eq!(d.to.to_key(), "init-system=systemd");
        assert!(d.requires_reboot);
    }

    #[test]
    fn preflight_refuses_an_offline_root_before_any_mutation() {
        let root = ctx_root("migrate-apply-offline");
        let error = preflight(&root, Path::new("/proc")).unwrap_err();
        assert!(format!("{error:#}").contains("offline root"), "{error:#}");
        assert!(!root.join("var/lib/nawa").exists());
    }

    #[test]
    fn preflight_refuses_a_container_context() {
        let base = ctx_root("migrate-apply-container");
        let root = base.join("root");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join(".dockerenv"), b"").unwrap();
        let proc = fake_proc(&base, "/usr/bin/runit", "runit", &root);
        let error = preflight(&root, &proc).unwrap_err();
        assert!(format!("{error:#}").contains("container"), "{error:#}");
    }

    #[test]
    fn preflight_refuses_a_live_host_that_is_not_running_the_source_init() {
        let base = ctx_root("migrate-apply-wrong-init");
        let root = base.join("root");
        std::fs::create_dir_all(&root).unwrap();
        seed_profile(&root);
        let proc = fake_proc(&base, "/usr/lib/systemd/systemd", "systemd", &root);
        let error = preflight_for_tests(&root, &proc).unwrap_err();
        assert!(format!("{error:#}").contains("running init is systemd"), "{error:#}");
    }

    #[test]
    fn preflight_refuses_a_pid1_identity_that_is_only_indicated() {
        let base = ctx_root("migrate-apply-indicated");
        let root = base.join("root");
        std::fs::create_dir_all(&root).unwrap();
        seed_profile(&root);
        let proc = fake_proc(&base, "/usr/bin/runit", "runit", &root);
        // drop the comm evidence: the exe alone only indicates runit
        std::fs::remove_file(proc.join("1/comm")).unwrap();
        let error = preflight_for_tests(&root, &proc).unwrap_err();
        assert!(
            format!("{error:#}").contains("only indicated"),
            "{error:#}"
        );
    }

    #[test]
    fn preflight_refuses_when_the_target_init_is_not_declared() {
        let base = ctx_root("migrate-apply-no-profile");
        let root = base.join("root");
        std::fs::create_dir_all(&root).unwrap();
        let proc = fake_proc(&base, "/usr/bin/runit", "runit", &root);
        let error = preflight_for_tests(&root, &proc).unwrap_err();
        assert!(format!("{error:#}").contains("no system profile"), "{error:#}");
    }

    #[test]
    fn preflight_accepts_a_verified_runit_host_with_a_systemd_target_profile() {
        let base = ctx_root("migrate-apply-ok");
        let root = base.join("root");
        std::fs::create_dir_all(&root).unwrap();
        seed_profile(&root);
        let proc = fake_proc(&base, "/usr/bin/runit", "runit", &root);
        preflight_for_tests(&root, &proc).expect("all preflight facts hold");
    }

    #[test]
    fn explicit_authorization_must_name_the_exact_migration_and_boundaries() {
        let d = builtin_definition();
        assert!(MigrationApproval::explicit(
            &d.id,
            &d.from,
            &d.to,
            true,
            true
        )
        .is_ok());
        assert!(MigrationApproval::explicit("other", &d.from, &d.to, true, true).is_err());
        assert!(MigrationApproval::explicit(&d.id, &d.from, &d.to, false, true).is_err());
    }

    #[test]
    fn a_generic_yes_cannot_authorize_apply_and_a_blocked_handler_is_reported_honestly() {
        let base = ctx_root("migrate-apply-refusal");
        let root = base.join("root");
        std::fs::create_dir_all(&root).unwrap();
        seed_profile(&root);
        let proc = fake_proc(&base, "/usr/bin/runit", "runit", &root);
        let d = builtin_definition();
        let approval =
            MigrationApproval::explicit(&d.id, &d.from, &d.to, true, true).unwrap();
        match apply_for_tests(&root, &proc, approval).unwrap() {
            ApplyRefusal::HandlerBlocked(blockers) => {
                assert!(blockers.iter().any(|b| {
                    b.contains("B1") && b.contains("BIOS+UEFI") && b.contains("/proc/1/comm=systemd")
                }));
                assert!(blockers.iter().any(|b| {
                    b.contains("B2") && b.contains("/usr/bin/init") && b.contains("co-ownership")
                }));
                assert!(blockers.iter().any(|b| {
                    b.contains("B4") && b.contains("administrator override")
                }));
                assert!(blockers.iter().any(|b| b.contains("B5")));
                assert!(blockers.iter().any(|b| b.contains("B6")));
                assert!(blockers.iter().any(|b| {
                    b.contains("B7") && b.contains("not a shell sandbox")
                }));
            }
        }
        assert!(
            !root.join("var/lib/nawa").exists(),
            "a refused apply creates no record, gate or package change"
        );
    }

    #[test]
    fn plan_is_read_only_and_states_the_rollback_limits() {
        let root = ctx_root("migrate-plan-readonly");
        let migration = plan(&root, Path::new("/proc"));
        assert_eq!(migration.definition.id, "faseela-runit-to-systemd-v1");
        assert!(migration.rollback_limits.iter().any(|r| r.contains("no automatic rollback")));
        assert!(!root.join("var/lib/nawa").exists(), "planning persists nothing");
    }
}
