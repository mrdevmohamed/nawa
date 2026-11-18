//! Presentation layer (design §4.1/§4.3): terminal output, diagnostics,
//! confirmation prompts and human-readable formatting. Every module here is
//! I/O-bound and renders values decided elsewhere; application/domain code
//! must never import this layer (enforced by `scripts/check-layering.py`).

pub(crate) mod color;
pub(crate) mod confirm;
pub(crate) mod diag;
pub(crate) mod error;
pub(crate) mod humanize;
pub(crate) mod output;
