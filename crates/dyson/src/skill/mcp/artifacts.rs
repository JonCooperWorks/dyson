//! Decode MCP image and resource content into local artifacts.
use super::protocol::McpResourceContents;
use crate::error::{DysonError, Result};
use base64::Engine as _;
use std::path::PathBuf;

pub(super) fn save_mcp_image(
    server_name: &str,
    tool_name: &str,
    idx: usize,
    mime_type: &str,
    data: &str,
) -> Result<(PathBuf, usize)> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data)
        .map_err(|e| DysonError::Mcp {
            server: server_name.to_string(),
            message: format!("invalid base64 image content from '{tool_name}': {e}"),
        })?;
    let extension = image_extension(mime_type);
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let filename = format!(
        "dyson_mcp_{}_{}_{}_{}.{}",
        safe_filename_part(server_name),
        safe_filename_part(tool_name),
        stamp,
        idx,
        extension
    );
    let path = std::env::temp_dir().join(filename);
    std::fs::write(&path, &bytes).map_err(|e| DysonError::Mcp {
        server: server_name.to_string(),
        message: format!("failed to save MCP image content from '{tool_name}': {e}"),
    })?;
    Ok((path, bytes.len()))
}

pub(super) fn image_extension(mime_type: &str) -> &'static str {
    match mime_type {
        "image/jpeg" => "jpg",
        "image/webp" => "webp",
        "image/gif" => "gif",
        "image/svg+xml" => "svg",
        _ => "png",
    }
}

/// Maximum bytes we will decode + write from a single MCP resource
/// block.  Matches the swarm proxy's per-file inline cap; an MCP server
/// (or an assist) that hands us a larger blob is most likely buggy or
/// hostile.
pub(super) const MAX_MCP_RESOURCE_BYTES: usize = 64 * 1024 * 1024;

/// Cap for inlining a text resource body in `read()` tool output.  Larger
/// bodies are truncated inline (with a note) but the full body still lands
/// in the workspace artefact.  16 KiB ≈ 4-5K tokens — enough for docs and
/// configs without bloating context on runaway logs.
pub(super) const INLINE_TEXT_CAP: usize = 16 * 1024;

/// `str::floor_char_boundary` is unstable; this is the stable workalike.
/// Walks back from `index` until the byte is at a UTF-8 char boundary so
/// the truncated head can never split a multi-byte sequence.
pub(super) fn floor_char_boundary(s: &str, index: usize) -> usize {
    if index >= s.len() {
        return s.len();
    }
    let mut i = index;
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Save an MCP `resource` content block as a local file and return the
/// path, byte count, and the original (pre-sanitization) filename.
///
/// Accepts both spec variants:
///   * `BlobResourceContents` — base64-decode `resource.blob`.
///   * `TextResourceContents` — write `resource.text` as UTF-8 bytes.
///
/// Filename derivation:
///   * Take the path component after the last `/` in `resource.uri`.
///   * Sanitize via [`safe_filename_part`]: ASCII alphanumerics +
///     `._-` only, truncated to 64 chars, falling back to `resource`
///     when nothing survives.
///   * Prefix with a per-call stamp + idx so two resources in the
///     same tool call never collide on disk.
///
/// Refuses:
///   * Both `blob` and `text` empty (no body).
///   * `blob` whose decoded size exceeds [`MAX_MCP_RESOURCE_BYTES`].
///   * `text` whose UTF-8 size exceeds [`MAX_MCP_RESOURCE_BYTES`].
///   * Invalid base64 in `blob`.
pub(super) fn save_mcp_resource(
    server_name: &str,
    tool_name: &str,
    idx: usize,
    resource: &McpResourceContents,
) -> Result<(PathBuf, usize, String)> {
    let bytes = if !resource.blob.is_empty() {
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&resource.blob)
            .map_err(|e| DysonError::Mcp {
                server: server_name.to_string(),
                message: format!("invalid base64 resource content from '{tool_name}': {e}"),
            })?;
        if decoded.len() > MAX_MCP_RESOURCE_BYTES {
            return Err(DysonError::Mcp {
                server: server_name.to_string(),
                message: format!(
                    "resource blob from '{tool_name}' exceeds {MAX_MCP_RESOURCE_BYTES}-byte cap ({} bytes)",
                    decoded.len()
                ),
            });
        }
        decoded
    } else if !resource.text.is_empty() {
        let text_bytes = resource.text.as_bytes().to_vec();
        if text_bytes.len() > MAX_MCP_RESOURCE_BYTES {
            return Err(DysonError::Mcp {
                server: server_name.to_string(),
                message: format!(
                    "resource text from '{tool_name}' exceeds {MAX_MCP_RESOURCE_BYTES}-byte cap ({} bytes)",
                    text_bytes.len()
                ),
            });
        }
        text_bytes
    } else {
        return Err(DysonError::Mcp {
            server: server_name.to_string(),
            message: format!("resource from '{tool_name}' has neither blob nor text body"),
        });
    };
    let original_name = uri_basename(&resource.uri)
        .unwrap_or("resource")
        .to_string();
    let safe_name = safe_filename_part(&original_name);
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let filename = format!(
        "dyson_mcp_{}_{}_{}_{}_{}",
        safe_filename_part(server_name),
        safe_filename_part(tool_name),
        stamp,
        idx,
        safe_name,
    );
    let path = std::env::temp_dir().join(filename);
    std::fs::write(&path, &bytes).map_err(|e| DysonError::Mcp {
        server: server_name.to_string(),
        message: format!("failed to save MCP resource from '{tool_name}': {e}"),
    })?;
    Ok((path, bytes.len(), original_name))
}

/// Trailing path component of an MCP resource URI.  Returns `None`
/// when the uri has no `/` separator (treat as opaque) or when the
/// path ends in `/`.  Pure string slicing — no URI parser dep — so
/// the LLM-visible marker can always show what came in even if the
/// uri is non-standard.
pub(super) fn uri_basename(uri: &str) -> Option<&str> {
    let after_scheme = uri.split_once("://").map(|(_, rest)| rest).unwrap_or(uri);
    let last = after_scheme.rsplit('/').next()?;
    if last.is_empty() { None } else { Some(last) }
}

pub(super) fn safe_filename_part(value: &str) -> String {
    let mut out: String = value
        .chars()
        .map(|c| {
            // `.` is allowed so the resource sanitizer can preserve
            // extensions like `realdl.txt`.  Path-traversal is still
            // prevented because `/` is mapped to `_` and the result is
            // always sandwiched between a prefix and an idx — the
            // final filename is a single component under /tmp.
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .take(64)
        .collect();
    if out.is_empty() {
        out.push_str("mcp");
    }
    out
}
