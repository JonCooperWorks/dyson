// ===========================================================================
// SilentOutput — discards all output (used by side-channel LLM calls).
// ===========================================================================

use crate::controller::Output;
use crate::error::{DysonError, Result};
use crate::tool::ToolOutput;

/// A no-op output sink used for side-channel LLM calls where we want
/// tool execution but don't need to stream text to the user.
pub(super) struct SilentOutput;

impl Output for SilentOutput {
    fn text_delta(&mut self, _: &str) -> Result<()> {
        Ok(())
    }
    fn tool_use_start(&mut self, _: &str, _: &str) -> Result<()> {
        Ok(())
    }
    fn tool_use_complete(&mut self) -> Result<()> {
        Ok(())
    }
    fn tool_result(&mut self, _: &ToolOutput) -> Result<()> {
        Ok(())
    }
    fn send_file(&mut self, _: &std::path::Path) -> Result<()> {
        Ok(())
    }
    fn error(&mut self, _: &DysonError) -> Result<()> {
        Ok(())
    }
    fn flush(&mut self) -> Result<()> {
        Ok(())
    }
}
