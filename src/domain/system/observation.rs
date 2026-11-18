//! Typed, source-attributed observations of the managed Linux system.
//!
//! These values contain facts gathered by application adapters. The domain
//! owns their shape and meaning, but does not discover them or perform I/O.
//! Runtime PID 1, installed candidates, desired policy and execution context
//! remain separate so an unknown or contradictory fact is never guessed.

use std::path::PathBuf;

use crate::domain::system::identity::InitKind;
use crate::domain::system::profile::SystemProfile;

/// One observation together with the source it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Evidence {
    pub(crate) source: String,
    pub(crate) value: String,
}

/// Where the managed root stands relative to this process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Context {
    /// Canonical root, PID 1 evidence statable, no container/chroot markers.
    LiveHost { evidence: Vec<Evidence> },
    /// Container markers found; runtime observations refer to the container.
    Container { evidence: Vec<Evidence> },
    /// This process's root differs from PID 1's root.
    Chroot { evidence: Vec<Evidence> },
    /// An explicit non-canonical target root.
    OfflineRoot { root: PathBuf },
    /// The environment could not be attributed.
    Unknown { reason: String },
}

/// Confidence of a runtime-init observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Confidence {
    /// Both readable sources agree.
    Proven,
    /// One source identifies the init.
    Indicated,
}

/// The observed runtime init of the managed environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RuntimeInit {
    Observed {
        kind: InitKind,
        confidence: Confidence,
        evidence: Vec<Evidence>,
    },
    Unknown {
        reason: String,
    },
}

/// What the target root has staged for init. Presence is not proof of PID 1.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct InstalledInit {
    pub(crate) candidates: Vec<InitCandidate>,
}

/// One staged init candidate found in the target root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InitCandidate {
    pub(crate) kind: InitKind,
    /// Path inside (or under) the target root.
    pub(crate) path: PathBuf,
    pub(crate) evidence: String,
}

/// Desired init declared by the effective target profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DesiredInit {
    /// Normalized identity and the file that declared it.
    Declared { identity: String, source: String },
    /// No profile declares an init provider.
    NotDeclared,
    /// A profile exists but is invalid; callers must fail closed.
    Invalid { reason: String },
}

/// A system observation is an immutable snapshot of independently sourced
/// facts. The effective profile is retained with the observation so planners
/// never need to re-read policy files after the snapshot was taken.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ObservedSystem {
    /// The root that was observed.
    pub(crate) root: PathBuf,
    pub(crate) context: Context,
    pub(crate) runtime: RuntimeInit,
    pub(crate) installed: InstalledInit,
    pub(crate) desired: DesiredInit,
    /// Validated effective profile from the same read that produced desired.
    pub(crate) effective_profile: Option<SystemProfile>,
}
