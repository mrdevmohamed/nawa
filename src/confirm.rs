//! Interactive confirmation for destructive operations (`remove`, `upgrade`).
//!
//! Rules:
//! * `--yes` skips the question entirely (scripts, CI);
//! * non-interactive stdin without `--yes` **refuses** to run — silently
//!   defaulting to "no" would break automation, defaulting to "yes" would
//!   be dangerous (this is how apt's `-y` contract works);
//! * the prompt goes to **stderr** so stdout stays pipe-clean;
//! * declining aborts with a non-zero exit and a clear message.

use anyhow::{bail, Result};
use std::io::{BufRead, IsTerminal, Write};

/// Ask the user. `yes=true` short-circuits.
pub fn confirm(prompt: &str, yes: bool) -> Result<()> {
    let interactive = std::io::stdin().is_terminal();
    confirm_with(prompt, yes, interactive, &mut std::io::stdin().lock())
}

/// The policy itself, with the terminal check and the input injected so the
/// tests do not depend on how `cargo test` was launched (a real terminal
/// vs. CI / a pipe).
fn confirm_with(
    prompt: &str,
    yes: bool,
    interactive: bool,
    input: &mut dyn BufRead,
) -> Result<()> {
    if yes {
        return Ok(());
    }
    if !interactive {
        bail!(
            "refusing to proceed non-interactively without --yes\n\
             (re-run with --yes, or answer the prompt in a terminal)"
        );
    }
    let mut err = std::io::stderr();
    write!(err, "{prompt} [y/N] ")?;
    err.flush()?;
    let mut line = String::new();
    input.read_line(&mut line)?;
    match line.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => Ok(()),
        _ => bail!("aborted by user — nothing was changed"),
    }
}

/// Ask a one-letter question: `allowed` are the lowercase answers, an empty
/// answer picks `default`. Without a terminal there is nobody to ask, so the
/// default is returned (unlike [`confirm`], which refuses: a *choice* between
/// two safe sources is not a destructive step). Prompt goes to stderr.
pub fn choose(prompt: &str, default: char, allowed: &[char]) -> Result<char> {
    let interactive = std::io::stdin().is_terminal();
    choose_with(prompt, default, allowed, interactive, &mut std::io::stdin().lock())
}

fn choose_with(
    prompt: &str,
    default: char,
    allowed: &[char],
    interactive: bool,
    input: &mut dyn BufRead,
) -> Result<char> {
    if !interactive {
        return Ok(default);
    }
    let mut shown: Vec<String> = Vec::new();
    for c in allowed {
        shown.push(if *c == default {
            c.to_ascii_uppercase().to_string()
        } else {
            c.to_ascii_lowercase().to_string()
        });
    }
    let mut err = std::io::stderr();
    loop {
        write!(err, "{prompt} [{}] ", shown.join("/"))?;
        err.flush()?;
        let mut line = String::new();
        if input.read_line(&mut line)? == 0 {
            bail!("no answer (end of input) — nothing was changed");
        }
        let t = line.trim().to_ascii_lowercase();
        if t.is_empty() {
            return Ok(default);
        }
        let mut chars = t.chars();
        if let (Some(c), None) = (chars.next(), chars.next()) {
            if allowed.contains(&c) {
                return Ok(c);
            }
        }
        writeln!(err, "please answer with one of: {}", shown.join(", "))?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ask(yes: bool, interactive: bool, typed: &str) -> Result<()> {
        confirm_with("proceed?", yes, interactive, &mut typed.as_bytes())
    }

    #[test]
    fn yes_flag_short_circuits() {
        // --yes wins even without a terminal and without any input
        ask(true, false, "").unwrap();
    }

    #[test]
    fn non_interactive_without_yes_refuses() {
        let err = ask(false, false, "y\n").unwrap_err();
        assert!(err.to_string().contains("--yes"));
    }

    #[test]
    fn interactive_accepts_y_and_yes_only() {
        ask(false, true, "y\n").unwrap();
        ask(false, true, " YES \n").unwrap();
        for no in ["n\n", "\n", "", "yep\n"] {
            let err = ask(false, true, no).unwrap_err();
            assert!(err.to_string().contains("aborted"), "input {no:?}");
        }
    }

    #[test]
    fn choose_defaults_without_a_terminal() {
        let c = choose_with("q?", 'r', &['r', 'b'], false, &mut "b\n".as_bytes()).unwrap();
        assert_eq!(c, 'r');
    }

    #[test]
    fn choose_reads_letter_empty_means_default_and_reprompts_on_junk() {
        let ask = |typed: &str| choose_with("q?", 'r', &['r', 'b'], true, &mut typed.as_bytes());
        assert_eq!(ask("b\n").unwrap(), 'b');
        assert_eq!(ask(" B \n").unwrap(), 'b');
        assert_eq!(ask("\n").unwrap(), 'r');
        assert_eq!(ask("x\nb\n").unwrap(), 'b', "junk is re-asked");
        assert!(ask("").is_err(), "EOF must not loop forever");
    }
}
