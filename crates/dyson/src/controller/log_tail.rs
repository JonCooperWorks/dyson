//! Read bounded tails without loading whole log files.
use std::path::PathBuf;

/// Read the last `n` lines from the most recent `~/.dyson/dyson.log.*` file.
///
/// `tracing_appender::rolling::daily` creates files like `dyson.log.2026-04-03`.
/// We pick the most recent one by sorting the matching filenames.
pub fn read_log_tail(n: usize) -> Result<String, String> {
    let home = std::env::var("HOME").map_err(|_| "HOME not set".to_string())?;
    let log_dir = PathBuf::from(home).join(".dyson");
    read_log_tail_from_dir(&log_dir, n)
}

/// Read the last `n` lines from the most recent `dyson.log*` file in `log_dir`.
pub(super) fn read_log_tail_from_dir(
    log_dir: &std::path::Path,
    n: usize,
) -> Result<String, String> {
    use std::io::{Read as _, Seek, SeekFrom};

    let mut log_files: Vec<PathBuf> = std::fs::read_dir(log_dir)
        .map_err(|e| format!("cannot read {}: {e}", log_dir.display()))?
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with("dyson.log") {
                Some(entry.path())
            } else {
                None
            }
        })
        .collect();

    if log_files.is_empty() {
        return Err("no log files found".to_string());
    }

    // Sort descending so the most recent date-suffixed file comes first.
    log_files.sort();
    log_files.reverse();

    let path = &log_files[0];
    let mut file =
        std::fs::File::open(path).map_err(|e| format!("cannot open {}: {e}", path.display()))?;

    // Read from the end of the file in chunks to find the last `n` lines,
    // avoiding loading the entire file into memory.
    let file_len = file.metadata().map_err(|e| e.to_string())?.len();

    if file_len == 0 {
        return Ok(String::new());
    }

    const CHUNK: u64 = 8192;
    // If the file does not end with '\n', the content after the last newline
    // is already one line, so start the count at 1.
    let mut newlines_found = {
        file.seek(SeekFrom::End(-1)).map_err(|e| e.to_string())?;
        let mut last = [0u8; 1];
        file.read_exact(&mut last).map_err(|e| e.to_string())?;
        if last[0] == b'\n' { 0usize } else { 1usize }
    };
    let mut tail_start = 0u64; // byte offset where the tail begins
    let mut offset = file_len;

    // Walk backwards through the file one chunk at a time.
    'outer: while offset > 0 {
        let read_start = offset.saturating_sub(CHUNK);
        let read_len = (offset - read_start) as usize;
        file.seek(SeekFrom::Start(read_start))
            .map_err(|e| e.to_string())?;

        let mut buf = vec![0u8; read_len];
        file.read_exact(&mut buf).map_err(|e| e.to_string())?;

        // Scan the chunk from back to front for newline characters.
        for i in (0..read_len).rev() {
            if buf[i] == b'\n' {
                newlines_found += 1;
                // We need n+1 newlines to capture n complete lines (the last
                // newline may be at EOF, so the +1 accounts for that).
                if newlines_found > n {
                    tail_start = read_start + (i as u64) + 1;
                    break 'outer;
                }
            }
        }

        offset = read_start;
    }

    // Read from tail_start to end of file.
    file.seek(SeekFrom::Start(tail_start))
        .map_err(|e| e.to_string())?;
    let mut result = String::new();
    file.read_to_string(&mut result)
        .map_err(|e| e.to_string())?;

    // Trim a single trailing newline so the caller gets clean lines.
    if result.ends_with('\n') {
        result.pop();
    }

    Ok(result)
}
