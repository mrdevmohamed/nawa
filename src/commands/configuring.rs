//! `config show` and `init`.

use crate::*;

pub(crate) fn cmd_config_show(ctx: &Ctx) -> Result<()> {
    // the config resolved at startup, flags included, so sources are truthful
    let cfg = &ctx.cfg;
    let pal = Palette::auto(Stream::Stdout);
    println!(
        "{} {}",
        pal.header("nawa config"),
        pal.dim("(precedence: flag > env > nawa.toml > /etc/nawa/nawa.toml > default)")
    );
    let show = |key: &str, value: String, source: &str| {
        println!("{:<10} {:<40} {}", pal.header(key), value, pal.dim(&format!("({source})")));
    };
    show("root", ctx.root.display().to_string(), cfg.source_of("root"));
    show("recipes", ctx.recipes.display().to_string(), cfg.source_of("recipes"));
    show("builddir", ctx.builddir.display().to_string(), cfg.source_of("builddir"));
    show("profiles", ctx.profiles.display().to_string(), cfg.source_of("profiles"));
    let jobs = ctx
        .jobs
        .map(|j| j.to_string())
        .unwrap_or_else(|| "(profile `jobs`, else CPU count)".into());
    show("jobs", jobs, cfg.source_of("jobs"));
    show(
        "repo",
        ctx.repo.clone().unwrap_or_else(|| "(switched off — pass --repo to use one)".into()),
        cfg.source_of("repo"),
    );
    show("install_mode", ctx.install_mode.as_str().to_string(), cfg.source_of("install_mode"));
    if let Some(f) = &cfg.file {
        println!("{:<10} {}", pal.header("config"), f.display());
    } else {
        println!("{:<10} {}", pal.header("config"), pal.dim("(no project nawa.toml — run `nawa init` to create one)"));
    }
    match &cfg.system_file {
        Some(f) => println!("{:<10} {}", pal.header("system"), f.display()),
        None => println!(
            "{:<10} {}",
            pal.header("system"),
            pal.dim(&format!(
                "(none at {} — `nawa init --system` creates it)",
                config::system_file().display()
            ))
        ),
    }
    Ok(())
}

pub(crate) fn cmd_init(force: bool, system: bool) -> Result<()> {
    let (path, body) = if system {
        (config::system_file(), config::system_template())
    } else {
        (PathBuf::from(config::CONFIG_FILE), config::template())
    };
    if path.exists() && !force {
        bail!(
            "{} already exists (use --force to overwrite)",
            path.display()
        );
    }
    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("create {} (are you root?)", dir.display()))?;
        }
    }
    std::fs::write(&path, body).with_context(|| format!("write {}", path.display()))?;
    println!(
        "nawa: wrote {} — uncomment what you need, then check with `nawa config show`",
        path.display()
    );
    Ok(())
}
