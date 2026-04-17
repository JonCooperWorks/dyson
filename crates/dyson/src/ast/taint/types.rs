// Data types for the taint_trace symbol index and BFS output.

use std::collections::HashMap;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Index into `SymbolIndex::fn_defs`.  Opaque.
pub type FnId = usize;

#[derive(Debug, Clone)]
pub struct FnDef {
    pub file: PathBuf,
    pub line: usize,
    pub def_range: Range<usize>,
    pub body_range: Range<usize>,
    pub name: String,
    pub params: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct CallSite {
    pub file: PathBuf,
    pub line: usize,
    pub byte_range: Range<usize>,
    pub in_fn: FnId,
    pub callee: String,
    pub arg_idents: Vec<Vec<String>>,
}

#[derive(Debug, Clone)]
pub struct Assignment {
    pub line: usize,
    pub byte_start: usize,
    pub in_fn: FnId,
    pub lhs: Vec<String>,
    pub rhs_idents: Vec<String>,
}

/// Per-language symbol index.  Built once per language per `ToolContext`,
/// reused across `taint_trace` calls, invalidated on mtime change.
pub struct SymbolIndex {
    pub language: &'static str,
    pub fn_defs: Vec<FnDef>,
    /// Function name → every `FnId` with that name.  Ambiguity is preserved
    /// and surfaced to the agent as a hop annotation.
    pub by_name: HashMap<String, Vec<FnId>>,
    pub call_sites: Vec<CallSite>,
    pub assignments: Vec<Assignment>,
    /// Pre-computed `in_fn → sorted indices into call_sites` for O(1) BFS.
    pub calls_by_fn: HashMap<FnId, Vec<usize>>,
    /// Pre-computed `in_fn → sorted indices into assignments` for O(1) BFS.
    pub assigns_by_fn: HashMap<FnId, Vec<usize>>,
    pub fn_by_file: HashMap<PathBuf, Vec<FnId>>,
    pub file_mtimes: HashMap<PathBuf, SystemTime>,
    pub truncated: bool,
    pub unresolved_callees: usize,
}

impl SymbolIndex {
    /// Find the innermost `FnId` whose *full definition* contains `byte`.
    /// Header-inclusive — source lines on `def handler(...)` resolve here.
    pub fn fn_enclosing(&self, file: &Path, byte: usize) -> Option<FnId> {
        self.fn_in_file(file, byte, |d| &d.def_range)
    }

    /// Like [`fn_enclosing`] but body-only.  Used for "is this call/sink
    /// *inside* the function body (not the header)?" queries.
    pub fn fn_enclosing_body(&self, file: &Path, byte: usize) -> Option<FnId> {
        self.fn_in_file(file, byte, |d| &d.body_range)
    }

    fn fn_in_file(
        &self,
        file: &Path,
        byte: usize,
        range: impl Fn(&FnDef) -> &Range<usize>,
    ) -> Option<FnId> {
        let list = self.fn_by_file.get(file)?;
        list.iter()
            .copied()
            .filter(|&id| range(&self.fn_defs[id]).contains(&byte))
            .min_by_key(|&id| {
                let r = range(&self.fn_defs[id]);
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
}

#[derive(Debug, Clone)]
pub struct Hop {
    pub file: PathBuf,
    pub line: usize,
    pub byte_range: Range<usize>,
    pub detail: String,
    pub kind: HopKind,
    /// For `HopKind::Ambiguous`: the candidate fn defs.  Rendered at output
    /// time via `SymbolIndex` lookup — keeps the hop cheap to clone.
    pub ambiguous_candidates: Vec<FnId>,
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
        self.hops.len() - self.unresolved_hops()
    }
}
