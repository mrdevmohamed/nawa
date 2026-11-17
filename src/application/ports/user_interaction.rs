//! The user-interaction port (design §4.4 step 5): everything a use case may
//! ask of the person running the command. The use case decides *what* is
//! shown and asked — including the exact wording; the presentation adapter
//! decides *how* it is rendered (colours, column widths, prompt mechanics).
//!
//! All fields are owned boxed closures so a use case can drive one
//! self-contained interaction object without knowing which adapter built it.
//! The command handlers construct them from the real prompt and rendering
//! code; unit tests construct recorders.

use anyhow::Result;

/// One row of the plan table: the display data a use case hands over. Row
/// computation (source, sizes, the why-note) lives in the use case; table
/// geometry, colours and the totals line stay in the renderer.
pub(crate) struct PlanRow {
    pub name: String,
    /// `version-rel`
    pub ver: String,
    /// `"repo"` or `"build"`
    pub tag: &'static str,
    /// download size in bytes, `0` when unknown (built from source)
    pub dl: u64,
    /// installed size in bytes, `0` when unknown
    pub inst: u64,
    /// why this package is in the plan, e.g. `explicit` or
    /// `group (replaces 1.0-1)`
    pub note: String,
}

/// The complete plan as display data, one [`PlanRow`] per package in install
/// order (dependencies first, repository-only `extras` last).
pub(crate) struct PlanView {
    pub rows: Vec<PlanRow>,
}

/// The plan table renderer: receives the display data, prints the table.
pub(crate) type ShowPlanFn = Box<dyn Fn(&PlanView) + 'static>;
/// A single-letter choice prompt: returns the chosen letter.
pub(crate) type ChooseFn = Box<dyn Fn(&str, char, &[char]) -> Result<char> + 'static>;
/// A confirmation prompt; an error means the user (or the missing terminal)
/// declined.
pub(crate) type ConfirmFn = Box<dyn Fn(&str) -> Result<()> + 'static>;

/// What a use case may ask of the user. Every field maps to one verbatim
/// output or prompt; a use case must not print through anything else.
pub(crate) struct UserInteraction {
    /// one raw line on stderr, exactly as given (source-pick notes, failure
    /// continuation notices)
    pub stderr_line: Box<dyn Fn(&str) + 'static>,
    /// an informational progress line
    pub info: Box<dyn Fn(&str) + 'static>,
    /// a warning line
    pub warn: Box<dyn Fn(&str) + 'static>,
    /// render the plan table (the header info line is an `info` call
    /// the use case makes right before this)
    pub show_plan: ShowPlanFn,
    /// offer a single-letter choice and return the chosen letter
    pub choose: ChooseFn,
    /// ask for confirmation; an error means the user (or the missing
    /// terminal) declined
    pub confirm: ConfirmFn,
}
