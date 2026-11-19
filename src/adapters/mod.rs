//! Infrastructure adapters: the concrete I/O mechanisms Nawa uses to touch
//! the package database, archives, signatures and the filesystem/process.
//!
//! Every module here owns one mechanism and depends only on lower-level
//! primitives (or on other adapters). Use cases in `application` call these
//! through explicit paths; nothing in this layer imports presentation code.
//! Cohesive higher-level subsystems that are themselves adapters —
//! `install`, `transaction`, `repo`, `plugins`, `sandbox`, `fetch` — remain
//! their own top-level namespaces rather than being flattened here.

pub(crate) mod archive;
pub(crate) mod artifact;
pub(crate) mod atomic;
pub(crate) mod checksum;
pub(crate) mod config;
pub(crate) mod db;
pub(crate) mod fswalk;
pub(crate) mod installed_metadata;
pub(crate) mod interrupt;
pub(crate) mod lock;
pub(crate) mod migration_boot_snapshot;
pub(crate) mod migration_record;
pub(crate) mod pathutil;
pub(crate) mod sign;
pub(crate) mod sys;
pub(crate) mod unique;
