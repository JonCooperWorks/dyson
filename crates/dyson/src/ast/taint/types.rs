// Data types for the taint_trace symbol index and BFS output.

use std::collections::{BTreeSet, HashMap};
use std::ops::Range;
use std::path::PathBuf;
use std::time::SystemTime;

/// Index into `SymbolIndex::fn_defs`.  Opaque; do not treat as meaningful.
pub type FnId = usize;

/// A parsed function / method / class-method definition node, flattened
/// out of the per-file tree-sitter parse into something we can walk
/// cheaply during BFS without keeping each `tree_sitter::Tree` alive.
#[derive(Debug, Clone)]
pub struct FnDef {
    /// Path relative to the working directory.
    pub file: PathBuf,
    /// 1-indexed line of the definition header.
    pub line: usize,
    /// Byte range of the definition header (for citation / read_file spans).
    pub def_range: Range<usize>,
    /// Byte range of the function body.  Empty range for trait / interface
    /// declarations with no body.  BFS uses this to decide which call sites
    /// live "inside" this function.
    pub body_range: Range<usize>,
    /// Extracted name.  Empty for anonymous defs.
    pub name: String,
    /// Parameter names in declaration order.  Positional binding maps
    /// arg index → this slot.
    pub params: Vec<String>,
}

/// A call site inside a function body.
#[derive(Debug, Clone)]
pub struct CallSite {
    pub file: PathBuf,
    pub line: usize,
    pub byte_range: Range<usize>,
    /// Which `FnDef` contains this call.  Cached at index-build time so
    /// BFS doesn't re-walk the tree.
    pub in_fn: FnId,
    /// Extracted callee name.  Empty for dynamic dispatch, computed
    /// callees, method chains we couldn't flatten.
    pub callee: String,
    /// Identifiers appearing inside each positional argument slot.
    /// `arg_idents[i]` is the set for the i-th arg.
    pub arg_idents: Vec<Vec<String>>,
    /// Short rendered snippet of the call (e.g. `clean(req.body.q)`),
    /// truncated to ~80 chars for output.
    pub snippet: String,
}

/// An assignment statement inside a function body.  Tracked so same-frame
/// taint propagation (e.g. `const x = req.body; sink(x)`) works.
#[derive(Debug, Clone)]
pub struct Assignment {
    pub line: usize,
    pub byte_start: usize,
    pub in_fn: FnId,
    /// LHS identifiers receiving the assignment.
    pub lhs: Vec<String>,
    /// RHS identifiers read to produce the value.
    pub rhs_idents: Vec<String>,
}

/// Per-language symbol index.  Held on `ToolContext`, built once, reused
/// across `taint_trace` calls, invalidated when any indexed file's mtime
/// exceeds `built_at`.
pub struct SymbolIndex {
    pub language: &'static str,
    pub fn_defs: Vec<FnDef>,
    /// Function name → every `FnId` with that name.  Same-name functions
    /// in different files are preserved (surfaced as AMBIGUOUS in output).
    pub by_name: HashMap<String, Vec<FnId>>,
    pub call_sites: Vec<CallSite>,
    pub assignments: Vec<Assignment>,
    /// File → all `FnId`s defined in that file (sorted by `def_range.start`).
    /// Speeds up "what function contains file:line?" queries.
    pub fn_by_file: HashMap<PathBuf, Vec<FnId>>,
    /// mtime of each indexed file at build time.
    pub file_mtimes: HashMap<PathBuf, SystemTime>,
    /// When `ast::MAX_FILES` was hit during discovery.
    pub truncated: bool,
    /// Counts for the output header.
    pub unresolved_callees: usize,
}

impl SymbolIndex {
    /// Find the `FnId` whose *full definition* contains `byte` in `file`.
    /// Includes the header (so source lines pointing at `def handler(...)`
    /// resolve correctly), not just the body.  Prefers the innermost
    /// definition when nested.
    pub fn fn_enclosing(&self, file: &std::path::Path, byte: usize) -> Option<FnId> {
        let list = self.fn_by_file.get(file)?;
        list.iter()
            .copied()
            .filter(|&id| {
                let r = &self.fn_defs[id].def_range;
                r.contains(&byte)
            })
            .min_by_key(|&id| {
                let r = &self.fn_defs[id].def_range;
                r.end - r.start
            })
    }

    /// Like [`fn_enclosing`] but checks `body_range` (excludes the header).
    /// Used for "is this call/sink INSIDE the function body?" queries.
    pub fn fn_enclosing_body(&self, file: &std::path::Path, byte: usize) -> Option<FnId> {
        let list = self.fn_by_file.get(file)?;
        list.iter()
            .copied()
            .filter(|&id| {
                let r = &self.fn_defs[id].body_range;
                r.contains(&byte)
            })
            .min_by_key(|&id| {
                let r = &self.fn_defs[id].body_range;
                r.end - r.start
            })
    }
}

// ---------------------------------------------------------------------------
// Output types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct TaintPath {
    pub hops: Vec<Hop>,
    /// True when BFS hit `max_frontier` before finding all paths.
    pub truncated: bool,
}

#[derive(Debug, Clone)]
pub struct Hop {
    pub file: PathBuf,
    pub line: usize,
    pub byte_range: Range<usize>,
    pub fn_name: String,
    /// Short human-readable description ("calls `clean(req.body.q)` → param
    /// `input`", "`conn.query(input)` [SINK REACHED]", etc.)
    pub detail: String,
    pub kind: HopKind,
    /// For `HopKind::Ambiguous`: all candidate defs with file:line so the
    /// agent can inspect each.
    pub ambiguous_candidates: Vec<(PathBuf, usize, String)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HopKind {
    Source,
    Resolved,
    ImpreciseBinding,
    Ambiguous,
    UnresolvedCallee,
    Sink,
}

impl TaintPath {
    pub fn unresolved_hops(&self) -> usize {
        self.hops
            .iter()
            .filter(|h| matches!(h.kind, HopKind::UnresolvedCallee))
            .count()
    }

    pub fn imprecise_bindings(&self) -> usize {
        self.hops
            .iter()
            .filter(|h| matches!(h.kind, HopKind::ImpreciseBinding))
            .count()
    }

    pub fn depth(&self) -> usize {
        self.hops.len().saturating_sub(1)
    }

    pub fn resolved_hops(&self) -> usize {
        self.hops
            .iter()
            .filter(|h| !matches!(h.kind, HopKind::UnresolvedCallee))
            .count()
    }
}

/// BFS frame — internal.
#[derive(Debug, Clone)]
pub(crate) struct Frame {
    pub fn_id: FnId,
    pub tainted: BTreeSet<String>,
    pub hops: Vec<Hop>,
}
