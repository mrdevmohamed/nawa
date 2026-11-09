//! Lightweight error classification for branching without string matching.
//!
//! The binary uses `anyhow` throughout, so the actual error carries no
//! structured kind. This module offers a single, testable classifier that
//! callers (`upgrade`, `sync`, `diag`) can use instead of scattering
//! `contains("sha256 mismatch")` checks. The classification is heuristic
//! (it looks at io errors and well-known messages in the chain) — it is
//! for UX hints and retry decisions, never for control-flow correctness.

use std::io::ErrorKind as IoKind;

/// Coarse class of a failure, derived from the `anyhow::Error` chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Class {
    /// Filesystem / network / process I/O (`std::io::Error`).
    Io,
    /// Configuration or recipe validation (bad TOML, invalid name, …).
    Config,
    /// Trust / signature verification.
    Trust,
    /// Checksum / hash mismatch.
    Checksum,
    /// Offline mode prevented an operation.
    Offline,
    /// Interrupted by a signal (SIGINT/SIGTERM) or `ErrorKind::Interrupted`.
    Interrupted,
    /// Anything else (not classified).
    Other,
}

impl Class {
    /// Classify `e` by walking its anyhow chain.
    pub fn of(e: &anyhow::Error) -> Self {
        // Collect the full message chain once (lower-cased for matching).
        let msgs: Vec<String> = e.chain().map(|c| c.to_string().to_lowercase()).collect();
        let joined = msgs.join(" ");

        // 1. Interrupted first (highest priority signal).
        if e.chain().any(|c| {
            c.downcast_ref::<std::io::Error>()
                .is_some_and(|io| io.kind() == IoKind::Interrupted)
        }) {
            return Self::Interrupted;
        }

        // 2. Checksum / trust / offline / config via message hints.
        for m in &msgs {
            if m.contains("sha256 mismatch") || m.contains("checksum") || m.contains("hash mismatch") {
                return Self::Checksum;
            }
            if m.contains("offline") {
                return Self::Offline;
            }
            if m.contains("signature") || m.contains("trust") || m.contains("untrusted") {
                return Self::Trust;
            }
            if m.contains("config") || m.contains("invalid recipe") || m.contains("recipe not found") {
                return Self::Config;
            }
        }

        // 3. Any io error in the chain → Io.
        if e.chain().any(|c| c.downcast_ref::<std::io::Error>().is_some()) {
            return Self::Io;
        }

        let _ = joined;
        Self::Other
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[allow(unused_imports)]
    use anyhow::{anyhow, Context};

    #[test]
    fn classify_io() {
        let e = std::io::Error::new(std::io::ErrorKind::NotFound, "missing").into();
        assert_eq!(Class::of(&e), Class::Io);
    }

    #[test]
    fn classify_interrupted_beats_io() {
        let e = std::io::Error::new(std::io::ErrorKind::Interrupted, "closed").into();
        assert_eq!(Class::of(&e), Class::Interrupted);
    }

    #[test]
    fn classify_checksum_message() {
        let e = anyhow!("sha256 mismatch for foo");
        assert_eq!(Class::of(&e), Class::Checksum);
    }

    #[test]
    fn classify_offline_message() {
        let e = anyhow!("offline mode: cannot download");
        assert_eq!(Class::of(&e), Class::Offline);
    }

    #[test]
    fn classify_trust_message() {
        let e = anyhow!("signature verification failed");
        assert_eq!(Class::of(&e), Class::Trust);
    }

    #[test]
    fn classify_with_context_chain() {
        let inner = std::io::Error::new(std::io::ErrorKind::NotFound, "missing");
        let e = anyhow::Error::new(inner).context("open lock file");
        assert_eq!(Class::of(&e), Class::Io);
    }

    #[test]
    fn classify_unknown_is_other() {
        let e = anyhow!("something weird");
        assert_eq!(Class::of(&e), Class::Other);
    }
}
