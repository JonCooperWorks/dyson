// ===========================================================================
// Recording output — structured event capture for the Output trait.
//
// LEARNING OVERVIEW
//
// What this file does:
//   Provides `RecordingOutput`, a reusable `Output` implementation that
//   captures every event the agent loop emits into a timestamped log.
//   Instead of writing to a terminal or sending messages to a chat service,
//   it stores each event as a structured `OutputEvent` wrapped with a
//   monotonic timestamp (`Instant`).
//
// Why it exists:
//   Before this, every test file defined its own ad-hoc mock output struct
//   (at least 4 duplicates across the codebase).  `RecordingOutput` gives
//   us a single, tested, structured implementation that can replace those
//   mocks and also serve as the basis for audit logging, replay, and
//   programmatic inspection of agent runs.
//
// How it fits in the architecture:
//
//   agent.run(input, &mut RecordingOutput)
//     │
//     ├── output.text_delta("Hello")    → RecordedEvent { TextDelta, t0 }
//     ├── output.tool_use_start(...)    → RecordedEvent { ToolUseStart, t1 }
//     ├── output.tool_result(...)       → RecordedEvent { ToolResult, t2 }
//     └── output.flush()                → RecordedEvent { Flush, t3 }
//
//   After the run, call `output.events()` to inspect what happened,
//   `output.text()` to get the concatenated text, or `output.tool_calls()`
//   to see which tools were invoked.
// ===========================================================================

use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::controller::Output;
use crate::error::DysonError;
use crate::tool::ToolOutput;

// ---------------------------------------------------------------------------
// Event types
// ---------------------------------------------------------------------------

/// One variant per `Output` method, capturing the arguments that were passed.
#[derive(Debug, Clone)]
pub enum OutputEvent {
    TextDelta { text: String },
    ToolUseStart { id: String, name: String },
    ToolUseComplete,
    ToolResult { content: String, is_error: bool },
    SendFile { path: PathBuf },
    Error { message: String },
    Flush,
}

/// An `OutputEvent` paired with the monotonic timestamp at which it occurred.
#[derive(Debug, Clone)]
pub struct RecordedEvent {
    pub timestamp: Instant,
    pub event: OutputEvent,
}

// ---------------------------------------------------------------------------
// RecordingOutput
// ---------------------------------------------------------------------------

/// A structured, reusable `Output` implementation that captures every event
/// into a timestamped log.
#[derive(Default)]
pub struct RecordingOutput {
    events: Vec<RecordedEvent>,
}

impl RecordingOutput {
    /// Create a new, empty recording.
    pub fn new() -> Self {
        Self::default()
    }

    /// Borrow the full event log.
    pub fn events(&self) -> &[RecordedEvent] {
        &self.events
    }

    /// Concatenate the text from all `TextDelta` events.
    ///
    /// This is the convenience method that replaces what `MockOutput.text`
    /// did in the various test files.
    pub fn text(&self) -> String {
        let mut result = String::new();
        for recorded in &self.events {
            if let OutputEvent::TextDelta { text } = &recorded.event {
                result.push_str(text);
            }
        }
        result
    }

    /// Return references to all `ToolUseStart` events.
    pub fn tool_calls(&self) -> Vec<&RecordedEvent> {
        self.events
            .iter()
            .filter(|e| matches!(e.event, OutputEvent::ToolUseStart { .. }))
            .collect()
    }

    /// Return `(content, is_error)` pairs for all `ToolResult` events.
    pub fn tool_results(&self) -> Vec<(&str, bool)> {
        self.events
            .iter()
            .filter_map(|e| match &e.event {
                OutputEvent::ToolResult { content, is_error } => {
                    Some((content.as_str(), *is_error))
                }
                _ => None,
            })
            .collect()
    }

    /// Return paths from all `SendFile` events.
    pub fn sent_files(&self) -> Vec<&Path> {
        self.events
            .iter()
            .filter_map(|e| match &e.event {
                OutputEvent::SendFile { path } => Some(path.as_path()),
                _ => None,
            })
            .collect()
    }

    /// Return error messages from all `Error` events.
    pub fn errors(&self) -> Vec<&str> {
        self.events
            .iter()
            .filter_map(|e| match &e.event {
                OutputEvent::Error { message } => Some(message.as_str()),
                _ => None,
            })
            .collect()
    }

    /// Discard all recorded events.
    pub fn clear(&mut self) {
        self.events.clear();
    }

    // -- internal helper ----------------------------------------------------

    fn record(&mut self, event: OutputEvent) {
        self.events.push(RecordedEvent {
            timestamp: Instant::now(),
            event,
        });
    }
}

