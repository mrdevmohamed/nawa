//! One-line, colored status messages for Nawa commands.
//!
//! Status messages use one compact format. Informational and successful
//! messages go to stdout; warnings and errors go to stderr. ANSI styles are
//! adapted to the destination terminal and removed from ordinary pipes.

use anstream::{AutoStream, ColorChoice};
use anstyle::{AnsiColor, Style};
use std::fmt;
use std::io::{self, Write};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Info,
    Success,
    Note,
    Warning,
    Error,
}

impl Level {
    fn label(self) -> &'static str {
        match self {
            Self::Info => "",
            Self::Success => "ok:",
            Self::Note => "note:",
            Self::Warning => "warning:",
            Self::Error => "error:",
        }
    }

    fn style(self) -> Style {
        match self {
            Self::Info => Style::new(),
            Self::Success => Style::new().fg_color(Some(AnsiColor::Green.into())).bold(),
            Self::Note => Style::new().fg_color(Some(AnsiColor::Cyan.into())),
            Self::Warning => Style::new().fg_color(Some(AnsiColor::Yellow.into())).bold(),
            Self::Error => Style::new().fg_color(Some(AnsiColor::Red.into())).bold(),
        }
    }

    fn stderr(self) -> bool {
        matches!(self, Self::Warning | Self::Error)
    }
}

fn color_choice<S: anstream::stream::RawStream>(raw: &S) -> ColorChoice {
    if crate::color::colors_enabled(raw.is_terminal()) {
        ColorChoice::Always
    } else {
        ColorChoice::Never
    }
}

fn write_line<W: Write>(stream: &mut W, level: Level, args: fmt::Arguments<'_>) {
    let brand = Style::new().fg_color(Some(AnsiColor::Cyan.into())).bold();
    let label = level.label();
    if label.is_empty() {
        let _ = writeln!(stream, "{brand}nawa:{brand:#} {args}");
    } else {
        let style = level.style();
        let _ = writeln!(
            stream,
            "{brand}nawa:{brand:#} {style}{label}{style:#} {args}"
        );
    }
}

/// Emit a message using the default stream for its level.
pub fn emit(level: Level, args: fmt::Arguments<'_>) {
    emit_to(level, args, level.stderr());
}

/// Emit a message to stderr while retaining its semantic level.
pub fn emit_stderr(level: Level, args: fmt::Arguments<'_>) {
    emit_to(level, args, true);
}

fn emit_to(level: Level, args: fmt::Arguments<'_>, stderr: bool) {
    if stderr {
        let raw = io::stderr();
        let choice = color_choice(&raw);
        let mut stream = AutoStream::new(raw, choice);
        write_line(&mut stream, level, args, true);
    } else {
        let raw = io::stdout();
        let choice = color_choice(&raw);
        let mut stream = AutoStream::new(raw, choice);
        write_line(&mut stream, level, args, false);
    }
}

/// Write already-rendered diagnostics through an adaptive stream.
pub(crate) fn write_stderr(text: &str) {
    let raw = io::stderr();
    let choice = color_choice(&raw);
    let mut stream = AutoStream::new(raw, choice);
    let _ = stream.write_all(text.as_bytes());
}

#[macro_export]
macro_rules! nawa_info {
    ($($arg:tt)*) => {
        $crate::output::emit($crate::output::Level::Info, format_args!($($arg)*))
    };
}

#[macro_export]
macro_rules! nawa_info_stderr {
    ($($arg:tt)*) => {
        $crate::output::emit_stderr($crate::output::Level::Info, format_args!($($arg)*))
    };
}

#[macro_export]
macro_rules! nawa_success {
    ($($arg:tt)*) => {
        $crate::output::emit($crate::output::Level::Success, format_args!($($arg)*))
    };
}

#[macro_export]
macro_rules! nawa_note {
    ($($arg:tt)*) => {
        $crate::output::emit($crate::output::Level::Note, format_args!($($arg)*))
    };
}

#[macro_export]
macro_rules! nawa_note_stderr {
    ($($arg:tt)*) => {
        $crate::output::emit_stderr($crate::output::Level::Note, format_args!($($arg)*))
    };
}

#[macro_export]
macro_rules! nawa_warn {
    ($($arg:tt)*) => {
        $crate::output::emit($crate::output::Level::Warning, format_args!($($arg)*))
    };
}

#[macro_export]
macro_rules! nawa_error {
    ($($arg:tt)*) => {
        $crate::output::emit($crate::output::Level::Error, format_args!($($arg)*))
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_are_short_and_stable() {
        assert_eq!(Level::Info.label(), "");
        assert_eq!(Level::Success.label(), "ok:");
        assert_eq!(Level::Note.label(), "note:");
        assert_eq!(Level::Warning.label(), "warning:");
        assert_eq!(Level::Error.label(), "error:");
    }

    #[test]
    fn only_warnings_and_errors_default_to_stderr() {
        assert!(!Level::Info.stderr());
        assert!(!Level::Success.stderr());
        assert!(!Level::Note.stderr());
        assert!(Level::Warning.stderr());
        assert!(Level::Error.stderr());
    }

    #[test]
    fn status_styles_are_defined() {
        assert!(Level::Info.style().is_plain());
        assert!(!Level::Success.style().is_plain());
        assert!(!Level::Warning.style().is_plain());
        assert!(!Level::Error.style().is_plain());
    }
}
