// ===========================================================================
// HTTP controller — response builders, gzip wrapper, body readers, URL
// helpers.  The boring plumbing every route handler reaches for.
// ===========================================================================

use std::convert::Infallible;

use http_body_util::{BodyExt, Full, combinators::BoxBody};
use hyper::body::Bytes;
use hyper::{Request, Response, StatusCode};
use serde::{Deserialize, Serialize};

use super::state::HttpState;
use super::wire::AuthMode;

/// Boxed-body response type — what every route handler returns and
/// what hyper's `serve_connection` wants from the service.  Aliased
/// for terseness because it shows up in 50+ signatures.
pub(crate) type Resp = Response<BoxBody<Bytes, Infallible>>;

pub(crate) fn boxed(bytes: Bytes) -> BoxBody<Bytes, Infallible> {
    Full::new(bytes).boxed()
}

/// Tiny URL-query parser, sufficient for `?path=foo&bar=baz`.
pub(crate) fn parse_query(q: &str) -> Vec<(String, String)> {
    q.split('&')
        .filter_map(|kv| {
            let (k, v) = kv.split_once('=')?;
            Some((url_decode(k), url_decode(v)))
        })
        .collect()
}

/// Gate for path components that become filesystem names in on-disk
/// stores (`<id>.meta.json`, `<id>.body`, `<id>.bin`).  Minted IDs are
/// `a<u64>` / `f<u64>`, but the value reaches us through a URL, so an
/// attacker can submit `../etc/passwd` after `url_decode` turns `%2F`
/// into `/`.  We're strict rather than blacklist-y — anything outside
/// the minted alphabet is suspicious.
pub(crate) fn safe_store_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

