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
}
