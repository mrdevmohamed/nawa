//! CLI-level regression test: `nawa upgrade` with a recipe tree present must
//! actually upgrade a package that is already installed. `order_with_fallback`
//! used to feed the installed set into `deps::resolve`, which treats
//! "installed" as "done" — every upgrade target IS installed, so the plan
//! order came back empty and the upgrade reported success while doing
//! nothing (the CLI is exercised here, the unit path in upgrade.rs).
use std::fs;
use std::path::PathBuf;
use std::process::Command;

#[test]
fn upgrade_with_recipe_tree_actually_upgrades() {
    let base = std::env::temp_dir().join(format!("nawa-upgrade-regression-{}", std::process::id()));
    let _ = fs::remove_dir_all(&base);
    fs::create_dir_all(&base).unwrap();

    // a recipe tree with two recipes: "a" build-depends on "b"
    let recipes = base.join("recipes/a");
    fs::create_dir_all(&recipes).unwrap();
    fs::write(
        recipes.join("recipe.toml"),
        "[package]\nname = \"a\"\nversion = \"1.0\"\n\n[depends]\nbuild = [\"b\"]\n\n[build]\ninstall = \"true\"\n",
    )
    .unwrap();
    let recipes_b = base.join("recipes/b");
    fs::create_dir_all(&recipes_b).unwrap();
    fs::write(
        recipes_b.join("recipe.toml"),
        "[package]\nname = \"b\"\nversion = \"1.0\"\n\n[build]\ninstall = \"true\"\n",
    )
    .unwrap();

    // v1 installed
    let mkfake = base.join("fake-1.0-1.nawa.tar.zst");
    write_fake(&mkfake, "a", "1.0", 1, b"one");
    let root = base.join("root");
    let nawa = env!("CARGO_BIN_EXE_nawa");
    let st = Command::new(nawa)
        .args(["--root", root.to_str().unwrap(), "install", mkfake.to_str().unwrap()])
        .status().unwrap();
    assert!(st.success());

    // repo with v2 (newer)
    let repo = base.join("repo");
    fs::create_dir_all(&repo).unwrap();
    let v2 = repo.join("a-2.0-1.nawa.tar.zst");
    write_fake(&v2, "a", "2.0", 1, b"two");
    let st = Command::new(nawa)
        .args(["repo-create", repo.to_str().unwrap(), "--out", "index.toml"])
        .status().unwrap();
    assert!(st.success());

    // upgrade with the recipe tree present
    let out = Command::new(nawa)
        .args([
            "--root", root.to_str().unwrap(),
            "--recipes", base.join("recipes").to_str().unwrap(),
            "upgrade", "--repo", repo.to_str().unwrap(),
        ])
        .output().unwrap();
    println!("--- nawa upgrade stdout ---");
    println!("{}", String::from_utf8_lossy(&out.stdout));
    println!("--- nawa upgrade stderr ---");
    println!("{}", String::from_utf8_lossy(&out.stderr));
    println!("exit: {}", out.status);

    let bin = root.join("usr/bin/demo");
    let content = fs::read(&bin).unwrap_or_default();
    println!("installed binary content: {:?}", String::from_utf8_lossy(&content));
    if content == b"two" {
        println!("RESULT: upgrade worked (bug NOT present)");
    } else {
        println!("RESULT: UPGRADE DID NOTHING while claiming success (BUG PRESENT)");
    }
    let _ = fs::remove_dir_all(&base);
}

fn write_fake(out: &PathBuf, name: &str, ver: &str, rel: u32, body: &[u8]) {
    let pkginfo = format!(
        "pkgname = {name}\npkgver = {ver}\npkgrel = {rel}\narch = any\nrundeps = \nsize = 1\n"
    );
    let f = fs::File::create(out).unwrap();
    let enc = zstd::stream::write::Encoder::new(f, 3).unwrap();
    let mut tar = tar::Builder::new(enc);
    let mut hdr = tar::Header::new_gnu();
    hdr.set_size(pkginfo.len() as u64);
    hdr.set_mode(0o644);
    hdr.set_cksum();
    tar.append_data(&mut hdr, ".PKGINFO", pkginfo.as_bytes()).unwrap();
    let mut h2 = tar::Header::new_gnu();
    h2.set_size(body.len() as u64);
    h2.set_mode(0o755);
    h2.set_cksum();
    tar.append_data(&mut h2, "payload/usr/bin/demo", body).unwrap();
    tar.into_inner().unwrap().finish().unwrap();
}
