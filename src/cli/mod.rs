//! Command-line definition, split by concern: `args` holds the clap
//! types (`Cli`, `Cmd` and the subcommand enums), `completions` the three
//! generated shell completion scripts. Pure move out of `cli.rs`; no
//! behaviour change. Lock policy lives in `application::lifecycle`
//! (design §4.5).

mod args;
mod completions;

pub(crate) use args::*;
pub(crate) use completions::*;
