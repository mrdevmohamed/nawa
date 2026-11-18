//! `plan-install`: decide what an install or update will do before anything
//! is built, fetched or installed. This is the decision core of
//! `nawa install` / `nawa update` (design §4.4: the plan use case); the
//! command handler keeps argument parsing, rendering and the execution loop,
//! and prints exactly what this module returns.
//!
//! Nothing here renders: repository notes come back as [`Notice`] values and
//! the per-package source note from [`Sourcing::pick`] as a string the
//! handler writes to stderr verbatim. Like the planner it calls, this module
//! still works over `db::Entry` and `recipe::Recipe` values — the domain
//! package model that replaces them is a later roadmap step.

use anyhow::{bail, Context, Result};
use std::collections::{BTreeMap, HashMap};

use crate::application::features::system::profile::EffectiveProfile;
use crate::config::InstallMode;
use crate::application::context::Ctx;
use crate::adapters::db::Entry;
use crate::fetch::RepoRef;
use crate::plan::{self, Why};
use crate::recipe::Recipe;
use crate::repo::RepoEntry;
use crate::upgrade::OpenRepo;

/// A message the use case decided on but did not print. The handler renders
/// these through the standard status macros (see `output::Notice::emit`), so
/// this module never touches presentation code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Notice {
    /// `nawa: note: …` on stdout
    Note(String),
    /// plain informational line on stdout
    Info(String),
}

/// Where one package of a plan comes from, decided per package.
#[derive(Debug, Clone)]
pub(crate) enum Source {
    /// build it from its recipe
    Build,
    /// install this archive from the repository (boxed: index v4 entries
    /// carry solving metadata, so the entry is larger than the build variant)
    Repo(Box<RepoEntry>),
}

/// The repository (when one is usable) plus the policy for using it.
pub(crate) struct Sourcing {
    /// `Auto` (repository when it has the package at least as new as the
    /// recipe, else build), `Repo` (strict: repository only) or `Build`
    pub(crate) mode: InstallMode,
    /// `install_mode = ask`: offer "repository or build?" on a terminal
    pub(crate) ask: bool,
    pub(crate) open: Option<OpenRepo>,
}

impl Sourcing {
    /// Always build — the default for `update` and explicit `--build`.
    pub(crate) fn build_only() -> Self {
        Sourcing {
            mode: InstallMode::Build,
            ask: false,
            open: None,
        }
    }

    /// The repository's entry for `name`, if a repository is open and
    /// usable on this machine's architecture.
    pub(crate) fn entry(&self, name: &str) -> Option<&RepoEntry> {
        let open = self.open.as_ref()?;
        let re = open.index.find(name)?;
        if crate::adapters::sys::arch_fits(&re.arch) {
            Some(re)
        } else {
            None
        }
    }

    /// Decide the source of one recipe-backed package. Returns the source
    /// and — when the repository version is installed although the recipe
    /// tree is newer — the stderr note the handler prints verbatim.
    pub(crate) fn pick(&self, rec: &Recipe) -> Result<(Source, Option<String>)> {
        if self.mode == InstallMode::Build {
            return Ok((Source::Build, None));
        }
        let name = &rec.package.name;
        let have = self.entry(name);
        let strict = self.mode == InstallMode::Repo;
        match have {
            None if strict => bail!(
                "`{name}` is not in the repository for this architecture \
                 (use --build to build it, or the default `auto` mode to fall back to a build)"
            ),
            None => Ok((Source::Build, None)),
            Some(re) => {
                let theirs = format!("{}-{}", re.version, re.rel);
                let ours = format!("{}-{}", rec.package.version, rec.package.rel);
                let older = crate::version::vercmp(&theirs, &ours) == std::cmp::Ordering::Less;
                if older && !strict {
                    // the recipe tree is newer than the repository: honour the recipe
                    Ok((Source::Build, None))
                } else {
                    let note = if older {
                        Some(format!(
                            "nawa: note: repository has {name} {theirs}, the recipe is {ours} \
                             — installing the repository version"
                        ))
                    } else {
                        None
                    };
                    Ok((Source::Repo(Box::new(re.clone())), note))
                }
            }
        }
    }
}

