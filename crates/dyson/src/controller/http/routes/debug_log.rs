// Auth-bypassed tail of the dyson rolling log file.  Mounted at
// `GET /api/_debug/log` for forensic access from the host while
// debugging the cube → swarm `/llm` hang.  This is a debug-only
// surface that ships with the binary; it should be ripped out (or
// gated behind a build feature) once the underlying bug is closed.

use hyper::{Response, StatusCode, header};

use super::super::responses::{Resp, boxed};

const TAIL_BYTES: u64 = 200_000;

pub(super) fn tail() -> Resp {
    let dir = match std::env::var("HOME") {
        Ok(h) => std::path::PathBuf::from(h).join(".dyson"),
        Err(_) => {
            return text_resp(StatusCode::INTERNAL_SERVER_ERROR, "HOME not set");
        }
    };
    let entries = match std::fs::read_dir(&dir) {
        Ok(it) => it,
        Err(e) => {
            return text_resp(
                StatusCode::NOT_FOUND,
                &format!("read_dir {}: {e}", dir.display()),
            );
        }
    };
    let mut latest: Option<(std::path::PathBuf, std::time::SystemTime)> = None;
    for entry in entries.flatten() {
        let path = entry.path();
        let name = match path.file_name().and_then(|s| s.to_str()) {
            Some(n) => n,
            None => continue,
        };
        if !name.starts_with("dyson.log") {
            continue;
        }
        let modified = match entry.metadata().and_then(|m| m.modified()) {
            Ok(t) => t,
            Err(_) => continue,
        };
        match &latest {
            Some((_, t)) if modified <= *t => {}
            _ => latest = Some((path, modified)),
        }
    }
    let Some((path, _)) = latest else {
        return text_resp(StatusCode::NOT_FOUND, "no dyson.log* files found");
    };
    let body = match read_tail(&path, TAIL_BYTES) {
        Ok(b) => b,
        Err(e) => {
            return text_resp(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("read {}: {e}", path.display()),
            );
        }
    };
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .header("X-Dyson-Log-Path", path.display().to_string())
        .body(boxed(hyper::body::Bytes::from(body)))
        .unwrap()
}

fn read_tail(path: &std::path::Path, max: u64) -> std::io::Result<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path)?;
    let len = f.metadata()?.len();
    let start = len.saturating_sub(max);
    f.seek(SeekFrom::Start(start))?;
    let mut buf = Vec::with_capacity((len - start) as usize);
    f.read_to_end(&mut buf)?;
    Ok(buf)
}

fn text_resp(status: StatusCode, msg: &str) -> Resp {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(boxed(hyper::body::Bytes::from(msg.to_owned())))
        .unwrap()
}
