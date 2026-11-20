//! Feature-oriented use cases, grouped by user-visible feature (design
//! `system-intelligence-and-migrations.md` §4.3). Each use case decides and
//! returns data; command handlers keep argument parsing and rendering.

pub(crate) mod package;
pub(crate) mod repository;
pub(crate) mod system;
