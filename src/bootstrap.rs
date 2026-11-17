//! The imperative shell's exit path: error rendering and process exit codes.
//! The boot sequence itself (parse → config → context → dispatch) reads
//! top-to-bottom in `main`; this module holds what happens when it ends.
//! Pure move out of `main.rs`; no behaviour change.

use anyhow::{Error, Result};

use crate::color::{Palette, Stream};
use crate::diag::{render_error, Env};
use crate::output::write_stderr;

/// `nawa: error: ...` plus indented causes and an optional hint; colors only
/// on a real terminal (see `diag::render_error`).
pub(crate) fn print_error(e: Error) {
    let p = Palette::auto(Stream::Stderr);
    let rendered = render_error(&e, &p, &Env::current());
    write_stderr(&rendered);
}

/// Render `result` on failure and exit with the shell's conventions:
/// Ctrl+C exits 130 with a calm note (the current step was rolled back or
/// finished — never left half-done), any other error prints and exits 1.
/// Success returns, so `main` ends normally and stdout is flushed as usual.
pub(crate) fn exit_on_error(result: Result<()>) {
    if let Err(e) = result {
        // Ctrl+C: the current step was rolled back or finished, never left
        // half-done. Not an error to shout about; exit like a shell would.
        if e.downcast_ref::<crate::interrupt::Interrupted>().is_some() {
            crate::nawa_note_stderr!("interrupted - system state is consistent");
            std::process::exit(130);
        }
        print_error(e);
        std::process::exit(1);
    }
}