/// Open the configured repository according to the install mode. A repository
/// that cannot be used (offline, empty, no trusted key yet, switched off) is
/// an error only in strict `repo` mode; otherwise it is one `note:` line and
/// packages are built from source. The note lines are returned for the
/// handler to print.
pub(crate) fn open_sourcing(
    ctx: &Ctx,
    mode: InstallMode,
    repo_url: Option<&str>,
    allow_unsigned: bool,
    force: bool,
) -> Result<(Sourcing, Vec<Notice>)> {
    let ask = mode == InstallMode::Ask;
    let eff = if ask { InstallMode::Auto } else { mode };
    if eff == InstallMode::Build {
        return Ok((Sourcing::build_only(), Vec::new()));
    }
    let strict = eff == InstallMode::Repo;
    let url = match repo_url {
        Some(u) => u,
        None if strict => bail!(
            "no repository configured: pass --repo <dir-or-url>, set `repo` in nawa.toml \
             or /etc/nawa/nawa.toml, or export NAWA_REPO"
        ),
        None => {
            return Ok((
                Sourcing::build_only(),
                vec![Notice::Note(
                    "no repository configured — building from source".to_string(),
                )],
            ));
        }
    };
    let repo_ref = RepoRef::parse(url)?;
    match crate::upgrade::open_repo(&ctx.root, &repo_ref, allow_unsigned, force) {
        Ok(open) => Ok((
            Sourcing {
                mode: eff,
                ask,
                open: Some(open),
            },
            Vec::new(),
        )),
        Err(e) if strict => Err(e).with_context(|| format!("repository {url} is not usable")),
        Err(e) => Ok((
            Sourcing::build_only(),
            vec![Notice::Note(format!(
                "repository {url} is not usable ({e:#}) — building from source"
            ))],
        )),
    }
}

/// What an install or update will do, decided before anything is touched.
pub(crate) struct PlanOutcome {
    /// recipe-backed packages in build + install order: dependencies first
    pub(crate) plan: plan::Plan,
    /// repository packages without a recipe, installed last
    pub(crate) extras: Vec<RepoEntry>,
}

/// The explanatory note for the solve's init decision (§8.4: "required by:
/// target profile", with the declaring file for attribution, §5.1). Only a
/// *declared* selection is explained — an unresolved decision prints
/// nothing, because nothing was decided (§8.2: unknown is never guessed).
/// `identity` comes from the resolution (§8.2's structured result), the
/// source file from the loader's role attribution.
pub(crate) fn init_note(profile: &EffectiveProfile, init: &plan::InitSelection) -> Option<Notice> {
    let plan::InitSelection::Declared { identity } = init else {
        return None;
    };
    let source = profile.role_sources.get("init-system")?;
    Some(Notice::Note(format!(
        "target profile selects init-system={identity} (from {}) — installing does not \
         switch the init provider",
        source.display()
    )))
}