pub(crate) fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hi = (bytes[i + 1] as char).to_digit(16);
                let lo = (bytes[i + 2] as char).to_digit(16);
                match (hi, lo) {
                    (Some(h), Some(l)) => {
                        out.push((h * 16 + l) as u8);
                        i += 3;
                    }
                    _ => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// True when the request's `Accept-Encoding` header advertises gzip.
/// Conservative parse — we only look for the literal token `gzip`, not
/// q-values or `*`.  That's fine: every real browser lists `gzip` plainly
/// alongside brotli/deflate.
pub(crate) fn client_accepts_gzip(headers: &hyper::header::HeaderMap) -> bool {
    headers
        .get(hyper::header::ACCEPT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .map(|v| {
            v.split(',')
                .map(|p| p.trim().split(';').next().unwrap_or("").trim())
                .any(|tok| tok.eq_ignore_ascii_case("gzip"))
        })
        .unwrap_or(false)
}

/// Content-Type prefixes worth gzipping.  woff2/png/jpeg are already
/// compressed; SSE (`text/event-stream`) is infinite-streaming so we
/// must never buffer it.  Anything not in this set passes through.
pub(crate) fn compressible_content_type(ct: &str) -> bool {
    ct.starts_with("text/html")
        || ct.starts_with("text/css")
        || ct.starts_with("text/plain")
        || ct.starts_with("text/csv")
        || ct.starts_with("application/javascript")
        || ct.starts_with("application/json")
        || ct.starts_with("image/svg+xml")
}

/// Gzip at miniz_oxide's default level.  Called on the hot path for
/// every cold-load asset fetch; the cost is bounded by what the
/// frontend bundle ships (~220 KiB worst case today) and amortised
/// against the bandwidth saved — on slow-4G a 60 % compression win
/// beats the CPU cost by an order of magnitude.
pub(crate) fn gzip_bytes(bytes: &[u8]) -> std::io::Result<Vec<u8>> {
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use std::io::Write;
    let mut enc = GzEncoder::new(
        Vec::with_capacity(bytes.len() / 2),
        Compression::default(),
    );
    enc.write_all(bytes)?;
    enc.finish()
}

/// Post-dispatch wrapper: collect the response body, compress it if the
/// client asked for gzip and the content-type is text-ish and the body
/// is big enough to be worth it.  SSE responses sail through untouched
/// because `text/event-stream` isn't in the compressible set — their
/// streaming body would otherwise be buffered forever.
pub(crate) async fn maybe_gzip(resp: Resp, accepts_gzip: bool) -> Resp {
    if !accepts_gzip {
        return resp;
    }
    let is_compressible = resp
        .headers()
        .get("Content-Type")
        .and_then(|v| v.to_str().ok())
        .map(compressible_content_type)
        .unwrap_or(false);
    if !is_compressible {
        return resp;
    }
    if resp.headers().contains_key("Content-Encoding") {
        return resp;
    }
    let (parts, body) = resp.into_parts();
    // Body is `BoxBody<Bytes, Infallible>` — collect can't fail, but we
    // match defensively anyway so a future body type can't silently
    // panic in production.
    let bytes = match body.collect().await {
        Ok(c) => c.to_bytes(),
        Err(_) => return Response::from_parts(parts, boxed(Bytes::new())),
    };
    // ~1 KiB floor: below that the gzip header overhead can make the
    // output bigger than the input, and the transfer is one TCP packet
    // either way.
    if bytes.len() < 1024 {
        return Response::from_parts(parts, boxed(bytes));
    }
    let compressed = match gzip_bytes(&bytes) {
        Ok(v) => v,
        Err(_) => return Response::from_parts(parts, boxed(bytes)),
    };
    let mut resp = Response::from_parts(parts, boxed(Bytes::from(compressed)));
    let headers = resp.headers_mut();
    headers.insert(
        hyper::header::CONTENT_ENCODING,
        hyper::header::HeaderValue::from_static("gzip"),
    );
    headers.insert(
        hyper::header::VARY,
        hyper::header::HeaderValue::from_static("Accept-Encoding"),
    );
    // Hyper computes Content-Length from the Full body, but the old
    // header (if any) lingered from the uncompressed build — drop it
    // so the new, smaller length is authoritative.
    headers.remove(hyper::header::CONTENT_LENGTH);
    resp
}

pub(crate) fn json_ok<T: Serialize>(v: &T) -> Resp {
    let bytes = serde_json::to_vec(v).unwrap_or_else(|_| b"null".to_vec());
    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "application/json")
        .body(boxed(Bytes::from(bytes)))
        .unwrap()
}

pub(crate) fn bad_request(msg: &str) -> Resp {
    let body = serde_json::json!({ "error": msg }).to_string();
    Response::builder()
        .status(StatusCode::BAD_REQUEST)
        .header("Content-Type", "application/json")
        .body(boxed(Bytes::from(body)))
        .unwrap()
}

pub(crate) fn not_found() -> Resp {
    Response::builder()
        .status(StatusCode::NOT_FOUND)
        .header("Content-Type", "application/json")
        .body(boxed(Bytes::from_static(br#"{"error":"not found"}"#)))
        .unwrap()
}

pub(crate) fn method_not_allowed() -> Resp {
    Response::builder()
        .status(StatusCode::METHOD_NOT_ALLOWED)
        .header("Content-Type", "application/json")
        .body(boxed(Bytes::from_static(
            br#"{"error":"method not allowed"}"#,
        )))
        .unwrap()
}

pub(crate) fn unauthorized(state: &HttpState) -> Resp {
    let mut builder = Response::builder()
        .status(StatusCode::UNAUTHORIZED)
        .header("Content-Type", "application/json");

    // RFC 6750 challenge.  In OIDC mode include the issuer + the
    // authorization endpoint so a client that doesn't already know
    // about /api/auth/config can still find its way to the IdP — the
    // header is the well-trodden path for non-browser clients
    // (curl wrappers, terraform providers, k6 load tests).
    let challenge = match &state.auth_mode {
        AuthMode::Bearer => Some(r#"Bearer realm="dyson", error="invalid_token""#.to_string()),
        AuthMode::Oidc {
            issuer,
            authorization_endpoint,
            ..
        } => {
            // Defense-in-depth: strip CRLF / `"` from the URLs before
            // interpolating into the challenge.  Quoting them into the
            // header without sanitisation would let a misconfigured
            // (or attacker-controlled) issuer break out of the
            // parameter and inject a sibling header.
            let safe_iss = sanitize_header_value(issuer);
            let safe_auth = sanitize_header_value(authorization_endpoint);
            Some(format!(
                r#"Bearer realm="dyson", error="invalid_token", as_uri="{safe_auth}", iss="{safe_iss}""#
            ))
        }
        AuthMode::None => None,
    };
    if let Some(c) = challenge {
        builder = builder.header("WWW-Authenticate", c);
    }
    builder
        .body(boxed(Bytes::from_static(br#"{"error":"unauthorized"}"#)))
        .unwrap()
}

/// `GET /api/auth/config` — unauthenticated discovery endpoint the SPA
/// calls before it has any credential.  Returns the auth mode plus the
/// minimum the frontend needs to bootstrap (OIDC: issuer +
/// authorization_endpoint + client_id + required_scopes; bearer: just
/// the mode tag; none: just the mode tag).
pub(crate) fn get_auth_config(state: &HttpState) -> Resp {
    json_ok(&state.auth_mode)
}

/// Produce a `HeaderMap` to feed into `auth.validate_request`,
/// folding `?access_token=` into an `Authorization: Bearer …` header
/// when the path is an SSE endpoint and no header is already present.
/// Returns `None` when the original headers are usable as-is — the
/// caller borrows `req.headers()` and pays no allocation in that
/// (overwhelmingly common) case.
pub(crate) fn auth_headers_for(
    path: &str,
    req: &Request<hyper::body::Incoming>,
) -> Option<hyper::HeaderMap> {
    if !path.ends_with("/events") || req.headers().contains_key("authorization") {
        return None;
    }
    let token = parse_query(req.uri().query()?)
        .into_iter()
        .find(|(k, _)| k == "access_token")
        .map(|(_, v)| v)
        .filter(|v| !v.is_empty())?;
    let value = format!("Bearer {token}").parse().ok()?;
    let mut headers = req.headers().clone();
    headers.insert("authorization", value);
    Some(headers)
}

pub(crate) async fn read_json<T: for<'de> Deserialize<'de>>(
    req: Request<hyper::body::Incoming>,
) -> std::result::Result<T, String> {
    let collected = req
        .collect()
        .await
        .map_err(|e| format!("body read: {e}"))?;
    let bytes = collected.to_bytes();
    serde_json::from_slice(&bytes).map_err(|e| format!("json parse: {e}"))
}

/// Like `read_json` but with a hard byte cap.  Used by upload-bearing
/// endpoints to refuse oversized requests after the body is read
/// (the Content-Length pre-check covers honest clients; this catches
/// chunked bodies that lie about length).
pub(crate) async fn read_json_capped<T: for<'de> Deserialize<'de>>(
    req: Request<hyper::body::Incoming>,
    max: usize,
) -> std::result::Result<T, String> {
    let collected = req
        .collect()
        .await
        .map_err(|e| format!("body read: {e}"))?;
    let bytes = collected.to_bytes();
    if bytes.len() > max {
        return Err(format!("body too large ({} bytes; max {max})", bytes.len()));
    }
    serde_json::from_slice(&bytes).map_err(|e| format!("json parse: {e}"))
}

/// MIME type for an agent-produced file based on extension.  Used by
/// `send_file` to label inline images vs. download attachments.
pub(crate) fn mime_for_extension(path: &std::path::Path) -> String {
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "pdf" => "application/pdf",
        "txt" | "md" | "log" => "text/plain; charset=utf-8",
        "json" => "application/json",
        "html" | "htm" => "text/html; charset=utf-8",
        "csv" => "text/csv; charset=utf-8",
        "zip" => "application/zip",
        "tar" => "application/x-tar",
        "gz" => "application/gzip",
        _ => "application/octet-stream",
    }
    .to_string()
}

/// Sanitise a title for use inside a `filename="..."` Content-Disposition
/// parameter.  Strips characters that would either break the header
/// (`\r`, `\n`, `"`) or confuse downstream shells / archivers (`/`,
/// `\\`).  Non-ASCII passes through — browsers tolerate it in quoted
/// filenames and the UI already uses it as the display title.
pub(crate) fn sanitize_filename(s: &str) -> String {
    s.chars()
        .filter(|c| !matches!(c, '"' | '\r' | '\n' | '/' | '\\'))
        .collect()
}

/// Sanitise a string value before interpolating into a header.  Strips
/// CR / LF (the bytes that delimit headers — without this an
/// attacker-controlled value would let them inject a whole second
/// header) and `"` (so the value can sit inside a quoted parameter
/// like `as_uri="…"` without escaping).  Defense-in-depth: the only
/// callers today route through values the operator configured
/// (issuer URLs in dyson.json) or the controller minted (chat ids),
/// neither of which should carry these bytes.  But `header(name, val)`
/// would silently drop the rest of the response on `\r\n` and the
/// value layer doesn't enforce this in current hyper versions, so we
/// keep our own gate.
pub(crate) fn sanitize_header_value(s: &str) -> String {
    s.chars().filter(|c| !matches!(c, '\r' | '\n' | '"')).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_store_id_accepts_minted_alphabet_only() {
        // Real ids: `f<u64>` and `a<u64>`.  Anything outside [A-Za-z0-9_-]
        // must fall through to 404 — `safe_store_id` is the only thing
        // standing between a URL-decoded `../etc/passwd` and File::open.
        assert!(safe_store_id("f1"));
        assert!(safe_store_id("a42"));
        assert!(safe_store_id("a-1"));
        assert!(safe_store_id("legacy_id_with_underscore"));
        assert!(!safe_store_id(""), "empty must be rejected");
        assert!(!safe_store_id("../etc/passwd"));
        assert!(!safe_store_id("/etc/passwd"));
        assert!(!safe_store_id("..\\nope"));
        assert!(!safe_store_id("a/b"), "slash mid-id must be rejected");
        assert!(!safe_store_id("a.b"), "dots are not in the alphabet");
        assert!(!safe_store_id("a b"), "space is rejected");
        assert!(!safe_store_id("a\nb"));
        assert!(!safe_store_id("a\0b"), "NUL byte must be rejected");
        // Unicode percent-decoded ids should not be accepted — minted
        // ids are pure ASCII.
        assert!(!safe_store_id("café"));
        // Length cap.
        let big = "a".repeat(129);
        assert!(!safe_store_id(&big), "anything > 128 chars is rejected");
        let edge = "a".repeat(128);
        assert!(safe_store_id(&edge), "exactly 128 chars is fine");
    }

    #[test]
    fn url_decoded_traversal_attempts_are_blocked_by_safe_store_id() {
        // dispatch hands the URL-decoded value to safe_store_id, so
        // walk through the decode + check pipeline that
        // `/api/files/<id>` actually performs.
        let percent_traversal = url_decode("%2F..%2Fetc%2Fpasswd");
        assert_eq!(percent_traversal, "/../etc/passwd");
        assert!(!safe_store_id(&percent_traversal));
        let plus_decoded = url_decode("a+b");
        assert!(!safe_store_id(&plus_decoded), "decoded space rejected");
    }

    #[test]
    fn parse_query_handles_empty_and_missing_value_pairs() {
        assert!(parse_query("").is_empty());
        // Bare key without `=` is dropped (we only emit (k, v) pairs).
        let q = parse_query("foo&bar=baz");
        assert_eq!(q, vec![("bar".to_string(), "baz".to_string())]);
        // Repeats are kept in order so callers can pick the last (or
        // first) per their own policy.
        let q = parse_query("k=1&k=2");
        assert_eq!(q, vec![("k".to_string(), "1".to_string()), ("k".to_string(), "2".to_string())]);
    }

    #[tokio::test]
    async fn maybe_gzip_skips_streaming_content_types() {
        // Body the SSE handler returns: text/event-stream.  Even with
        // Accept-Encoding: gzip we MUST NOT buffer-and-compress
        // because that would break streaming for the lifetime of the
        // turn.
        let resp = Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", "text/event-stream")
            .body(boxed(Bytes::from_static(b"data: hello\n\n")))
            .unwrap();
        let out = maybe_gzip(resp, true).await;
        assert!(out.headers().get("Content-Encoding").is_none());
    }

    #[tokio::test]
    async fn maybe_gzip_skips_small_bodies() {
        // Below the 1 KiB floor the header overhead can make the
        // output bigger than the input — pass through untouched.
        let body = Bytes::from_static(b"<html></html>");
        let resp = Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", "text/html")
            .body(boxed(body))
            .unwrap();
        let out = maybe_gzip(resp, true).await;
        assert!(out.headers().get("Content-Encoding").is_none());
    }

    #[tokio::test]
    async fn maybe_gzip_compresses_large_html_when_accepted() {
        let html = "<html>".repeat(1024); // ~6 KiB highly compressible
        let resp = Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", "text/html")
            .body(boxed(Bytes::from(html)))
            .unwrap();
        let out = maybe_gzip(resp, true).await;
        assert_eq!(
            out.headers().get("Content-Encoding").map(|h| h.to_str().unwrap()),
            Some("gzip"),
        );
        // Vary must be set so caches don't return gzip to a client
        // that didn't ask for it.
        assert!(out.headers().get("Vary").is_some());
    }

    #[tokio::test]
    async fn maybe_gzip_passes_through_when_not_accepted() {
        let resp = Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", "text/html")
            .body(boxed(Bytes::from("<html>".repeat(1024))))
            .unwrap();
        let out = maybe_gzip(resp, false).await;
        assert!(out.headers().get("Content-Encoding").is_none());
    }


    #[test]
    fn sanitize_filename_strips_quote_breaking_chars() {
        assert_eq!(sanitize_filename("hello.md"), "hello.md");
        assert_eq!(sanitize_filename("a\"b\rc\nd/e\\f"), "abcdef");
        // Non-ASCII is kept.
        assert_eq!(sanitize_filename("naïve.md"), "naïve.md");
    }

    #[test]
    fn mime_for_extension_handles_uppercase_and_unknown_extensions() {
        use std::path::Path;
        assert_eq!(mime_for_extension(Path::new("foo.PNG")), "image/png");
        assert_eq!(mime_for_extension(Path::new("foo.jpg")), "image/jpeg");
        assert_eq!(mime_for_extension(Path::new("foo.JPEG")), "image/jpeg");
        assert_eq!(mime_for_extension(Path::new("foo.svg")), "image/svg+xml");
        assert_eq!(mime_for_extension(Path::new("foo.pdf")), "application/pdf");
        assert_eq!(mime_for_extension(Path::new("foo.bin")), "application/octet-stream");
        assert_eq!(mime_for_extension(Path::new("noext")), "application/octet-stream");
    }

    #[test]
    fn client_accepts_gzip_parses_common_headers() {
        use hyper::header::{ACCEPT_ENCODING, HeaderMap, HeaderValue};
        fn h(v: &str) -> HeaderMap {
            let mut m = HeaderMap::new();
            m.insert(ACCEPT_ENCODING, HeaderValue::from_str(v).unwrap());
            m
        }
        // What real browsers send — gzip listed alongside other encodings.
        assert!(client_accepts_gzip(&h("gzip, deflate, br")));
        assert!(client_accepts_gzip(&h("br, gzip")));
        assert!(client_accepts_gzip(&h("gzip")));
        // Q-values and whitespace shouldn't trip us up.
        assert!(client_accepts_gzip(&h("deflate, gzip;q=0.8")));
        assert!(client_accepts_gzip(&h("GZIP")));
        // Only non-gzip encodings.
        assert!(!client_accepts_gzip(&h("br, deflate")));
        assert!(!client_accepts_gzip(&h("identity")));
        // No header at all (old clients, curl without -H).
        assert!(!client_accepts_gzip(&HeaderMap::new()));
    }

    #[test]
    fn compressible_content_type_allowlist() {
        // Text-ish: compress.  Image/svg is worth it, font/woff2 isn't
        // (already compressed).  text/event-stream MUST stay out —
        // it's the SSE channel and buffering it would deadlock.
        assert!(compressible_content_type("text/html; charset=utf-8"));
        assert!(compressible_content_type("application/javascript; charset=utf-8"));
        assert!(compressible_content_type("application/json"));
        assert!(compressible_content_type("image/svg+xml"));
        assert!(!compressible_content_type("text/event-stream"));
        assert!(!compressible_content_type("font/woff2"));
        assert!(!compressible_content_type("image/png"));
        assert!(!compressible_content_type("application/octet-stream"));
    }

    #[test]
    fn gzip_bytes_round_trips() {
        use flate2::read::GzDecoder;
        use std::io::Read;
        // The 223 KiB app bundle is the real workload — use a large
        // repetitive buffer so the compressor has room to show it
        // actually compresses (not just a no-op wrap).
        let payload = b"dyson ".repeat(10_000);
        let compressed = gzip_bytes(&payload).expect("gzip should succeed");
        assert!(compressed.len() < payload.len() / 4, "should compress repetitive text");
        let mut decoded = Vec::new();
        GzDecoder::new(&compressed[..])
            .read_to_end(&mut decoded)
            .expect("round-trip decode");
        assert_eq!(decoded, payload);
    }

    #[test]
    fn url_decode_handles_percent_and_plus() {
        assert_eq!(url_decode("foo%20bar"), "foo bar");
        assert_eq!(url_decode("a+b"), "a b");
        assert_eq!(url_decode("memory%2F2026.md"), "memory/2026.md");
        // Bad escape — pass through the literal % rather than panic.
        assert_eq!(url_decode("100%"), "100%");
    }

    #[test]
    fn parse_query_extracts_path() {
        let pairs = parse_query("path=memory%2FSOUL.md&x=1");
        assert!(pairs.iter().any(|(k, v)| k == "path" && v == "memory/SOUL.md"));
    }

    #[test]
    fn sanitize_header_value_strips_crlf_and_quote() {
        // CRLF and `"` are the bytes that would let a maliciously-shaped
        // value (issuer URL from dyson.json, chat_id from disk, etc.)
        // break out of a quoted parameter or inject a sibling header.
        assert_eq!(sanitize_header_value("normal-value"), "normal-value");
        assert_eq!(
            sanitize_header_value("https://idp.example.com\r\nX-Evil: yes"),
            "https://idp.example.comX-Evil: yes",
            "CRLF that would close the header line is stripped",
        );
        assert_eq!(
            sanitize_header_value(r#"https://idp.example.com" injected="x"#),
            "https://idp.example.com injected=x",
            "double quote that would close `as_uri=\"…\"` is stripped",
        );
        // Non-ASCII passes through — header values that aren't valid in
        // RFC 7230's visible-ASCII set are caught later by hyper's
        // HeaderValue layer.
        assert_eq!(sanitize_header_value("naïve"), "naïve");
    }
}
