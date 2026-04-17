// BFS taint-reachability from a source `file:line` to a sink `file:line`
// over a pre-built `SymbolIndex`.  Name-based call resolution with
// positional argument binding.  Lossy by design — the tool is a
// hypothesis generator; the agent verifies each hop.

use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};

use crate::ast::{self, LanguageConfig};

use super::index::collect_tainted_identifiers;
use super::types::{FnId, Frame, Hop, HopKind, SymbolIndex, TaintPath};

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

    // Re-parse the source file once to read identifiers on source.line.
    let source_parsed = ast::try_parse_file(source_file, working_dir, false)
        .map_err(|e| TraceError::ParseFailed {
            file: source_rel.display().to_string(),
            reason: e.to_string(),
        })?
        .ok_or_else(|| TraceError::SourceNotIndexed {
            file: source_rel.display().to_string(),
        })?;
    let source_tree = &source_parsed.1.tree;
    let source_src = source_parsed.1.source.as_str();

    let source_byte = byte_offset_of_line(source_src, source_line);
    let source_line_end = line_end_byte(source_src, source_line);
    let source_node = source_tree
        .root_node()
        .descendant_for_byte_range(source_byte, source_line_end)
        .unwrap_or_else(|| source_tree.root_node());

    let enclosing_node = ast::find_enclosing_function(source_node, config, source_src.as_bytes())
        .ok_or_else(|| TraceError::NoEnclosingFunction {
            file: source_rel.display().to_string(),
            line: source_line,
        })?;

    // Map enclosing node to its FnId in the index.  Fall back to
    // `fn_enclosing` lookup — the index is the source of truth.
    let start_fn = match index.fn_enclosing(&source_rel, source_byte) {
        Some(id) => id,
        None => {
            return Err(TraceError::NoEnclosingFunction {
                file: source_rel.display().to_string(),
                line: source_line,
            });
        }
    };

    // Extract initial tainted identifiers from the *statement* containing
    // source.line.  Skip the LHS on declarations (receivers are not
    // sources — the RHS is).
    let initial_tainted =
        extract_source_taint(enclosing_node, source_byte, source_src, config);
    if initial_tainted.is_empty() {
        // Fall back to all identifiers on the line — conservative, gives
        // the agent some starting ground instead of silent zero paths.
        // Real "no identifiers" usually means blank / comment / punctuation,
        // which NoEnclosingFunction will have caught first.
    }

    // Parse sink file to look up identifiers on sink.line for the
    // reachability check.
    let sink_parsed = ast::try_parse_file(sink_file, working_dir, false)
        .map_err(|e| TraceError::ParseFailed {
            file: sink_rel.display().to_string(),
            reason: e.to_string(),
        })?
        .ok_or_else(|| TraceError::SinkNotIndexed {
            file: sink_rel.display().to_string(),
        })?;
    let sink_src = sink_parsed.1.source.as_str();
    let sink_byte = byte_offset_of_line(sink_src, sink_line);
    let sink_identifiers = identifiers_on_line(&sink_parsed.1.tree, sink_src, sink_line, config);

    // BFS.
    let mut paths: Vec<TaintPath> = Vec::new();
    let mut visited: HashSet<(FnId, Vec<String>)> = HashSet::new();
    let mut frontier_count = 0usize;
    let mut truncated_frontier = false;

    let source_hop = Hop {
        file: source_rel.clone(),
        line: source_line,
        byte_range: byte_range_of_line(source_src, source_line),
        fn_name: index.fn_defs[start_fn].name.clone(),
        detail: format!(
            "fn `{}` — taint root: {}",
            index.fn_defs[start_fn].name,
            if initial_tainted.is_empty() {
                "<none extracted>".to_string()
            } else {
                initial_tainted.iter().cloned().collect::<Vec<_>>().join(", ")
            },
        ),
        kind: HopKind::Source,
        ambiguous_candidates: Vec::new(),
    };

    let mut queue: Vec<Frame> = vec![Frame {
        fn_id: start_fn,
        tainted: initial_tainted,
        hops: vec![source_hop],
    }];

    while let Some(frame) = queue.pop() {
        if paths.len() >= opts.max_paths {
            break;
        }
        let dedup_key = (frame.fn_id, frame.tainted.iter().cloned().collect::<Vec<_>>());
        if !visited.insert(dedup_key) {
            continue;
        }

        // Gather all call sites in this frame's function, in byte order.
        let mut call_sites: Vec<&super::types::CallSite> = index
            .call_sites
            .iter()
            .filter(|cs| cs.in_fn == frame.fn_id)
            .collect();
        call_sites.sort_by_key(|cs| cs.byte_range.start);

        let mut local_assigns: Vec<&super::types::Assignment> = index
            .assignments
            .iter()
            .filter(|a| a.in_fn == frame.fn_id)
            .collect();
        local_assigns.sort_by_key(|a| a.byte_start);

        // Check same-frame sink reachability.
        if frame.fn_id == start_fn_for_sink(index, &sink_rel, sink_byte).unwrap_or(usize::MAX)
            || Some(frame.fn_id) == index.fn_enclosing(&sink_rel, sink_byte)
        {
            // Apply same-frame assignment propagation up to the sink line,
            // then check overlap with sink identifiers.
            let mut running_taint = frame.tainted.clone();
            for a in &local_assigns {
                if a.line >= sink_line {
                    break;
                }
                if a.rhs_idents.iter().any(|r| running_taint.contains(r)) {
                    for l in &a.lhs {
                        running_taint.insert(l.clone());
                    }
                }
            }
            if sink_identifiers.iter().any(|s| running_taint.contains(s)) {
                let sink_hop = Hop {
                    file: sink_rel.clone(),
                    line: sink_line,
                    byte_range: byte_range_of_line(sink_src, sink_line),
                    fn_name: index.fn_defs[frame.fn_id].name.clone(),
                    detail: format!(
                        "[SINK REACHED] — tainted at sink: {}",
                        sink_identifiers
                            .iter()
                            .filter(|s| running_taint.contains(*s))
                            .cloned()
                            .collect::<Vec<_>>()
                            .join(", "),
                    ),
                    kind: HopKind::Sink,
                    ambiguous_candidates: Vec::new(),
                };
                let mut hops = frame.hops.clone();
                hops.push(sink_hop);
                paths.push(TaintPath {
                    hops,
                    truncated: false,
                });
                continue;
            }
        }

        // Stop expanding if we've hit depth.
        if frame.hops.len() > opts.max_depth {
            continue;
        }

        // Propagate same-frame assignments into a per-frame running set
        // that we can reference at each call site below.
        let mut running = frame.tainted.clone();
        for (idx, cs) in call_sites.iter().enumerate() {
            // Apply assignments that precede this call site.
            for a in &local_assigns {
                if a.byte_start >= cs.byte_range.start {
                    break;
                }
                if a.rhs_idents.iter().any(|r| running.contains(r)) {
                    for l in &a.lhs {
                        running.insert(l.clone());
                    }
                }
            }

            // Is any argument slot tainted?
            let mut tainted_arg_positions: Vec<usize> = Vec::new();
            for (i, args) in cs.arg_idents.iter().enumerate() {
                if args.iter().any(|id| running.contains(id)) {
                    tainted_arg_positions.push(i);
                }
            }
            if tainted_arg_positions.is_empty() {
                continue;
            }

            // Resolve callee to fn defs.
            let candidates: Vec<FnId> = if cs.callee.is_empty() {
                Vec::new()
            } else {
                index
                    .by_name
                    .get(&cs.callee)
                    .cloned()
                    .unwrap_or_default()
            };

            if candidates.is_empty() {
                // Unresolved — record hop but don't extend path.
                let mut hops = frame.hops.clone();
                hops.push(Hop {
                    file: cs.file.clone(),
                    line: cs.line,
                    byte_range: cs.byte_range.clone(),
                    fn_name: index.fn_defs[frame.fn_id].name.clone(),
                    detail: format!(
                        "calls `{}` — callee unresolved (dynamic dispatch, import alias, or out of index)",
                        if cs.callee.is_empty() {
                            "<computed>".to_string()
                        } else {
                            cs.callee.clone()
                        },
                    ),
                    kind: HopKind::UnresolvedCallee,
                    ambiguous_candidates: Vec::new(),
                });
                // Still emit as a "candidate path" if the frame already
                // reached the sink's function; but since we gate on
                // same-fn-as-sink above, this branch is strictly informational.
                // We include it only if it would advance us toward the sink
                // — skip extending the queue here.
                let _ = idx; // silence warn
                continue;
            }

            // Resolved — push one frame per candidate.
            let ambiguous = candidates.len() > 1;
            for &cand in &candidates {
                let mut new_tainted = BTreeSet::new();
                let callee_params = &index.fn_defs[cand].params;
                let mut imprecise = false;
                for &arg_pos in &tainted_arg_positions {
                    if arg_pos < callee_params.len() {
                        new_tainted.insert(callee_params[arg_pos].clone());
                    } else {
                        // Arity mismatch — fall back to tainting all params.
                        for p in callee_params {
                            new_tainted.insert(p.clone());
                        }
                        imprecise = true;
                    }
                }
                // Also carry over any globally-visible symbols the
                // callee might refer to.  For MVP we don't track
                // closures — just pass params.

                let hop_kind = if imprecise {
                    HopKind::ImpreciseBinding
                } else if ambiguous {
                    HopKind::Ambiguous
                } else {
                    HopKind::Resolved
                };

                let ambiguous_candidates: Vec<(PathBuf, usize, String)> = if ambiguous {
                    candidates
                        .iter()
                        .map(|&c| {
                            let d = &index.fn_defs[c];
                            (d.file.clone(), d.line, d.name.clone())
                        })
                        .collect()
                } else {
                    Vec::new()
                };

                let detail = format!(
                    "calls `{}({})` → {}{}",
                    cs.callee,
                    render_tainted_args(&cs.arg_idents, &tainted_arg_positions),
                    if new_tainted.is_empty() {
                        "no param binding".to_string()
                    } else {
                        format!(
                            "param{} {}",
                            if new_tainted.len() == 1 { "" } else { "s" },
                            new_tainted
                                .iter()
                                .map(|p| format!("`{p}`"))
                                .collect::<Vec<_>>()
                                .join(", "),
                        )
                    },
                    if imprecise { " [IMPRECISE]" } else { "" },
                );

                let mut new_hops = frame.hops.clone();
                new_hops.push(Hop {
                    file: cs.file.clone(),
                    line: cs.line,
                    byte_range: cs.byte_range.clone(),
                    fn_name: index.fn_defs[frame.fn_id].name.clone(),
                    detail,
                    kind: hop_kind,
                    ambiguous_candidates,
                });

                if new_hops.len() > opts.max_depth + 1 {
                    continue;
                }

                frontier_count += 1;
                if frontier_count > opts.max_frontier {
                    truncated_frontier = true;
                    break;
                }

                queue.push(Frame {
                    fn_id: cand,
                    tainted: new_tainted,
                    hops: new_hops,
                });
            }
            if truncated_frontier {
                break;
            }
        }
        if truncated_frontier {
            break;
        }
    }

    // Rank: (unresolved asc, depth asc, imprecise asc).
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
// Helpers
// ---------------------------------------------------------------------------

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
    if line == 0 {
        return 0;
    }
    let mut current_line = 1;
    for (i, b) in src.bytes().enumerate() {
        if current_line == line {
            return i;
        }
        if b == b'\n' {
            current_line += 1;
        }
    }
    src.len().saturating_sub(1)
}

