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
    let known = recipe::load_all(&ctx.recipes)?;
    let order = deps::resolve_target(name, &installed, &known)?;
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

/// `nawa revdep`: broken shared-library links among installed packages (and,
/// with `--unowned`, among files no package owns). Exits non-zero when
/// anything is broken, so scripts can gate on it; `--json` prints one JSON
/// document instead of the text report.
#[allow(clippy::too_many_arguments)]
pub(crate) fn cmd_revdep(
    ctx: &Ctx,
    names: &[String],
    lib_dirs: &[String],
    ignore: &[String],
    no_symlinks: bool,
    unowned: bool,
    scan_dirs: &[String],
    json: bool,
    no_suggest: bool,
) -> Result<()> {
    let db = db::Db::new(&ctx.root);
    for n in names {
        if db.entry(n)?.is_none() {
            bail!("`{n}` is not installed");
        }
    }
    let opts = revdep::Options { lib_dirs, ignore, symlinks: !no_symlinks, unowned, scan_dirs };
    let rep = revdep::scan(&ctx.root, &db, names, &opts)?;

    // which package provides what is missing? (archives built here, then recipe names)
    let missing = rep.missing_libs();
    let sug = if no_suggest || missing.is_empty() {
        revdep::Suggestions::new()
    } else {
        let recipes: Vec<String> = recipe::load_all(&ctx.recipes)
            .map(|m| m.into_keys().collect())
            .unwrap_or_default();
        let installed: std::collections::HashSet<String> =
            db.installed()?.into_iter().map(|e| e.name).collect();
        revdep::suggest(&missing, &ctx.builddir.join("out"), &recipes, &installed)
    };

    let unowned_note = if rep.unowned.is_clean() { "" } else { " and unowned files" };
    if json {
        println!("{}", rep.to_json(&sug));
        if rep.is_clean() {
            return Ok(());
        }
        bail!("broken links found in {} package(s){unowned_note}", rep.packages.len());
    }

    let pal = Palette::auto(Stream::Stdout);
    for (pkg, pr) in &rep.packages {
        println!("{}", pal.name(pkg));
        print!("{}", fmt_revdep_pkg(&pal, pr, &sug));
    }
    if !rep.unowned.is_clean() {
        println!("{}", pal.name("(not owned by any package)"));
        print!("{}", fmt_revdep_pkg(&pal, &rep.unowned, &sug));
    }
    if rep.unparseable > 0 {
        println!("{} {} ELF file(s) could not be parsed", pal.warn("warning:"), rep.unparseable);
    }
    let extra = if rep.unowned_checked {
        format!(" + {} unowned file(s)", rep.unowned_files)
    } else {
        String::new()
    };
    println!(
        "{}",
        pal.dim(&format!(
            "scanned {} ELF file(s) in {} package(s){extra}",
            rep.elf_files, rep.scanned_packages
        ))
    );
    if rep.is_clean() {
        println!("{}", pal.ok("all links OK"));
        return Ok(());
    }
    if !rep.packages.is_empty() {
        let broken: Vec<&str> = rep.packages.keys().map(|s| s.as_str()).collect();
        println!("packages to rebuild: {}", broken.join(" "));
    }
    bail!("broken links found in {} package(s){unowned_note}", rep.packages.len())
}

/// The text report of one package (or of the unowned files).
fn fmt_revdep_pkg(pal: &Palette, pr: &revdep::PkgReport, sug: &revdep::Suggestions) -> String {
    use std::fmt::Write as _;
    let mut s = String::new();
    for f in &pr.files {
        let _ = writeln!(s, "  {}", f.path);
        for m in &f.missing {
            let note = if m.wrong_arch { " (wrong architecture)" } else { "" };
            let _ = writeln!(s, "      {} {}{note}", pal.err("missing:"), m.name);
            for p in sug.get(&m.name).into_iter().flatten() {
                let _ = writeln!(s, "        {} {}", pal.dim("->"), fmt_provider(p));
            }
        }
        if let Some(i) = &f.interp {
            let _ = writeln!(s, "      {} {i}", pal.err("missing interpreter:"));
        }
    }
    for l in &pr.dead_links {
        let _ = writeln!(s, "  {l}  {}", pal.warn("(broken symlink)"));
    }
    s
}

fn fmt_provider(p: &revdep::Provider) -> String {
    let how = match p.source {
        revdep::Source::Archive => "ships it",
        revdep::Source::Recipe => "name match",
    };
    if p.installed {
        format!("{} ({how}) is installed but lacks the file — reinstall or rebuild it", p.package)
    } else {
        format!("{} ({how}) is not installed — nawa install {}", p.package, p.package)
    }
}

/// After install / update / remove: re-check what the change can have broken
/// (the packages themselves and everything that depends on them) and warn on
/// stderr. Never fails the command; `NAWA_NO_REVDEP=1` turns it off.
pub(crate) fn warn_revdep(ctx: &Ctx, changed: &[String]) {
    if std::env::var("NAWA_NO_REVDEP").is_ok_and(|v| !v.is_empty() && v != "0") {
        return;
    }
    let db = db::Db::new(&ctx.root);
    let opts = revdep::Options { lib_dirs: &[], ignore: &[], symlinks: true, unowned: false, scan_dirs: &[] };
    let checked = revdep::affected(&db, changed).and_then(|names| {
        if names.is_empty() {
            Ok(revdep::Report::default())
        } else {
            revdep::scan(&ctx.root, &db, &names, &opts)
        }
    });
    let rep = match checked {
        Ok(r) => r,
        Err(e) => {
            eprintln!("nawa: warning: revdep check failed: {e:#}");
            return;
        }
    };
    if rep.is_clean() {
        return;
    }
    let pal = Palette::auto(Stream::Stderr);
    let none = revdep::Suggestions::new();
    eprintln!("nawa: {} this change left broken shared-library links:", pal.warn("warning:"));
    for (pkg, pr) in &rep.packages {
        eprintln!("{}", pal.name(pkg));
        eprint!("{}", fmt_revdep_pkg(&pal, pr, &none));
    }
    let pkgs: Vec<&str> = rep.packages.keys().map(|s| s.as_str()).collect();
    eprintln!("nawa: details and fixes: nawa revdep {}   (silence: NAWA_NO_REVDEP=1)", pkgs.join(" "));
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
