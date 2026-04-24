// ===========================================================================
// `GET /` and `GET /assets/*` — serve the embedded React bundle.
//
// `build.rs` walks `web/dist/` at compile time and `include_bytes!`s
// every file into the binary; `assets::lookup` is the read side.
// Decoded here before the traversal check because raw `%2e%2e%2f`
// would slip past `contains("..")`.
// ===========================================================================

use hyper::body::Bytes;
use hyper::{Response, StatusCode};

use super::super::assets;
use super::super::responses::{Resp, boxed, not_found, url_decode};

pub(super) async fn serve(path: &str) -> Resp {
    // Decode before the traversal check — raw `%2e%2e%2f` would otherwise
    // slip past `contains("..")` and resolve to `..` when the OS opens
    // the file.  Reject backslashes too; we don't serve on Windows but
    // the embedded-asset lookup is case-sensitive and `\` is never a
    // legitimate URL path byte.
    let decoded = url_decode(path);
    if decoded.contains("..") || decoded.contains('\\') || decoded.contains('\0') {
        return not_found();
    }
    // The frontend is always served from the embedded bundle generated
    // by `build.rs`.  Frontend changes ride into the binary on the next
    // `cargo build` (mtime-gated, so a clean tree is a no-op); there is
    // no on-disk webroot override.
    match assets::lookup(&decoded) {
        Some((bytes, ct)) => Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", ct)
            .header("Cache-Control", "no-cache")
            .body(boxed(Bytes::from_static(bytes)))
            .unwrap(),
        None => not_found(),
    }
}
