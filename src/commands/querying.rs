//! read-only query commands: search, info, why, deps, query, files, owner.

use crate::*;

pub(crate) fn cmd_search(ctx: &Ctx, term: &str, glob: bool) -> Result<()> {
    let all = recipe::load_all(&ctx.recipes)?;
    let db = db::Db::new(&ctx.root);
    let installed: HashSet<String> = db.installed()?.into_iter().map(|e| e.name).collect();
    let pal = Palette::auto(Stream::Stdout);
    let needle = term.to_lowercase();

    let mut hits = 0usize;
    for (name, rec) in &all {
        let nl = name.to_lowercase();
        let matched = if glob {
            glob_match(&needle, &nl)
        } else {
            nl.contains(&needle)
                || rec.package.summary.to_lowercase().contains(&needle)
        };
        if !matched {
            continue;
        }
        hits += 1;
        let mark = if installed.contains(name) {
            pal.ok("*")
        } else {
            " ".to_string()
        };
        println!(
            "{mark} {} {}  {}",
            pal.name(&format!("{nl:<24}")),
            pal.ver(&format!("{:<12}", format!("{}-{}", rec.package.version, rec.package.rel))),
            pal.dim(&rec.package.summary),
        );
    }
    if hits == 0 {
        eprintln!(
            "nawa: {} no recipes match {term:?} in {}",
            pal.warn("note:"),
            ctx.recipes.display()
        );
        std::process::exit(1);
    }
    Ok(())
}

pub(crate) fn cmd_info(ctx: &Ctx, name: &str) -> Result<()> {
    let rec = recipe::load_by_name(&ctx.recipes, name)?;
    let db = db::Db::new(&ctx.root);
    let entry = db.entry(name)?;
    let pal = Palette::auto(Stream::Stdout);

    let row = |label: &str, value: String| {
        println!("{:<10} {}", pal.header(&format!("{label}:")), value);
    };

    let recipe_path = recipe::find_recipe_path(&ctx.recipes, name)?;
    row("name", pal.name(name));
    row("recipe", recipe_path.display().to_string());
    row(
        "version",
        pal.ver(&format!("{}-{}", rec.package.version, rec.package.rel)),
    );
    row("arch", rec.arch());
    if !rec.package.summary.is_empty() {
        row("summary", rec.package.summary.clone());
    }
    if !rec.package.license.is_empty() {
        row("license", rec.package.license.join(", "));
    }
    let src = if rec.package.source.is_empty() {
        "(meta package — no source)".to_string()
    } else if let Some(p) = rec.package.source.strip_prefix("local:") {
        format!("local:{p}")
    } else if rec.package.sha256.is_empty() {
        pal.warn(&format!("{} (source sha256 NOT pinned)", rec.package.source))
    } else {
        format!("{} (sha256 pinned)", rec.package.source)
    };
    row("source", src);

    let dep = |list: &[String]| {
        if list.is_empty() {
            "-".to_string()
        } else {
            list.join(", ")
        }
    };
    println!(
        "{:<10} {} build: {}  run: {}  check: {}",
        pal.header("depends:"),
        pal.dim("·"),
        pal.dim(&dep(&rec.depends.build)),
        pal.dim(&dep(&rec.depends.run)),
        pal.dim(&dep(&rec.depends.check)),
    );
    row(
        "sandbox",
        format!(
            "network={} sysfs={}",
            if rec.sandbox.network { "on" } else { "off" },
            if rec.sandbox.sysfs { "on" } else { "off" },
        ),
    );

    match entry {
        Some(e) => {
            let mut detail = format!(
                "{} ({}, installed {})",
                pal.ok(&e.version_full()),
                e.reason,
                format_date(e.install_date),
            );
            if e.size > 0 {
                detail.push_str(&format!(", {}", format_size(e.size)));
            }
            let nfiles = db.files(name)?.len();
            if nfiles > 0 {
                detail.push_str(&format!(", {nfiles} files"));
            }
            row("installed", detail);
        }
        None => row("installed", pal.dim("not installed")),
    }
    Ok(())
}