/// Explain which planned packages provide the init capability the effective
/// target profile selected (design §8.4, B3 chain §8: the solver must explain
/// provider selection). Only authenticated recipe metadata is consulted; a
/// package without `[capabilities]` is never guessed into the role. Returns
/// nothing when no profile decision was declared, so legacy behavior is
/// byte-identical.
pub(crate) fn provider_note(plan: &crate::plan::Plan) -> Option<Notice> {
    let plan::InitSelection::Declared { identity } = &plan.init else {
        return None;
    };
    use crate::domain::system::capability::{InitProvider, Provides};
    let wanted = format!("init-system={identity}");
    // The solver's candidate-based provider set, computed from authenticated
    // candidate metadata (§8.4). Recipe declarations are also inspected below
    // only to *report* ambiguous candidates, never to choose one.
    let mut providers: Vec<String> = plan.init_providers.to_vec();
    let mut ambiguous = Vec::new();
    for item in &plan.items {
        let declared = match &item.rec.capabilities {
            None => Provides::Unknown,
            Some(caps) => match crate::domain::package::metadata::normalize_provides(&caps.provides) {
                Ok(values) => Provides::parse_all(&values).unwrap_or(Provides::Unknown),
                Err(_) => Provides::Unknown,
            },
        };
        match declared.init_provider() {
            // §6.2: the exclusive init role selects exactly one declared
            // identity; several declarations are reported, never chosen.
            InitProvider::Single(cap) if cap.to_key() == wanted => {
                if !providers.iter().any(|p| p == &item.rec.package.name) {
                    providers.push(item.rec.package.name.clone());
                }
            }
            InitProvider::Ambiguous(caps) => {
                let keys = caps.iter().map(|c| c.to_key()).collect::<Vec<_>>().join(", ");
                ambiguous.push(format!("{} [{keys}]", item.rec.package.name));
            }
            _ => {}
        }
    }
    if !ambiguous.is_empty() {
        return Some(Notice::Info(format!(
            "ambiguous init-system providers in this plan: {} — not resolved by preference (§6.2)",
            ambiguous.join("; ")
        )));
    }
    if providers.is_empty() {
        Some(Notice::Info(format!(
            "no planned package declares {wanted} — provider selection remains unresolved"
        )))
    } else {
        Some(Notice::Info(format!(
            "provider of {wanted} in this plan: {} (from authenticated recipe capabilities)",
            providers.join(", ")
        )))
    }
}

