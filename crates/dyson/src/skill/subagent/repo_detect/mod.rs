// ===========================================================================
// Runtime repo detection for security_engineer cheatsheet injection.
//
// `detect_repo` shallow-parses manifest files to identify the top two
// languages present in a review target and any frameworks pulled in by
// those languages' dependency lists.  `compose_cheatsheets` then
// concatenates the matching cheatsheet files (`include_str!`-bundled at
// build time) under a hard line cap.
//
// Why inline, not a runtime tool: the sheets are guidance the
// security_engineer should carry from the first turn.  A tool-driven
// lookup wastes a tool call and biases the model against the sheet
// (they'd read it as optional).  The cap keeps token cost bounded.
// ===========================================================================

mod cheatsheets;
mod detector;
mod rules;
mod types;

pub(super) use cheatsheets::detect_and_compose;
#[cfg(test)]
use cheatsheets::{
    MAX_CHEATSHEET_LINES, compose_cheatsheets, framework_sheet, lang_sheet, line_count,
};
#[cfg(test)]
use detector::detect_repo;
pub use types::{Detection, Framework, Language};

fn assert_send_sync<T: Send + Sync>() {}

const _: fn() = assert_send_sync::<Detection>;

#[cfg(test)]
mod tests;
