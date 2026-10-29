//! Smart terminal colors: colored output for humans, plain text for scripts.
//!
//! Colors are enabled only when ALL of these hold:
//! * the target stream is a real terminal (`std::io::IsTerminal`);
//! * `NO_COLOR` is not set in the environment (https://no-color.org);
//! * `NAWA_COLOR` is not `never` (`always` forces colors on even for pipes,
//!   `auto` is the default behaviour above).
//!
//! Every helper degrades to the plain string when colors are off, so call
//! sites never need two code paths.

use std::io::IsTerminal;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    Stdout,
    Stderr,
}

#[derive(Debug, Clone, Copy)]
pub struct Palette {
    enabled: bool,
}

impl Palette {
    /// Decide from the environment and stream type. Never fails.
    pub fn auto(stream: Stream) -> Self {
        let tty = match stream {
            Stream::Stdout => std::io::stdout().is_terminal(),
            Stream::Stderr => std::io::stderr().is_terminal(),
        };
        Palette {
            enabled: colors_enabled(tty),
        }
    }

    /// Fixed choice (tests, forced modes).
    #[cfg(test)]
    pub fn new(enabled: bool) -> Self {
        Palette { enabled }
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    fn wrap(&self, code: &str, s: &str) -> String {
        if self.enabled && !s.is_empty() {
            format!("\x1b[{code}m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    }

    /// Package / command names in user-facing listings.
    pub fn name(&self, s: &str) -> String {
        self.wrap("1", s) // bold
    }

    /// Versions and identifiers.
    pub fn ver(&self, s: &str) -> String {
        self.wrap("36", s) // cyan
    }

    /// Success / installed markers.
    pub fn ok(&self, s: &str) -> String {
        self.wrap("32", s) // green
    }

    /// Warnings.
    pub fn warn(&self, s: &str) -> String {
        self.wrap("33", s) // yellow
    }

    /// Errors.
    pub fn err(&self, s: &str) -> String {
        self.wrap("31", s) // red
    }

    /// Secondary text (reasons, dates, hints).
    pub fn dim(&self, s: &str) -> String {
        self.wrap("2", s)
    }

    /// Section headers in `info`-style output.
    pub fn header(&self, s: &str) -> String {
        self.wrap("1;39", s) // bold, default color
    }
}

/// The single decision point (unit-tested without a tty).
fn colors_enabled(is_tty: bool) -> bool {
    if std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty()) {
        return false;
    }
    match std::env::var("NAWA_COLOR")
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "always" | "yes" | "1" => true,
        "never" | "no" | "0" => false,
        // "auto", empty, or anything unrecognized: the default policy
        _ => is_tty,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // these tests mutate process-wide env vars; cargo runs a binary's tests
    // on parallel threads, so they must not overlap
    static COLOR_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn color_guard() -> std::sync::MutexGuard<'static, ()> {
        COLOR_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// set/unset one env var (caller must hold the guard)
    fn set_env(k: &str, v: Option<&str>) {
        match v {
            Some(v) => std::env::set_var(k, v),
            None => std::env::remove_var(k),
        }
    }

    #[test]
    fn tty_and_env_policy() {
        let _g = color_guard();
        set_env("NO_COLOR", None);
        set_env("NAWA_COLOR", None);
        assert!(colors_enabled(true));
        assert!(!colors_enabled(false));
    }

    #[test]
    fn no_color_wins_over_tty() {
        let _g = color_guard();
        set_env("NO_COLOR", Some("1"));
        set_env("NAWA_COLOR", None);
        assert!(!colors_enabled(true));
        // empty NO_COLOR is ignored per no-color.org
        set_env("NO_COLOR", Some(""));
        assert!(colors_enabled(true));
        set_env("NO_COLOR", None);
    }

    #[test]
    fn nawa_color_forces() {
        let _g = color_guard();
        set_env("NO_COLOR", None);
        set_env("NAWA_COLOR", Some("always"));
        assert!(colors_enabled(false), "always forces colors on a pipe");
        set_env("NAWA_COLOR", Some("never"));
        assert!(!colors_enabled(true), "never disables colors on a tty");
        set_env("NAWA_COLOR", Some("auto"));
        assert!(colors_enabled(true));
        assert!(!colors_enabled(false));
        set_env("NAWA_COLOR", None);
    }

    #[test]
    fn wrap_degrades_to_plain() {
        let off = Palette::new(false);
        assert_eq!(off.name("gzip"), "gzip");
        let on = Palette::new(true);
        assert_eq!(on.name("gzip"), "\x1b[1mgzip\x1b[0m");
        // empty strings stay empty (no stray escape sequences)
        assert_eq!(on.name(""), "");
    }
}