/// Plan the recipe-backed part of an install and split the remaining
/// targets: repository-only packages that are current become up-to-date
/// notices, the rest become `extras`; names that are neither a recipe nor a
/// repository package go to the planner, which reports them with a
/// "did you mean" hint. `profile` is the effective target profile (§8.1
/// plan input; its init decision is explained when it declares one, §8.4).
/// Notices accumulate here and the handler prints them even when planning
/// fails, exactly as the inline code did.
#[allow(clippy::too_many_arguments)]
pub(crate) fn plan(
    targets: &[String],
    known: &BTreeMap<String, Recipe>,
    installed: &HashMap<String, Entry>,
    sourcing: &Sourcing,
    profile: &EffectiveProfile,
    why: Why,
    with_deps: bool,
    notices: &mut Vec<Notice>,
) -> Result<PlanOutcome> {
    // a name without a recipe can still be installed when the repository has
    // it (a binary-only package: no dependency resolution, no build)
    let mut recipe_targets: Vec<String> = Vec::new();
    let mut extras: Vec<RepoEntry> = Vec::new();
    for t in targets {
        if known.contains_key(t) {
            recipe_targets.push(t.clone());
            continue;
        }
        match sourcing.entry(t) {
            Some(re) => match installed.get(t) {
                Some(cur)
                    if crate::version::vercmp(
                        &format!("{}-{}", cur.version, cur.rel),
                        &format!("{}-{}", re.version, re.rel),
                    ) != std::cmp::Ordering::Less =>
                {
                    notices.push(Notice::Info(format!(
                        "{t} is already installed and up to date"
                    )));
                }
                _ => extras.push(re.clone()),
            },
            // neither a recipe nor a repository package: the planner reports
            // it with a "did you mean" hint
            None => recipe_targets.push(t.clone()),
        }
    }

    let plan = plan::install_plan(
        &recipe_targets,
        installed,
        known,
        profile.profile.as_ref(),
        why,
        with_deps,
    )?;
    if let Some(note) = init_note(profile, &plan.init) {
        notices.push(note);
    }
    for name in &plan.up_to_date {
        notices.push(Notice::Info(format!(
            "{name} is already installed and up to date"
        )));
    }
    Ok(PlanOutcome { plan, extras })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo::Index;
    use crate::upgrade::ArchiveAuth;
    use std::path::PathBuf;

    fn rec(name: &str, ver: &str, run: &[&str]) -> Recipe {
        let q = |l: &[&str]| {
            l.iter()
                .map(|d| format!("{d:?}"))
                .collect::<Vec<_>>()
                .join(", ")
        };
        toml::from_str(&format!(
            "[package]\nname = \"{name}\"\nversion = \"{ver}\"\n\
             [depends]\nbuild = []\nrun = [{}]\n[build]\ninstall = \"true\"\n",
            q(run)
        ))
        .unwrap()
    }

    fn entry(name: &str, ver: &str, reason: &str, run: &[&str]) -> Entry {
        Entry {
            name: name.into(),
            version: ver.into(),
            rel: 1,
            arch: String::new(),
            reason: reason.into(),
            run: run.iter().map(|s| s.to_string()).collect(),
            install_date: 0,
            size: 0,
            artifact_sha256: String::new(),
        }
    }

    fn inst(es: Vec<Entry>) -> HashMap<String, Entry> {
        es.into_iter().map(|e| (e.name.clone(), e)).collect()
    }

    fn repo_entry(name: &str, ver: &str, rel: u32) -> RepoEntry {
        RepoEntry {
            name: name.into(),
            version: ver.into(),
            rel,
            arch: String::new(),
            file: format!("{name}.nawa.tar.zst"),
            sha256: "00".into(),
            download_url: None,
            size: 0,
            installed_size: 0,
            rundeps: None,
            provides: None,
        }
    }

    fn open_repo(entries: Vec<RepoEntry>) -> OpenRepo {
        OpenRepo {
            dir: PathBuf::new(),
            base_url: None,
            index: Index {
                generated: None,
                expires: None,
                repository: None,
                recipes: None,
                groups: None,
                packages: entries,
            },
            archive_auth: ArchiveAuth::DetachedSignature,
            index_bytes: Vec::new(),
            index_signature: None,
            signer_public: None,
            groups: None,
        }
    }

    fn infos(notices: &[Notice]) -> Vec<&str> {
        notices
            .iter()
            .filter_map(|n| match n {
                Notice::Info(text) => Some(text.as_str()),
                Notice::Note(_) => None,
            })
            .collect()
    }

    fn no_profile() -> EffectiveProfile {
        EffectiveProfile {
            profile: None,
            role_sources: BTreeMap::new(),
        }
    }

    fn profile_declaring(init: &str) -> EffectiveProfile {
        let scalars = BTreeMap::from([("schema".to_string(), "1".to_string())]);
        let providers = BTreeMap::from([("init-system".to_string(), init.to_string())]);
        let profile =
            crate::domain::system::profile::SystemProfile::from_doc(&scalars, &Some(providers))
                .unwrap();
        EffectiveProfile {
            role_sources: BTreeMap::from([(
                "init-system".to_string(),
                std::path::PathBuf::from("/etc/nawa/system-profile.toml"),
            )]),
            profile: Some(profile),
        }
    }

    #[test]
    fn the_profiles_init_decision_becomes_a_note_when_declared() {
        // §8.4's explanation: required by target profile, with the
        // declaring file — and an unresolved decision prints nothing
        let sourcing = Sourcing::build_only();
        let installed = inst(vec![]);
        let known = BTreeMap::from([("curl".to_string(), rec("curl", "1.0", &[]))]);
        let mut notices = Vec::new();
        plan(
            &["curl".to_string()],
            &known,
            &installed,
            &sourcing,
            &profile_declaring("systemd"),
            Why::Explicit,
            true,
            &mut notices,
        )
        .unwrap();
        let notes: Vec<&str> = notices
            .iter()
            .filter_map(|n| match n {
                Notice::Note(text) => Some(text.as_str()),
                Notice::Info(_) => None,
            })
            .collect();
        assert_eq!(
            notes,
            ["target profile selects init-system=systemd \
              (from /etc/nawa/system-profile.toml) — installing does not switch the init provider"],
        );

        // no profile: unresolved, no note
        let mut notices = Vec::new();
        plan(
            &["curl".to_string()],
            &known,
            &installed,
            &sourcing,
            &no_profile(),
            Why::Explicit,
            true,
            &mut notices,
        )
        .unwrap();
        assert!(notices.is_empty(), "{notices:?}");
    }

    #[test]
    fn current_repo_only_packages_become_up_to_date_notices() {
        let sourcing = Sourcing {
            mode: InstallMode::Auto,
            ask: false,
            open: Some(open_repo(vec![repo_entry("curl", "1.0", 1)])),
        };
        let installed = inst(vec![entry("curl", "1.0", "explicit", &[])]);
        let mut notices = Vec::new();
        let outcome = plan(
            &["curl".to_string()],
            &BTreeMap::new(),
            &installed,
            &sourcing,
            &no_profile(),
            Why::Explicit,
            true,
            &mut notices,
        )
        .unwrap();
        assert_eq!(
            infos(&notices),
            ["curl is already installed and up to date"]
        );
        assert!(outcome.plan.items.is_empty());
        assert!(outcome.extras.is_empty());
    }

    #[test]
    fn older_repo_only_packages_are_staged_as_extras() {
        let sourcing = Sourcing {
            mode: InstallMode::Auto,
            ask: false,
            open: Some(open_repo(vec![repo_entry("curl", "2.0", 1)])),
        };
        let installed = inst(vec![entry("curl", "1.0", "explicit", &[])]);
        let mut notices = Vec::new();
        let outcome = plan(
            &["curl".to_string()],
            &BTreeMap::new(),
            &installed,
            &sourcing,
            &no_profile(),
            Why::Explicit,
            true,
            &mut notices,
        )
        .unwrap();
        assert!(notices.is_empty());
        assert_eq!(outcome.extras.len(), 1);
        assert_eq!(outcome.extras[0].version, "2.0");
    }

    #[test]
    fn planning_failure_keeps_the_notices_already_decided() {
        // "cprl" has no recipe and no repository entry: the planner reports
        // it with a "did you mean" hint — after "curl" (repo-only, current)
        // was already decided
        let sourcing = Sourcing {
            mode: InstallMode::Auto,
            ask: false,
            open: Some(open_repo(vec![repo_entry("curl", "1.0", 1)])),
        };
        let known = BTreeMap::new();
        let installed = inst(vec![entry("curl", "1.0", "explicit", &[])]);
        let mut notices = Vec::new();
        let error = match plan(
            &["curl".to_string(), "cprl".to_string()],
            &known,
            &installed,
            &sourcing,
            &no_profile(),
            Why::Explicit,
            true,
            &mut notices,
        ) {
            Ok(_) => panic!("planning should fail for an unknown name"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("no recipe for `cprl`"));
        assert_eq!(
            infos(&notices),
            ["curl is already installed and up to date"]
        );
    }

    #[test]
    fn auto_mode_prefers_a_newer_recipe_over_the_repository() {
        let sourcing = Sourcing {
            mode: InstallMode::Auto,
            ask: false,
            open: Some(open_repo(vec![repo_entry("curl", "1.0", 1)])),
        };
        let recipe = rec("curl", "2.0", &[]);
        let (source, note) = sourcing.pick(&recipe).unwrap();
        assert!(matches!(source, Source::Build));
        assert!(note.is_none());
    }

    #[test]
    fn auto_mode_takes_the_repository_copy_and_notes_the_downgrade() {
        // strict repo mode installs the repository copy even when the recipe
        // tree is newer — with the "repository version" note on stderr
        let sourcing = Sourcing {
            mode: InstallMode::Repo,
            ask: false,
            open: Some(open_repo(vec![repo_entry("curl", "1.0", 1)])),
        };
        let recipe = rec("curl", "2.0", &[]);
        let (source, note) = sourcing.pick(&recipe).unwrap();
        assert!(matches!(source, Source::Repo(_)));
        assert_eq!(
            note.as_deref(),
            Some(
                "nawa: note: repository has curl 1.0-1, the recipe is 2.0-1 \
                 — installing the repository version"
            )
        );
    }

    #[test]
    fn strict_repo_mode_refuses_packages_the_repository_lacks() {
        let sourcing = Sourcing {
            mode: InstallMode::Repo,
            ask: false,
            open: Some(open_repo(vec![])),
        };
        let err = sourcing.pick(&rec("curl", "1.0", &[])).unwrap_err();
        assert!(err
            .to_string()
            .contains("not in the repository for this architecture"));
    }

    #[test]
    fn build_mode_never_consults_the_repository() {
        let sourcing = Sourcing::build_only();
        let (source, note) = sourcing.pick(&rec("curl", "1.0", &[])).unwrap();
        assert!(matches!(source, Source::Build));
        assert!(note.is_none());
    }

    fn ctx_with(root: &std::path::Path) -> Ctx {
        Ctx::from_config(&crate::config::Config {
            root: root.to_path_buf(),
            recipes: root.join("recipes"),
            builddir: root.join("build"),
            profiles: root.join("profiles"),
            jobs: None,
            repo: None,
            install_mode: InstallMode::Auto,
            downloads: 4,
            file: None,
            system_file: None,
            sources: BTreeMap::new(),
        })
    }

    #[test]
    fn no_repository_configured_notes_and_builds() {
        let ctx = ctx_with(&crate::testutil::tmpdir("sourcing-norepo"));
        let (sourcing, notes) = open_sourcing(&ctx, InstallMode::Auto, None, false, false).unwrap();
        assert!(sourcing.open.is_none());
        assert_eq!(
            notes,
            vec![Notice::Note(
                "no repository configured — building from source".to_string()
            )]
        );
    }

    #[test]
    fn strict_mode_without_a_repository_is_an_error() {
        let ctx = ctx_with(&crate::testutil::tmpdir("sourcing-strict"));
        let error = match open_sourcing(&ctx, InstallMode::Repo, None, false, false) {
            Ok(_) => panic!("strict mode without a repository should fail"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("no repository configured"));
    }

    #[test]
    fn an_unusable_repository_falls_back_to_a_build_with_a_note() {
        let ctx = ctx_with(&crate::testutil::tmpdir("sourcing-badrepo"));
        let missing = ctx.root.join("no-such-repo");
        let (sourcing, notes) = open_sourcing(
            &ctx,
            InstallMode::Auto,
            Some(&missing.display().to_string()),
            false,
            false,
        )
        .unwrap();
        assert!(sourcing.open.is_none());
        assert_eq!(notes.len(), 1);
        match &notes[0] {
            Notice::Note(text) => {
                assert!(text.contains("is not usable"), "unexpected note: {text}")
            }
            other => panic!("expected a Note, got {other:?}"),
        }
    }

    #[test]
    fn build_mode_opens_nothing() {
        let ctx = ctx_with(&crate::testutil::tmpdir("sourcing-build"));
        let (sourcing, notes) = open_sourcing(
            &ctx,
            InstallMode::Build,
            Some("https://mirror.example.com"),
            false,
            false,
        )
        .unwrap();
        assert!(sourcing.open.is_none());
        assert!(notes.is_empty());
    }

    #[test]
    fn provider_note_explains_which_planned_package_provides_the_selected_init() {
        use crate::plan::InitSelection;

        let mut plain = crate::plan::Plan::default();
        assert!(provider_note(&plain).is_none(), "no profile decision — no note");

        plain.init = InitSelection::Declared {
            identity: "systemd".to_string(),
        };
        let no_provider = provider_note(&plain).expect("a declared decision is explained");
        assert!(format!("{no_provider:?}").contains("no planned package declares"));

        let mut provider = rec("systemd", "255", &[]);
        provider.capabilities = Some(crate::recipe::model::Capabilities {
            provides: vec!["init-system=systemd".to_string()],
        });
        plain.items.push(crate::plan::Item {
            rec: provider,
            why: crate::plan::Why::Explicit,
            replaces: None,
        });
        let explained = provider_note(&plain).expect("the provider is named");
        let text = format!("{explained:?}");
        assert!(text.contains("systemd"), "{text}");
        assert!(text.contains("authenticated recipe capabilities"), "{text}");
    }
}
