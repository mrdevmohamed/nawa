//! Application ports (design §4.3): the narrow contracts a use case depends
//! on, implemented by adapters. The presentation adapter (the command
//! handlers) implements [`user_interaction`]; infrastructure adapters will
//! implement storage, network and signing ports as the refactor progresses.
//!
//! Ports carry data and closures only — no rendering, no prompts, no colour.

pub(crate) mod user_interaction;