// ---------------------------------------------------------------------------
// Output trait implementation
// ---------------------------------------------------------------------------

impl Output for RecordingOutput {
    fn text_delta(&mut self, text: &str) -> std::result::Result<(), DysonError> {
        self.record(OutputEvent::TextDelta {
            text: text.to_owned(),
        });
        Ok(())
    }

    fn tool_use_start(&mut self, id: &str, name: &str) -> std::result::Result<(), DysonError> {
        self.record(OutputEvent::ToolUseStart {
            id: id.to_owned(),
            name: name.to_owned(),
        });
        Ok(())
    }

    fn tool_use_complete(&mut self) -> std::result::Result<(), DysonError> {
        self.record(OutputEvent::ToolUseComplete);
        Ok(())
    }

    fn tool_result(&mut self, output: &ToolOutput) -> std::result::Result<(), DysonError> {
        self.record(OutputEvent::ToolResult {
            content: output.content.clone(),
            is_error: output.is_error,
        });
        Ok(())
    }

    fn send_file(&mut self, path: &Path) -> std::result::Result<(), DysonError> {
        self.record(OutputEvent::SendFile {
            path: path.to_path_buf(),
        });
        Ok(())
    }

    fn error(&mut self, error: &DysonError) -> std::result::Result<(), DysonError> {
        self.record(OutputEvent::Error {
            message: error.to_string(),
        });
        Ok(())
    }

    fn flush(&mut self) -> std::result::Result<(), DysonError> {
        self.record(OutputEvent::Flush);
        Ok(())
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Duration;

    /// Helper to build a ToolOutput for tests.
    fn tool_output(content: &str, is_error: bool) -> ToolOutput {
        ToolOutput {
            content: content.to_owned(),
            is_error,
            metadata: None,
            files: Vec::new(),
        }
    }

    #[test]
    fn events_recorded_in_order() {
        let mut rec = RecordingOutput::new();

        rec.text_delta("hello").unwrap();
        rec.tool_use_start("t1", "bash").unwrap();
        rec.tool_use_complete().unwrap();
        rec.tool_result(&tool_output("ok", false)).unwrap();
        rec.flush().unwrap();

        let events = rec.events();
        assert_eq!(events.len(), 5);
        assert!(matches!(events[0].event, OutputEvent::TextDelta { .. }));
        assert!(matches!(events[1].event, OutputEvent::ToolUseStart { .. }));
        assert!(matches!(events[2].event, OutputEvent::ToolUseComplete));
        assert!(matches!(events[3].event, OutputEvent::ToolResult { .. }));
        assert!(matches!(events[4].event, OutputEvent::Flush));
    }

    #[test]
    fn text_concatenates_deltas() {
        let mut rec = RecordingOutput::new();

        rec.text_delta("hello ").unwrap();
        rec.tool_use_start("t1", "bash").unwrap();
        rec.text_delta("world").unwrap();

        assert_eq!(rec.text(), "hello world");
    }

    #[test]
    fn tool_calls_filters() {
        let mut rec = RecordingOutput::new();

        rec.text_delta("ignored").unwrap();
        rec.tool_use_start("t1", "bash").unwrap();
        rec.text_delta("also ignored").unwrap();
        rec.tool_use_start("t2", "read_file").unwrap();
        rec.flush().unwrap();

        let calls = rec.tool_calls();
        assert_eq!(calls.len(), 2);

        if let OutputEvent::ToolUseStart { name, .. } = &calls[0].event {
            assert_eq!(name, "bash");
        } else {
            panic!("expected ToolUseStart");
        }

        if let OutputEvent::ToolUseStart { name, .. } = &calls[1].event {
            assert_eq!(name, "read_file");
        } else {
            panic!("expected ToolUseStart");
        }
    }

    #[test]
    fn clear_resets() {
        let mut rec = RecordingOutput::new();

        rec.text_delta("hello").unwrap();
        rec.tool_use_start("t1", "bash").unwrap();
        assert_eq!(rec.events().len(), 2);

        rec.clear();
        assert!(rec.events().is_empty());
        assert_eq!(rec.text(), "");
    }

    #[test]
    fn timestamps_are_monotonic() {
        let mut rec = RecordingOutput::new();

        rec.text_delta("a").unwrap();
        thread::sleep(Duration::from_millis(2));
        rec.text_delta("b").unwrap();
        thread::sleep(Duration::from_millis(2));
        rec.text_delta("c").unwrap();

        let events = rec.events();
        for window in events.windows(2) {
            assert!(
                window[1].timestamp >= window[0].timestamp,
                "timestamps must be monotonically increasing"
            );
        }
    }
}
