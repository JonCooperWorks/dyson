use crate::error::DysonError;

/// Injected after a tool call's JSON arguments were cut off by max_tokens.
/// Retrying verbatim would just re-truncate, so we redirect to small-write
/// primitives.
pub(super) const MAXTOKENS_TOOL_CALL_TRUNCATED: &str = "[Your last tool call was cut off mid-argument because the output exceeded \
     the max token limit.  Do NOT retry the same call — the JSON will \
     truncate again.  Instead: (1) use `write_file` instead of `bash` \
     heredocs to write files, or (2) split the content across multiple \
     smaller tool calls.  For writes over ~4 KB prefer `write_file`; for \
     larger files, write an initial chunk with `write_file` then append \
     subsequent chunks with `edit_file` or `bulk_edit`.]";

/// Result of attempting to stream an LLM response with retry/recovery.
pub(super) enum StreamResult {
    /// Successful response from the LLM.
    Response(crate::llm::StreamResponse),
    /// Controller requested recovery — caller should `continue` the loop.
    Recovered,
    /// Fatal error — caller should return.
    Error(DysonError),
}
