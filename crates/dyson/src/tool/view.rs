// ===========================================================================
// ToolView — typed UI payloads for tool results.
//
// `ToolOutput.content` is what the LLM sees (a string).  `ToolOutput.view`
// is what the UI sees: a typed payload the controller can render natively
// (a terminal for `bash`, a diff for `edit_file`, an SBOM table for
// `dependency_scan`, a taint flow for `taint_trace`, a read view for
// `read_file`).
//
// The shapes match the right-rail panel renderers in
// `web/prototype/components/panels.jsx`.  Wire format is JSON tagged on
// `kind` so the bridge layer can dispatch directly: it looks at the
// `kind` discriminator, hands the rest of the object to the matching
// React component, and the panel renders.
// ===========================================================================

use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolView {
    /// Terminal-style output — what the `bash` tool produces.
    Bash {
        lines: Vec<TermLine>,
        exit_code: Option<i32>,
        duration_ms: u64,
    },
    /// File diff — `edit_file` / `write_file` / `bulk_edit`.
    Diff { files: Vec<DiffFile> },
    /// File contents with optional line highlight — `read_file`.
    Read {
        path: String,
        lines: Vec<String>,
        highlight: Option<usize>,
    },
    /// SBOM + vulnerabilities — `dependency_scan`.
    Sbom {
        rows: Vec<SbomRow>,
        counts: SbomCounts,
    },
    /// Taint flow source → propagator(s) → sink — `taint_trace`.
    Taint { flow: Vec<TaintNode> },
}

#[derive(Debug, Clone, Serialize)]
pub struct TermLine {
    /// Line classification: `'p'` prompt, `'c'` content, `'e'` error,
    /// `'w'` warning, `'d'` dim/info.
    pub c: char,
    /// The line text (no trailing newline).
    pub t: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct DiffFile {
    pub path: String,
    pub add: usize,
    pub rem: usize,
    /// Hunk header (e.g. `@@ -8,9 +8,15 @@ pub async fn require_auth`).
    pub hunk: String,
    pub rows: Vec<DiffRow>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DiffRow {
    /// `"add"`, `"rem"`, or `"ctx"`.
    pub t: String,
    pub ln: usize,
    /// Visual sign character: `'+'`, `'-'`, or `' '`.
    pub sn: String,
    pub l: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SbomRow {
    pub pkg: String,
    pub ver: String,
    /// One of: `"crit"`, `"high"`, `"med"`, `"low"`, `"unknown"`.
    pub sev: String,
    pub id: String,
    /// `"reachable"` / `"unreachable"` / `"unknown"`.
    pub reach: String,
    pub note: String,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct SbomCounts {
    pub crit: usize,
    pub high: usize,
    pub med: usize,
    pub low: usize,
    pub total: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct TaintNode {
    /// `"source"`, `"prop"` (propagator), or `"sink"`.
    pub kind: String,
    /// Location, e.g. `"auth.rs:8"`.
    pub loc: String,
    /// Symbol/expression at this node.
    pub sym: String,
    /// Free-form note explaining what happens here.
    pub note: String,
}
