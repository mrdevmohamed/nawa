//! Human-friendly rendering of top-level failures.
//!
//! `anyhow`'s alternate format (`{:#}`) joins the whole context chain with
//! ": " on a single line, so a plain permission problem turns into
//!
//!   nawa: error: open lock file /var/lib/nawa/lock: Permission denied (os error 13)
//!
//! Here the outermost message gets its own line, every underlying cause is
//! indented below it (without the noisy `(os error N)` suffix), and a few
//! well-known failure kinds get a one-line, actionable hint.

use crate::color::Palette;
use std::io::ErrorKind;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

/// What `render_error` needs to know about the process; passed in so the
/// rendering itself stays a pure, testable function.
pub struct Env<'a> {
    /// Running with effective uid 0.
    pub root: bool,
    /// `doas`/`sudo`, when one is installed.
    pub privilege_helper: Option<&'a str>,
    /// The command line after the program name, e.g. `upgrade --dry-run`.
    pub args: &'a str,
}

impl Env<'static> {
    /// Describe the current process.
    pub fn current() -> Env<'static> {
        let args: Vec<String> = std::env::args().skip(1).collect();
        Env {
            root: is_root(),
            privilege_helper: ["doas", "sudo"].into_iter().find(|h| in_path(h)),
            // leaked once at exit time: the process is about to terminate
            args: Box::leak(args.join(" ").into_boxed_str()),
        }
    }
}

pub fn is_root() -> bool {
    rustix::process::geteuid().is_root()
}

/// True when some error in the chain is an I/O error of `kind`.
pub fn has_io_kind(e: &anyhow::Error, kind: ErrorKind) -> bool {
    e.chain().any(|c| {
        c.downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == kind)
    })
}

fn in_path(program: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else { return false };
    std::env::split_paths(&path).any(|dir| is_executable(&dir.join(program)))
}

fn is_executable(p: &Path) -> bool {
    std::fs::metadata(p)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// "Permission denied (os error 13)" -> "Permission denied"
fn strip_os_error(s: &str) -> &str {
    match s.rfind(" (os error ") {
        Some(i) if s.ends_with(')') => &s[..i],
        _ => s,
    }
}

fn hint(e: &anyhow::Error, env: &Env) -> Option<String> {
    if !env.root && has_io_kind(e, ErrorKind::PermissionDenied) {
        let how = match env.privilege_helper {
            Some(h) => format!("with: {h} nawa {}", env.args).trim_end().to_string(),
            None => "as root".to_string(),
        };
        return Some(format!(
            "this command changes the system and needs root privileges; run it again {how}"
        ));
    }
    None
}

/// Multi-line, colored (when `p` is enabled) description of `e`, ending with
/// a newline.
pub fn render_error(e: &anyhow::Error, p: &Palette, env: &Env) -> String {
    let mut chain = e.chain();
    let head = chain.next().map(|c| c.to_string()).unwrap_or_default();
    let mut out = format!("nawa: {} {}\n", p.err("error:"), strip_os_error(&head));
    for cause in chain {
        out.push_str(&format!(
            "  {} {}\n",
            p.dim("caused by:"),
            strip_os_error(&cause.to_string())
        ));
    }
    if let Some(h) = hint(e, env) {
        out.push_str(&format!("nawa: {} {}\n", p.warn("hint:"), h));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Context;

    fn denied() -> anyhow::Error {
        let io = std::io::Error::from_raw_os_error(13);
        Err::<(), _>(io)
            .context("cannot open the package database lock /var/lib/nawa/lock")
            .unwrap_err()
    }

    fn env(root: bool, helper: Option<&'static str>) -> Env<'static> {
        Env { root, privilege_helper: helper, args: "upgrade" }
    }

    #[test]
    fn chain_is_split_over_lines_without_os_error_noise() {
        let text = render_error(&denied(), &Palette::new(false), &env(true, None));
        assert_eq!(
            text,
            "nawa: error: cannot open the package database lock /var/lib/nawa/lock\n  caused by: Permission denied\n"
        );
    }

    #[test]
    fn permission_denied_for_a_normal_user_suggests_the_privilege_helper() {
        let text = render_error(&denied(), &Palette::new(false), &env(false, Some("doas")));
        assert!(text.ends_with("run it again with: doas nawa upgrade\n"), "{text}");
        assert!(text.contains("nawa: hint: "));
    }

    #[test]
    fn permission_denied_without_a_helper_says_as_root() {
        let text = render_error(&denied(), &Palette::new(false), &env(false, None));
        assert!(text.ends_with("run it again as root\n"), "{text}");
    }

    #[test]
    fn root_and_unrelated_errors_get_no_hint() {
        assert!(!render_error(&denied(), &Palette::new(false), &env(true, Some("doas"))).contains("hint"));
        let other = anyhow::anyhow!("package not found: foo");
        let text = render_error(&other, &Palette::new(false), &env(false, Some("doas")));
        assert_eq!(text, "nawa: error: package not found: foo\n");
    }

    #[test]
    fn colors_only_decorate_the_labels() {
        let text = render_error(&denied(), &Palette::new(true), &env(false, Some("doas")));
        assert!(text.contains("\x1b[31merror:\x1b[0m"));
        assert!(text.contains("\x1b[2mcaused by:\x1b[0m"));
        assert!(text.contains("\x1b[33mhint:\x1b[0m"));
    }
}
