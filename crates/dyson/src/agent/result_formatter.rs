// ===========================================================================
// Result Formatter — structured, LLM-optimized tool output formatting.
//
// Replaces raw output dumps with structured results that include summaries,
// key lines, exit codes, and truncation markers.
// ===========================================================================

use std::time::Duration;

use super::stream_handler::ToolCall;
use crate::tool::ToolOutput;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Outputs longer than this are marked as truncated.
const TRUNCATION_THRESHOLD: usize = 30_000;


// ---------------------------------------------------------------------------
// FormattedResult
// ---------------------------------------------------------------------------

/// A structured, LLM-optimized view of a tool execution result.
pub struct FormattedResult {
    /// Human-readable summary (includes timing, file paths, etc.).
    pub summary: String,

    /// The actual tool output content.  This is the primary payload — the raw
    /// stdout/stderr (for bash) or file contents (for reads).  Without this,
    /// the LLM can see *metadata* about the result but not the result itself.
    pub output: String,

    /// Whether the output was too long and was truncated.
    pub truncated: bool,
}

impl FormattedResult {
    /// Render this result into a string suitable for the LLM's tool_result message.
    pub fn to_llm_message(&self) -> String {
        let extra = if self.output.is_empty() {
            0
        } else {
            1 + self.output.len()
        } + if self.truncated {
            "\n[output truncated]".len()
        } else {
            0
        };
        let mut result = String::with_capacity(self.summary.len() + extra);
        result.push_str(&self.summary);
        if !self.output.is_empty() {
            result.push('\n');
            result.push_str(&self.output);
        }
        if self.truncated {
            result.push_str("\n[output truncated]");
        }
        result
    }
}

// ---------------------------------------------------------------------------
// ResultFormatter
// ---------------------------------------------------------------------------

/// Formats tool outputs into structured, LLM-optimized results.
pub struct ResultFormatter {
    truncation_threshold: usize,
}

impl Default for ResultFormatter {
    fn default() -> Self {
        Self {
            truncation_threshold: TRUNCATION_THRESHOLD,
        }
    }
}

impl ResultFormatter {
    /// Format a tool execution result into a structured representation.
    pub fn format(
        &self,
        call: &ToolCall,
        output: &ToolOutput,
        duration: Duration,
    ) -> FormattedResult {
        let truncated = output.content.len() > self.truncation_threshold;

        match call.name.as_str() {
            "bash" => self.format_bash(call, output, duration, truncated),
            "file_read" => self.format_file_read(call, output, duration, truncated),
            "file_write" => self.format_file_write(call, output, duration, truncated),
            _ => self.format_generic(call, output, duration, truncated),
        }
    }

    fn format_bash(
        &self,
        call: &ToolCall,
        output: &ToolOutput,
        duration: Duration,
        truncated: bool,
    ) -> FormattedResult {
        let ms = duration.as_millis();
        let command = call
            .input
            .get("command")
            .and_then(|v| v.as_str())
            .unwrap_or("<unknown>");

        let exit_code = if output.is_error {
            if output.content.contains("command not found") || output.content.contains("not found")
            {
                127
            } else {
                1
            }
        } else {
            0
        };

        let status = if output.is_error {
            "failed"
        } else {
            "completed"
        };
        let summary = format!(
            "bash: `{}` {} in {}ms (exit {})",
            truncate_str(command, 80),
            status,
            ms,
            exit_code,
        );

        FormattedResult {
            summary,
            output: output.content.clone(),
            truncated,
        }
    }

    fn format_file_read(
        &self,
        call: &ToolCall,
        output: &ToolOutput,
        duration: Duration,
        truncated: bool,
    ) -> FormattedResult {
        let path = call
            .input
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or("<unknown>");
        let len = output.content.len();
        let ms = duration.as_millis();

        let summary = format!("file_read: {} ({} bytes, {}ms)", path, len, ms,);

        FormattedResult {
            summary,
            output: output.content.clone(),
            truncated,
        }
    }

    fn format_file_write(
        &self,
        call: &ToolCall,
        output: &ToolOutput,
        duration: Duration,
        truncated: bool,
    ) -> FormattedResult {
        let path = call
            .input
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or("<unknown>");
        let ms = duration.as_millis();

        let summary = format!("file_write: {} — {} ({}ms)", path, output.content, ms,);

        FormattedResult {
            summary,
            output: output.content.clone(),
            truncated,
        }
    }

    fn format_generic(
        &self,
        call: &ToolCall,
        output: &ToolOutput,
        duration: Duration,
        truncated: bool,
    ) -> FormattedResult {
        let ms = duration.as_millis();
        let status = if output.is_error { "error" } else { "ok" };

        let summary = format!("{}: {} ({}ms)", call.name, status, ms,);

        FormattedResult {
            summary,
            output: output.content.clone(),
            truncated,
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Truncate a string to `max_len` characters, appending "..." if truncated.
fn truncate_str(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else {
        format!("{}...", &s[..max_len])
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod test_result_formatter {
    use super::*;
    use serde_json::json;

    #[test]
    fn formats_successful_bash_output() {
        let f = ResultFormatter::default();
        let output = ToolOutput::success("Compiling dyson\nFinished release");
        let fmt = f.format(
            &ToolCall::new("bash", json!({"command": "cargo build"})),
            &output,
            Duration::from_millis(342),
        );
        assert!(fmt.summary.contains("exit 0"));
        assert!(fmt.summary.contains("342ms"));
    }

    #[test]
    fn formats_failed_bash_output() {
        let f = ResultFormatter::default();
        let output = ToolOutput::error("command not found");
        let fmt = f.format(
            &ToolCall::new("bash", json!({"command": "bad"})),
            &output,
            Duration::from_millis(10),
        );
        assert!(fmt.summary.contains("exit 127"));
        assert!(fmt.summary.contains("failed"));
    }

    #[test]
    fn formats_file_read_with_length() {
        let f = ResultFormatter::default();
        let output = ToolOutput::success("x".repeat(1000));
        let fmt = f.format(
            &ToolCall::new("file_read", json!({"path": "main.rs"})),
            &output,
            Duration::from_millis(5),
        );
        assert!(fmt.summary.contains("main.rs"));
        assert!(fmt.summary.contains("1000"));
    }

    #[test]
    fn marks_truncated_outputs() {
        let f = ResultFormatter::default();
        let output = ToolOutput::success("x".repeat(50000));
        let fmt = f.format(
            &ToolCall::new("bash", json!({"command": "cat big"})),
            &output,
            Duration::from_millis(50),
        );
        assert!(fmt.truncated);
    }

    #[test]
    fn formats_file_write_confirmation() {
        let f = ResultFormatter::default();
        let output = ToolOutput::success("written");
        let fmt = f.format(
            &ToolCall::new("file_write", json!({"path": "config.json"})),
            &output,
            Duration::from_millis(15),
        );
        assert!(fmt.summary.contains("config.json"));
        assert!(fmt.summary.contains("written"));
    }
}
