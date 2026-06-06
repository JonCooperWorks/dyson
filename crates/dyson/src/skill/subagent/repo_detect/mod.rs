// ===========================================================================
// Runtime repo detection for security_engineer specialist selection.
//
// `detect_repo` shallow-parses manifest files to identify the languages
// present in a review target and any frameworks pulled in by those
// languages' dependency lists.  The hunt stage uses this to spawn a
// dedicated specialist hunter per detected framework/language, each briefed
// with its matching reference material (`language_briefing` /
// `framework_briefing`, `include_str!`-bundled at build time) — no
// shared-prompt injection.
// ===========================================================================

mod briefings;
mod detector;
mod rules;
mod types;

pub(super) use briefings::{framework_briefing, language_briefing};
pub(super) use detector::detect_repo;
pub use types::{Detection, Framework, Language};

fn assert_send_sync<T: Send + Sync>() {}

const _: fn() = assert_send_sync::<Detection>;

#[cfg(test)]
mod tests;
