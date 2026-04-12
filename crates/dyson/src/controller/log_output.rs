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