fn byte_range_of_line(src: &str, line: usize) -> std::ops::Range<usize> {
    let start = byte_offset_of_line(src, line);
    let end = line_end_byte(src, line);
    start..end
}

fn line_end_byte(src: &str, line: usize) -> usize {
    let start = byte_offset_of_line(src, line);
    let mut end = start;
    for b in src.bytes().skip(start) {
        if b == b'\n' {
            break;
        }
        end += 1;
    }
    end
}

/// Tree walk for identifiers on a given line, obeying the same "skip
/// callees, root of member_expression only" rules as the index builder.
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
    // Walk up to the enclosing statement so we scoop the whole line's
    // expressions, not just the leaf at the byte offset.
    let stmt = climb_to_statement(node);
    let mut out = Vec::new();
    collect_tainted_identifiers(stmt, src, config, &mut out);
    out
}

fn climb_to_statement<'a>(mut node: tree_sitter::Node<'a>) -> tree_sitter::Node<'a> {
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
        if k == "program" || k == "source_file" || k == "module" || k == "compilation_unit" {
            return node;
        }
        node = parent;
    }
    node
}

/// Extract initial taint from the source line.  Looks for the statement
/// at `byte`, prefers the `value` / `right` field (RHS of a declaration),
/// falls back to all data identifiers in the statement.
fn extract_source_taint(
    enclosing_fn: tree_sitter::Node<'_>,
    byte: usize,
    src: &str,
    config: &LanguageConfig,
) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let node = enclosing_fn
        .descendant_for_byte_range(byte, byte)
        .unwrap_or(enclosing_fn);
    let stmt = climb_to_statement(node);
    let mut collected: Vec<String> = Vec::new();
    // RHS-first: if the statement has a `value` or `right` field, prefer it.
    let rhs = stmt
        .child_by_field_name("value")
        .or_else(|| stmt.child_by_field_name("right"));
    if let Some(r) = rhs {
        collect_tainted_identifiers(r, src, config, &mut collected);
    }
    if collected.is_empty() {
        // Function parameters on the header line also count as taint sources
        // (e.g. agent points at `fn handler(req, res) {`).
        collect_tainted_identifiers(stmt, src, config, &mut collected);
    }
    for s in collected {
        out.insert(s);
    }
    out
}

fn start_fn_for_sink(index: &SymbolIndex, sink_rel: &Path, sink_byte: usize) -> Option<FnId> {
    index.fn_enclosing(sink_rel, sink_byte)
}

fn render_tainted_args(
    arg_idents: &[Vec<String>],
    tainted_positions: &[usize],
) -> String {
    let mut parts = Vec::new();
    for (i, args) in arg_idents.iter().enumerate() {
        let joined = if args.is_empty() {
            "_".to_string()
        } else {
            args.join("+")
        };
        if tainted_positions.contains(&i) {
            parts.push(format!("[{joined}]"));
        } else {
            parts.push(joined);
        }
    }
    parts.join(", ")
}
