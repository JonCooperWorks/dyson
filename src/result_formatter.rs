// ===========================================================================
// Result Formatter — structured, LLM-optimized tool output formatting.
//
// Replaces raw output dumps with structured results that include summaries,
// key lines, exit codes, and truncation markers.
// ===========================================================================

use std::time::Duration;

use crate::agent::stream_handler::ToolCall;
use crate::tool::ToolOutput;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Outputs longer than this are marked as truncated.
const TRUNCATION_THRESHOLD: usize = 30_000;

/// Maximum number of key lines to extract from output.
const MAX_KEY_LINES: usize = 20;

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

    /// Important lines extracted from the output (errors, compilation messages, etc.).
    pub key_lines: Vec<String>,

    /// Inferred exit code for bash-like tools (0 = success, 127 = not found, etc.).
    pub exit_code: Option<i32>,

    /// Whether the output was too long and was truncated.
    pub truncated: bool,

    /// Whether the full (un-truncated) output is available for retrieval.
    pub full_output_available: bool,
}

impl FormattedResult {
    /// Render this result into a string suitable for the LLM's tool_result message.
    pub fn to_llm_message(&self) -> String {
        let mut parts = Vec::new();
        parts.push(self.summary.clone());

        // Include the actual output so the LLM can see command results.
        if !self.output.is_empty() {
            parts.push(self.output.clone());
        }

        if self.truncated {
            parts.push("[output truncated]".to_string());
        }

        parts.join("\n")
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
        let full_output_available = truncated;

        match call.name.as_str() {
            "bash" => self.format_bash(call, output, duration, truncated, full_output_available),
            "file_read" => {
                self.format_file_read(call, output, duration, truncated, full_output_available)
            }
            "file_write" => {
                self.format_file_write(call, output, duration, truncated, full_output_available)
            }
            _ => self.format_generic(call, output, duration, truncated, full_output_available),
        }
    }

    fn format_bash(
        &self,
        call: &ToolCall,
        output: &ToolOutput,
        duration: Duration,
        truncated: bool,
        full_output_available: bool,
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
                Some(127)
            } else {
                Some(1)
            }
        } else {
            Some(0)
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
            exit_code.unwrap_or(-1),
        );

        // Extract key lines: compilation messages, errors, warnings.
        let key_lines = extract_key_lines(&output.content);

        FormattedResult {
            summary,
            output: output.content.clone(),
            key_lines,
            exit_code,
            truncated,
            full_output_available,
        }
    }

    fn format_file_read(
        &self,
        call: &ToolCall,
        output: &ToolOutput,
        duration: Duration,
        truncated: bool,
        full_output_available: bool,
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
            key_lines: Vec::new(),
            exit_code: None,
            truncated,
            full_output_available,
        }
    }

    fn format_file_write(
        &self,
        call: &ToolCall,
        output: &ToolOutput,
        duration: Duration,
        truncated: bool,
        full_output_available: bool,
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
            key_lines: Vec::new(),
            exit_code: None,
            truncated,
            full_output_available,
        }
    }

    fn format_generic(
        &self,
        call: &ToolCall,
        output: &ToolOutput,
        duration: Duration,
        truncated: bool,
        full_output_available: bool,
    ) -> FormattedResult {
        let ms = duration.as_millis();
        let status = if output.is_error { "error" } else { "ok" };

        let summary = format!("{}: {} ({}ms)", call.name, status, ms,);

        let key_lines = extract_key_lines(&output.content);

        FormattedResult {
            summary,
            output: output.content.clone(),
            key_lines,
            exit_code: None,
            truncated,
            full_output_available,
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

/// Extract key lines from output: lines containing compilation messages,
/// errors, warnings, or other important markers.
fn extract_key_lines(content: &str) -> Vec<String> {
    let markers = [
        "Compiling",
        "Finished",
        "error",
        "warning",
        "Error",
        "Warning",
        "FAILED",
        "PASSED",
        "panic",
        "thread '",
    ];

    content
        .lines()
        .filter(|line| markers.iter().any(|m| line.contains(m)))
        .take(MAX_KEY_LINES)
        .map(|l| l.to_string())
        .collect()
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
        assert_eq!(fmt.exit_code, Some(0));
        assert!(fmt.summary.contains("342ms"));
        assert!(fmt.key_lines.iter().any(|l| l.contains("Compiling")));
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
        assert_eq!(fmt.exit_code, Some(127));
        assert!(fmt.summary.contains("failed"));
    }

    #[test]
    fn formats_file_read_with_length() {
        let f = ResultFormatter::default();
        let output = ToolOutput::success(&"x".repeat(1000));
        let fmt = f.format(
            &ToolCall::new("file_read", json!({"path": "main.rs"})),
            &output,
            Duration::from_millis(5),
        );
        assert!(fmt.summary.contains("main.rs"));
        assert!(fmt.summary.contains("1000"));
        assert!(!fmt.key_lines.iter().any(|l| l.contains("xxxx")));
    }

    #[test]
    fn marks_truncated_outputs() {
        let f = ResultFormatter::default();
        let output = ToolOutput::success(&"x".repeat(50000));
        let fmt = f.format(
            &ToolCall::new("bash", json!({"command": "cat big"})),
            &output,
            Duration::from_millis(50),
        );
        assert!(fmt.truncated);
        assert!(fmt.full_output_available);
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