pub(crate) fn cmd_why(ctx: &Ctx, name: &str, tree: bool) -> Result<()> {
    let db = db::Db::new(&ctx.root);
    let pal = Palette::auto(Stream::Stdout);
    let entry = db.entry(name)?;
    let dependents: Vec<String> = db
        .installed()?
        .into_iter()
        .filter(|e| e.run.iter().any(|r| r == name))
        .map(|e| e.name)
        .collect();

    match &entry {
        Some(e) => {
            let reason = if e.reason == "explicit" {
                pal.ok(&e.reason)
            } else {
                pal.dim(&e.reason)
            };
            println!(
                "{} — {} ({})",
                pal.name(name),
                pal.ver(&e.version_full()),
                reason
            );
        }
        None => println!("{} — {}", pal.name(name), pal.dim("not installed")),
    }

    if dependents.is_empty() {
        println!("  nothing installed depends on it");
    } else {
        println!("  required by {} installed package(s):", dependents.len());
        for d in &dependents {
            println!("    {}", pal.name(d));
        }
    }

    if tree {
        let all = recipe::load_all(&ctx.recipes)?;
        let mut rdeps: Vec<(String, Vec<&str>)> = Vec::new();
        for (n, r) in &all {
            if n == name {
                continue;
            }
            let mut kinds: Vec<&str> = Vec::new();
            if r.depends.build.iter().any(|d| d == name) {
                kinds.push("build");
            }
            if r.depends.run.iter().any(|d| d == name) {
                kinds.push("run");
            }
            if !kinds.is_empty() {
                rdeps.push((n.clone(), kinds));
            }
        }
        if rdeps.is_empty() {
            println!("  no recipe in {} depends on it", ctx.recipes.display());
        } else {
            println!(
                "  required by {} recipe(s) in {}:",
                rdeps.len(),
                ctx.recipes.display()
            );
            for (n, kinds) in &rdeps {
                println!("    {} ({})", pal.name(n), pal.dim(&kinds.join(", ")));
            }
        }
    }

    if entry.is_none() && dependents.is_empty() {
        bail!("`{name}` is not installed and nothing depends on it");
    }
    Ok(())
}

pub(crate) fn cmd_deps(ctx: &Ctx, name: &str) -> Result<()> {
    let db = db::Db::new(&ctx.root);
    let installed: HashSet<String> =
        db.installed()?.into_iter().map(|e| e.name).collect();
    let order = deps::resolve_from_recipes(&ctx.recipes, &[name.to_string()], &installed)?;
    let pal = Palette::auto(Stream::Stdout);
    for (i, r) in order.iter().enumerate() {
        println!(
            "{:>3}. {} (build deps: {})",
            i + 1,
            pal.ver(&r.id()),
            pal.dim(&if r.depends.build.is_empty() {
                "-".into()
            } else {
                r.depends.build.join(", ")
            })
        );
    }
    Ok(())
}

pub(crate) fn cmd_deps_all(ctx: &Ctx, include_installed: bool) -> Result<()> {
    let all = recipe::load_all(&ctx.recipes)?;
    if all.is_empty() {
        bail!("no recipes found under {}", ctx.recipes.display());
    }
    let db = db::Db::new(&ctx.root);
    // default: skip what the db already records (a resumed stage2 rebuilds
    // only what is missing). --include-installed ignores the db entirely so
    // a stage3 world rebuild actually rebuilds everything — the resolver
    // treats "installed" as "done", which would otherwise empty the order.
    let installed: HashSet<String> = if include_installed {
        HashSet::new()
    } else {
        db.installed()?.into_iter().map(|e| e.name).collect()
    };
    let targets: Vec<String> = all.keys().cloned().collect();
    let order = deps::resolve(&targets, &installed, &all)?;
    for r in order {
        println!("{}", r.package.name);
    }
    Ok(())
}

