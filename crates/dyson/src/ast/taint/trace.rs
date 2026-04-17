// BFS taint-reachability from a source `file:line` to a sink `file:line`
// over a pre-built `SymbolIndex`.  Name-based call resolution, positional
// argument binding, lossy by design — the agent verifies each hop.

use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};

use tree_sitter::Node;

use crate::ast::{self, LanguageConfig};

use super::index::collect_tainted_identifiers;
use super::types::{FnId, Hop, HopKind, SymbolIndex, TaintPath};

pub struct TraceOptions {
    pub max_depth: usize,
    pub max_paths: usize,
    pub max_frontier: usize,
}

impl Default for TraceOptions {
    fn default() -> Self {
        Self {
            max_depth: 8,
            max_paths: 5,
            max_frontier: 10_000,
        }
    }
}

pub struct TraceResult {
    pub paths: Vec<TaintPath>,
    pub truncated_frontier: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum TraceError {
    #[error("no enclosing function at {file}:{line}")]
    NoEnclosingFunction { file: String, line: usize },
    #[error("source file {file} not indexed (wrong extension or outside working dir)")]
    SourceNotIndexed { file: String },
    #[error("sink file {file} not indexed (wrong extension or outside working dir)")]
    SinkNotIndexed { file: String },
    #[error("could not parse source file {file}: {reason}")]
    ParseFailed { file: String, reason: String },
    #[error("language '{language}' does not support taint_trace (no call node types)")]
    UnsupportedLanguage { language: &'static str },
}

#[derive(Debug, Clone)]
struct Frame {
    fn_id: FnId,
    tainted: BTreeSet<String>,
    hops: Vec<Hop>,
}

#[allow(clippy::too_many_arguments)]
pub fn trace(
    index: &SymbolIndex,
    config: &LanguageConfig,
    working_dir: &Path,
    source_file: &Path,
    source_line: usize,
    sink_file: &Path,
    sink_line: usize,
    opts: &TraceOptions,
) -> Result<TraceResult, TraceError> {
    if config.call_types.is_empty() {
        return Err(TraceError::UnsupportedLanguage {
            language: config.display_name,
        });
    }

    let source_rel = rel_path(working_dir, source_file);
    let sink_rel = rel_path(working_dir, sink_file);

    let source_parsed = parse(source_file, working_dir).map_err(|e| match e {
        ParseErr::Parse(r) => TraceError::ParseFailed {
            file: source_rel.display().to_string(),
            reason: r,
        },
        ParseErr::Missing => TraceError::SourceNotIndexed {
            file: source_rel.display().to_string(),
        },
    })?;
    let source_src = source_parsed.source.as_str();
    let source_byte = byte_offset_of_line(source_src, source_line);

    let source_node = source_parsed
        .tree
        .root_node()
        .descendant_for_byte_range(source_byte, line_end_byte(source_src, source_line))
        .unwrap_or_else(|| source_parsed.tree.root_node());
    let enclosing_fn_node = ast::find_enclosing_function(source_node, config, source_src.as_bytes())
        .ok_or_else(|| TraceError::NoEnclosingFunction {
            file: source_rel.display().to_string(),
            line: source_line,
        })?;
    let start_fn = index.fn_enclosing(&source_rel, source_byte).ok_or_else(|| {
        TraceError::NoEnclosingFunction {
            file: source_rel.display().to_string(),
            line: source_line,
        }
    })?;

    let initial_tainted = extract_source_taint(enclosing_fn_node, source_byte, source_src, config);

    let sink_parsed = parse(sink_file, working_dir).map_err(|e| match e {
        ParseErr::Parse(r) => TraceError::ParseFailed {
            file: sink_rel.display().to_string(),
            reason: r,
        },
        ParseErr::Missing => TraceError::SinkNotIndexed {
            file: sink_rel.display().to_string(),
        },
    })?;
    let sink_src = sink_parsed.source.as_str();
    let sink_byte = byte_offset_of_line(sink_src, sink_line);
    let sink_fn = index.fn_enclosing(&sink_rel, sink_byte);
    let sink_identifiers = identifiers_on_line(&sink_parsed.tree, sink_src, sink_line, config);
    let sink_line_range = byte_range_of_line(sink_src, sink_line);

    let mut paths: Vec<TaintPath> = Vec::new();
    let mut visited: HashSet<(FnId, BTreeSet<String>)> = HashSet::new();
    let mut frontier_count = 0usize;
    let mut truncated_frontier = false;

    let source_hop = Hop {
        file: source_rel.clone(),
        line: source_line,
        byte_range: byte_range_of_line(source_src, source_line),
        detail: format!(
            "fn `{}` — taint root: {}",
            index.fn_defs[start_fn].name,
            join_sorted(&initial_tainted).unwrap_or_else(|| "<none extracted>".into()),
        ),
        kind: HopKind::Source,
        ambiguous_candidates: Vec::new(),
    };

    let mut queue: Vec<Frame> = vec![Frame {
        fn_id: start_fn,
        tainted: initial_tainted,
        hops: vec![source_hop],
    }];

    'outer: while let Some(frame) = queue.pop() {
        if paths.len() >= opts.max_paths {
            break;
        }
        if !visited.insert((frame.fn_id, frame.tainted.clone())) {
            continue;
        }

        let local_calls = calls_for(index, frame.fn_id);
        let local_assigns = assigns_for(index, frame.fn_id);

        // Same-frame sink reachability: propagate assignments up to sink_line,
        // then check overlap.  Only run when we're actually in the sink's fn.
        if sink_fn == Some(frame.fn_id) {
            let running = propagate_until(&frame.tainted, local_assigns, index, |a| a.line < sink_line);
            if sink_identifiers.iter().any(|s| running.contains(s)) {
                let mut hops = frame.hops.clone();
                hops.push(Hop {
                    file: sink_rel.clone(),
                    line: sink_line,
                    byte_range: sink_line_range.clone(),
                    detail: format!(
                        "[SINK REACHED] — tainted at sink: {}",
                        sink_identifiers
                            .iter()
                            .filter(|s| running.contains(*s))
                            .cloned()
                            .collect::<Vec<_>>()
                            .join(", "),
                    ),
                    kind: HopKind::Sink,
                    ambiguous_candidates: Vec::new(),
                });
                paths.push(TaintPath { hops });
                continue;
            }
        }

        if frame.hops.len() > opts.max_depth {
            continue;
        }

        // Walk call sites in byte order, propagating assignments as we go.
        let mut running = frame.tainted.clone();
        let mut assign_iter = local_assigns.iter().map(|&i| &index.assignments[i]);
        let mut next_assign = assign_iter.next();

        for &cs_idx in local_calls {
            let cs = &index.call_sites[cs_idx];
            // Apply assignments preceding this call site.
            while let Some(a) = next_assign
                && a.byte_start < cs.byte_range.start
            {
                apply_assignment(&mut running, a);
                next_assign = assign_iter.next();
            }

            let tainted_args = tainted_arg_positions(&cs.arg_idents, &running);
            if tainted_args.is_empty() {
                continue;
            }

            let candidates = index.by_name.get(&cs.callee).map(Vec::as_slice).unwrap_or(&[]);
            if candidates.is_empty() {
                // Unresolved callee — annotate but don't extend path.
                let mut hops = frame.hops.clone();
                hops.push(Hop {
                    file: cs.file.clone(),
                    line: cs.line,
                    byte_range: cs.byte_range.clone(),
                    detail: format!(
                        "calls `{}` — callee unresolved (dynamic dispatch, import alias, or out of index)",
                        if cs.callee.is_empty() { "<computed>" } else { &cs.callee },
                    ),
                    kind: HopKind::UnresolvedCallee,
                    ambiguous_candidates: Vec::new(),
                });
                paths.push(TaintPath { hops });
                continue;
            }

            let ambiguous = candidates.len() > 1;
            for &cand in candidates {
                let (new_tainted, imprecise) = bind_args(&index.fn_defs[cand].params, &tainted_args);
                let kind = if imprecise {
                    HopKind::ImpreciseBinding
                } else if ambiguous {
                    HopKind::Ambiguous
                } else {
                    HopKind::Resolved
                };
                let detail = format!(
                    "calls `{}({})` → {}{}",
                    cs.callee,
                    render_tainted_args(&cs.arg_idents, &tainted_args),
                    render_param_binding(&new_tainted),
                    if imprecise { " [IMPRECISE]" } else { "" },
                );
                let mut new_hops = frame.hops.clone();
                new_hops.push(Hop {
                    file: cs.file.clone(),
                    line: cs.line,
                    byte_range: cs.byte_range.clone(),
                    detail,
                    kind,
                    ambiguous_candidates: if ambiguous { candidates.to_vec() } else { Vec::new() },
                });

                if new_hops.len() > opts.max_depth + 1 {
                    continue;
                }

                frontier_count += 1;
                if frontier_count > opts.max_frontier {
                    truncated_frontier = true;
                    break 'outer;
                }

                queue.push(Frame {
                    fn_id: cand,
                    tainted: new_tainted,
                    hops: new_hops,
                });
            }
        }
    }

    paths.sort_by(|a, b| {
        a.unresolved_hops()
            .cmp(&b.unresolved_hops())
            .then(a.depth().cmp(&b.depth()))
            .then(a.imprecise_bindings().cmp(&b.imprecise_bindings()))
    });
    paths.truncate(opts.max_paths);

    Ok(TraceResult {
        paths,
        truncated_frontier,
    })
}

// ---------------------------------------------------------------------------
// BFS helpers
// ---------------------------------------------------------------------------

fn calls_for(index: &SymbolIndex, fn_id: FnId) -> &[usize] {
    index.calls_by_fn.get(&fn_id).map(Vec::as_slice).unwrap_or(&[])
}

fn assigns_for(index: &SymbolIndex, fn_id: FnId) -> &[usize] {
    index.assigns_by_fn.get(&fn_id).map(Vec::as_slice).unwrap_or(&[])
}

fn apply_assignment(running: &mut BTreeSet<String>, a: &super::types::Assignment) {
    if a.rhs_idents.iter().any(|r| running.contains(r)) {
        for l in &a.lhs {
            running.insert(l.clone());
        }
    }
}

fn propagate_until(
    seed: &BTreeSet<String>,
    assign_ids: &[usize],
    index: &SymbolIndex,
    cond: impl Fn(&super::types::Assignment) -> bool,
) -> BTreeSet<String> {
    let mut running = seed.clone();
    for &i in assign_ids {
        let a = &index.assignments[i];
        if !cond(a) {
            break;
        }
        apply_assignment(&mut running, a);
    }
    running
}

fn tainted_arg_positions(arg_idents: &[Vec<String>], running: &BTreeSet<String>) -> Vec<usize> {
    arg_idents
        .iter()
        .enumerate()
        .filter_map(|(i, args)| args.iter().any(|id| running.contains(id)).then_some(i))
        .collect()
}

fn bind_args(callee_params: &[String], tainted_positions: &[usize]) -> (BTreeSet<String>, bool) {
    let mut out = BTreeSet::new();
    let mut imprecise = false;
    for &pos in tainted_positions {
        if pos < callee_params.len() {
            out.insert(callee_params[pos].clone());
        } else {
            for p in callee_params {
                out.insert(p.clone());
            }
            imprecise = true;
        }
    }
    (out, imprecise)
}

fn render_param_binding(bound: &BTreeSet<String>) -> String {
    if bound.is_empty() {
        return "no param binding".into();
    }
    let joined = bound.iter().map(|p| format!("`{p}`")).collect::<Vec<_>>().join(", ");
    format!("param{} {}", if bound.len() == 1 { "" } else { "s" }, joined)
}

fn render_tainted_args(arg_idents: &[Vec<String>], tainted_positions: &[usize]) -> String {
    arg_idents
        .iter()
        .enumerate()
        .map(|(i, args)| {
            let joined = if args.is_empty() { "_".into() } else { args.join("+") };
            if tainted_positions.contains(&i) {
                format!("[{joined}]")
            } else {
                joined
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn join_sorted(set: &BTreeSet<String>) -> Option<String> {
    if set.is_empty() {
        None
    } else {
        Some(set.iter().cloned().collect::<Vec<_>>().join(", "))
    }
}

// ---------------------------------------------------------------------------
// Parsing + byte/line helpers
// ---------------------------------------------------------------------------

enum ParseErr {
    Parse(String),
    Missing,
}

fn parse(file: &Path, working_dir: &Path) -> Result<crate::ast::ParsedFile, ParseErr> {
    ast::try_parse_file(file, working_dir, false)
        .map_err(|e| ParseErr::Parse(e.to_string()))?
        .map(|(_, p)| p)
        .ok_or(ParseErr::Missing)
}

fn rel_path(working_dir: &Path, file: &Path) -> PathBuf {
    if let Ok(canon_file) = file.canonicalize()
        && let Ok(canon_wd) = working_dir.canonicalize()
        && let Ok(rel) = canon_file.strip_prefix(&canon_wd)
    {
        return rel.to_path_buf();
    }
    file.strip_prefix(working_dir)
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|_| file.to_path_buf())
}

fn byte_offset_of_line(src: &str, line: usize) -> usize {
    if line <= 1 {
        return 0;
    }
    let mut current = 1;
    for (i, b) in src.bytes().enumerate() {
        if b == b'\n' {
            current += 1;
            if current == line {
                return i + 1;
            }
        }
    }
    src.len()
}

fn line_end_byte(src: &str, line: usize) -> usize {
    let start = byte_offset_of_line(src, line);
    src.bytes()
        .skip(start)
        .position(|b| b == b'\n')
        .map(|off| start + off)
        .unwrap_or(src.len())
}

fn byte_range_of_line(src: &str, line: usize) -> std::ops::Range<usize> {
    byte_offset_of_line(src, line)..line_end_byte(src, line)
}

fn identifiers_on_line(
    tree: &tree_sitter::Tree,
    src: &str,
    line: usize,
    config: &LanguageConfig,
) -> Vec<String> {
    let byte = byte_offset_of_line(src, line);
    let node = tree
        .root_node()
        .descendant_for_byte_range(byte, byte)
        .unwrap_or_else(|| tree.root_node());
    let stmt = climb_to_statement(node);
    let mut out = Vec::new();
    collect_tainted_identifiers(stmt, src, config, &mut out);
    out
}

/// Walk parents until hitting a statement-like ancestor.  Used to scope
/// identifier collection to the statement containing a given byte.
fn climb_to_statement(mut node: Node<'_>) -> Node<'_> {
    while let Some(parent) = node.parent() {
        let k = parent.kind();
        if k.ends_with("_statement")
            || k == "expression_statement"
            || k == "lexical_declaration"
            || k == "variable_declaration"
            || k == "let_declaration"
        {
            return parent;
        }
        if matches!(k, "program" | "source_file" | "module" | "compilation_unit") {
            return node;
        }
        node = parent;
    }
    node
}

fn extract_source_taint(
    enclosing_fn: Node<'_>,
    byte: usize,
    src: &str,
    config: &LanguageConfig,
) -> BTreeSet<String> {
    let node = enclosing_fn.descendant_for_byte_range(byte, byte).unwrap_or(enclosing_fn);
    let stmt = climb_to_statement(node);
    let mut collected = Vec::new();
    // Prefer the RHS of a declaration so the LHS receiver isn't itself a taint source.
    if let Some(rhs) = stmt
        .child_by_field_name("value")
        .or_else(|| stmt.child_by_field_name("right"))
    {
        collect_tainted_identifiers(rhs, src, config, &mut collected);
    }
    if collected.is_empty() {
        // Fall back to the whole statement — e.g. agent points at `fn handler(req)`.
        collect_tainted_identifiers(stmt, src, config, &mut collected);
    }
    collected.into_iter().collect()
}
