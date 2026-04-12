// ===========================================================================
// LogFileOutput — writes agent output to a log file.
//
// Used by background agents spawned via `/loop`.  Each agent gets its own
// log file so the user can `tail -f` it for real-time observability.
// ===========================================================================

use std::io::Write;
use std::path::{Path, PathBuf};

use crate::controller::Output;
use crate::error::DysonError;
use crate::tool::ToolOutput;

/// An `Output` implementation that appends to a log file.
///
/// Text deltas are written verbatim, tool events are written as bracketed
/// markers (e.g. `[Tool: bash]`), and errors are prefixed with `[Error]`.
pub struct LogFileOutput {
    file: std::io::BufWriter<std::fs::File>,
}

impl LogFileOutput {
    /// Create a new log output, creating the parent directory if needed.
    pub fn create(path: &Path) -> Result<Self, DysonError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                DysonError::Io(std::io::Error::new(
                    e.kind(),
                    format!("cannot create log directory {}: {e}", parent.display()),
                ))
            })?;
        }
        let file = std::fs::File::create(path).map_err(|e| {
            DysonError::Io(std::io::Error::new(
                e.kind(),
                format!("cannot create log file {}: {e}", path.display()),
            ))
        })?;
        Ok(Self {
            file: std::io::BufWriter::new(file),
        })
    }

    /// Return the default log directory (`~/.dyson/agents/`).
    fn default_log_dir() -> PathBuf {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
        PathBuf::from(home).join(".dyson").join("agents")
    }

    /// Return the log path for a given agent ID.
    pub fn log_path_for(id: u64) -> PathBuf {
        Self::default_log_dir().join(format!("{id}.log"))
    }
}

impl Output for LogFileOutput {
    fn text_delta(&mut self, text: &str) -> Result<(), DysonError> {
        write!(self.file, "{text}")?;
        Ok(())
    }

    fn tool_use_start(&mut self, _id: &str, name: &str) -> Result<(), DysonError> {
        writeln!(self.file, "\n\n[Tool: {name}]")?;
        Ok(())
    }

    fn tool_use_complete(&mut self) -> Result<(), DysonError> {
        Ok(())
    }

    fn tool_result(&mut self, output: &ToolOutput) -> Result<(), DysonError> {
        if !output.content.is_empty() {
            if output.content.len() > 500 {
                writeln!(
                    self.file,
                    "[Result: {}... ({} bytes)]",
                    &output.content[..500],
                    output.content.len(),
                )?;
            } else {
                writeln!(self.file, "[Result: {}]", output.content)?;
            }
        }
        Ok(())
    }

    fn send_file(&mut self, path: &Path) -> Result<(), DysonError> {
        writeln!(self.file, "[File: {}]", path.display())?;
        Ok(())
    }

    fn error(&mut self, error: &DysonError) -> Result<(), DysonError> {
        writeln!(self.file, "[Error: {error}]")?;
        Ok(())
    }

    fn flush(&mut self) -> Result<(), DysonError> {
        self.file.flush()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn read_log(path: &Path) -> String {
        std::fs::read_to_string(path).unwrap()
    }

    fn make_output(dir: &Path) -> (LogFileOutput, PathBuf) {
        let path = dir.join("test.log");
        let out = LogFileOutput::create(&path).unwrap();
        (out, path)
    }

    #[test]
    fn text_delta_writes_verbatim() {
        let dir = tempfile::tempdir().unwrap();
        let (mut out, path) = make_output(dir.path());
        out.text_delta("hello ").unwrap();
        out.text_delta("world").unwrap();
        out.flush().unwrap();
        assert_eq!(read_log(&path), "hello world");
    }

    #[test]
    fn tool_use_start_writes_marker() {
        let dir = tempfile::tempdir().unwrap();
        let (mut out, path) = make_output(dir.path());
        out.tool_use_start("id-1", "bash").unwrap();
        out.flush().unwrap();
        assert_eq!(read_log(&path), "\n\n[Tool: bash]\n");
    }

    #[test]
    fn tool_result_short() {
        let dir = tempfile::tempdir().unwrap();
        let (mut out, path) = make_output(dir.path());
        let output = ToolOutput {
            content: "ok".to_string(),
            is_error: false,
            metadata: None,
            files: vec![],
            checkpoints: vec![],
        };
        out.tool_result(&output).unwrap();
        out.flush().unwrap();
        assert_eq!(read_log(&path), "[Result: ok]\n");
    }

    #[test]
    fn tool_result_truncates_at_500() {
        let dir = tempfile::tempdir().unwrap();
        let (mut out, path) = make_output(dir.path());
        let long = "x".repeat(600);
        let output = ToolOutput {
            content: long,
            is_error: false,
            metadata: None,
            files: vec![],
            checkpoints: vec![],
        };
        out.tool_result(&output).unwrap();
        out.flush().unwrap();
        let log = read_log(&path);
        assert!(log.contains("... (600 bytes)"), "should truncate: {log}");
        assert!(log.starts_with("[Result: xxx"), "should start with content");
    }

    #[test]
    fn tool_result_empty_content_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let (mut out, path) = make_output(dir.path());
        let output = ToolOutput {
            content: String::new(),
            is_error: false,
            metadata: None,
            files: vec![],
            checkpoints: vec![],
        };
        out.tool_result(&output).unwrap();
        out.flush().unwrap();
        assert_eq!(read_log(&path), "");
    }

    #[test]
    fn error_writes_marker() {
        let dir = tempfile::tempdir().unwrap();
        let (mut out, path) = make_output(dir.path());
        let err = DysonError::Llm("rate limited".to_string());
        out.error(&err).unwrap();
        out.flush().unwrap();
        assert!(read_log(&path).contains("[Error:"));
    }

    #[test]
    fn send_file_writes_path() {
        let dir = tempfile::tempdir().unwrap();
        let (mut out, path) = make_output(dir.path());
        out.send_file(Path::new("/tmp/output.txt")).unwrap();
        out.flush().unwrap();
        assert_eq!(read_log(&path), "[File: /tmp/output.txt]\n");
    }

    #[test]
    fn create_makes_parent_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("a").join("b").join("test.log");
        let mut out = LogFileOutput::create(&nested).unwrap();
        out.text_delta("ok").unwrap();
        out.flush().unwrap();
        assert_eq!(read_log(&nested), "ok");
    }

    #[test]
    fn log_path_for_includes_id() {
        let path = LogFileOutput::log_path_for(42);
        assert!(path.ends_with("42.log"));
        assert!(path.to_string_lossy().contains("agents"));
    }
}