pub(crate) fn cmd_query(ctx: &Ctx, pattern: Option<&str>, explicit: bool, deps: bool) -> Result<()> {
    let db = db::Db::new(&ctx.root);
    let mut pkgs = db.installed()?;
    if explicit {
        pkgs.retain(|p| p.reason != "dependency");
    } else if deps {
        pkgs.retain(|p| p.reason == "dependency");
    }
    let pal = Palette::auto(Stream::Stdout);
    if pkgs.is_empty() {
        println!("nawa: no packages installed in {}", ctx.root.display());
        return Ok(());
    }
    let needle = pattern.map(|p| p.to_lowercase());
    if pal.enabled() {
        println!(
            "{} {} {} {}",
            pal.header(&format!("{:<24}", "PACKAGE")),
            pal.header(&format!("{:<12}", "VERSION")),
            pal.header(&format!("{:<6}", "REL")),
            pal.header("REASON"),
        );
    }
    let mut shown = 0usize;
    for p in pkgs {
        if let Some(n) = &needle {
            if !p.name.to_lowercase().contains(n) {
                continue;
            }
        }
        shown += 1;
        let reason = if p.reason == "explicit" {
            pal.ok(&p.reason)
        } else {
            pal.dim(&p.reason)
        };
        if pal.enabled() {
            println!(
                "{} {} {} {}",
                pal.name(&format!("{:<24}", p.name)),
                pal.ver(&format!("{:<12}", p.version)),
                pal.dim(&format!("rel-{:<3}", p.rel)),
                reason,
            );
        } else {
            println!("{:<24} {:<12} rel-{:<3} {}", p.name, p.version, p.rel, p.reason);
        }
    }
    if shown == 0 {
        eprintln!(
            "nawa: no installed package matches {:?}",
            pattern.unwrap_or_default()
        );
        std::process::exit(1);
    }
    Ok(())
}

pub(crate) fn cmd_files(ctx: &Ctx, name: &str) -> Result<()> {
    let db = db::Db::new(&ctx.root);
    if db.entry(name)?.is_none() {
        bail!("`{name}` is not installed");
    }
    for f in db.files(name)? {
        println!("{f}");
    }
    Ok(())
}

pub(crate) fn cmd_owner(ctx: &Ctx, file: &str) -> Result<()> {
    let db = db::Db::new(&ctx.root);
    let owners = db.file_owners()?;
    match owners.get(file) {
        Some(owner) => println!("{file} is owned by {owner}"),
        None => bail!("no installed package owns {file}"),
    }
    Ok(())
}

/// Every known package name: the recipes tree plus the local db. One bare
/// name per line — the completion scripts call this dynamically.
pub(crate) fn cmd_complete_pkgs(ctx: &Ctx) -> Result<()> {
    let mut names: BTreeSet<String> = recipe::load_all(&ctx.recipes)?
        .into_keys()
        .collect();
    for e in db::Db::new(&ctx.root).installed()? {
        names.insert(e.name);
    }
    for n in names {
        println!("{n}");
    }
    Ok(())
}

// ---- presentation helpers (unit-tested below) ------------------------------

/// Case-insensitive glob with `*` (any run) and `?` (any single char),
/// anchored at both ends — `nawa search --glob`.
pub(crate) fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.to_lowercase().chars().collect();
    let t: Vec<char> = text.to_lowercase().chars().collect();
    fn rec(p: &[char], t: &[char]) -> bool {
        match (p.first(), t.first()) {
            (None, None) => true,
            (None, Some(_)) => false,
            (Some('*'), _) => rec(&p[1..], t) || (!t.is_empty() && rec(p, &t[1..])),
            (Some('?'), Some(_)) => rec(&p[1..], &t[1..]),
            (Some(&c), Some(&x)) if c == x => rec(&p[1..], &t[1..]),
            _ => false,
        }
    }
    rec(&p, &t)
}
