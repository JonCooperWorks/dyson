// Build a per-language SymbolIndex by walking the working directory,
// parsing every file with the matching language, and flattening function
// definitions, call sites, and assignments into index-friendly lists.
//
// Runs inside `spawn_blocking` because tree-sitter parsing is sync CPU;
// blocking the tokio runtime starves concurrent tool calls that the
// security_engineer prompt explicitly encourages.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use tree_sitter::Node;

use crate::ast::{self, LanguageConfig, nodes};
use crate::error::Result;

use super::types::{Assignment, CallSite, FnDef, FnId, SymbolIndex};

/// Node kinds that act like "name = value" assignments, per language.
/// Tier 1 (TS / JS / Python / Rust / Go / Java / C / C++ / C# / Ruby) has
/// proper support; other languages parse calls/defs but same-frame
/// assignment propagation degrades to none.
fn assignment_types(language: &'static str) -> &'static [&'static str] {
    match language {
        "JavaScript" | "TypeScript" | "TSX" => {
            &["variable_declarator", "assignment_expression"]
        }
        "Python" => &["assignment", "augmented_assignment"],
        "Rust" => &["let_declaration", "assignment_expression"],
        "Go" => &["short_var_declaration", "assignment_statement"],
        "Java" => &["local_variable_declaration", "assignment_expression"],
        "C" | "C++" => &["init_declarator", "assignment_expression"],
        "C#" => &["variable_declarator", "assignment_expression"],
        "Ruby" => &["assignment"],
        _ => &[],
    }
}

/// Entry point — synchronous, must be called inside `spawn_blocking`.
pub fn build_index_sync(
    language: &'static LanguageConfig,
    working_dir: &Path,
) -> Result<SymbolIndex> {
    let working_dir_canon = working_dir
        .canonicalize()
        .unwrap_or_else(|_| working_dir.to_path_buf());

    let mut files_seen = 0usize;
    let mut truncated = false;

    let mut fn_defs: Vec<FnDef> = Vec::new();
    let mut call_sites: Vec<CallSite> = Vec::new();
    let mut assignments: Vec<Assignment> = Vec::new();
    let mut fn_by_file: HashMap<PathBuf, Vec<FnId>> = HashMap::new();
    let mut file_mtimes: HashMap<PathBuf, SystemTime> = HashMap::new();
    let mut unresolved_callees = 0usize;

    for entry in ast::walk_dir(&working_dir_canon).flatten() {
        if files_seen >= ast::MAX_FILES {
            truncated = true;
            break;
        }
        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
            continue;
        }
        let path = entry.path();
        let ext = match path.extension().and_then(|e| e.to_str()) {
            Some(e) => e,
            None => continue,
        };
        let config = match ast::config_for_extension(ext) {
            Some(c) => c,
            None => continue,
        };
        if config.display_name != language.display_name {
            continue;
        }

        let parsed = match ast::try_parse_file(path, &working_dir_canon, false) {
            Ok(Some((_, p))) => p,
            _ => continue,
        };
        files_seen += 1;

        let rel = PathBuf::from(&parsed.rel_path);
        if let Ok(md) = std::fs::metadata(path)
            && let Ok(mt) = md.modified()
        {
            file_mtimes.insert(rel.clone(), mt);
        }

        let assign_kinds = assignment_types(language.display_name);
        let mut walker = Walker {
            config: language,
            source: &parsed.source,
            rel_path: &rel,
            assign_kinds,
            fn_defs: &mut fn_defs,
            call_sites: &mut call_sites,
            assignments: &mut assignments,
            fn_by_file: &mut fn_by_file,
            unresolved_callees: &mut unresolved_callees,
            current_fn: None,
        };
        walker.walk(parsed.tree.root_node());
    }

    let mut by_name: HashMap<String, Vec<FnId>> = HashMap::new();
    for (id, def) in fn_defs.iter().enumerate() {
        if !def.name.is_empty() {
            by_name.entry(def.name.clone()).or_default().push(id);
        }
    }

    // Stable order inside fn_by_file so enclosing lookups prefer earlier defs.
    for ids in fn_by_file.values_mut() {
        ids.sort_by_key(|&id| fn_defs[id].def_range.start);
    }

    Ok(SymbolIndex {
        language: language.display_name,
        fn_defs,
        by_name,
        call_sites,
        assignments,
        fn_by_file,
        file_mtimes,
        truncated,
        unresolved_callees,
    })
}

/// Check whether any file in `index` has been modified since the index
/// was built.  Cheap — one `metadata` syscall per indexed file.
pub fn is_stale(index: &SymbolIndex, working_dir: &Path) -> bool {
    for (rel, built_mtime) in &index.file_mtimes {
        let abs = working_dir.join(rel);
        let Ok(md) = std::fs::metadata(&abs) else {
            return true;
        };
        match md.modified() {
            Ok(now) if now > *built_mtime => return true,
            Err(_) => return true,
            _ => {}
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Per-file recursive walker
// ---------------------------------------------------------------------------

struct Walker<'a> {
    config: &'static LanguageConfig,
    source: &'a str,
    rel_path: &'a Path,
    assign_kinds: &'static [&'static str],
    fn_defs: &'a mut Vec<FnDef>,
    call_sites: &'a mut Vec<CallSite>,
    assignments: &'a mut Vec<Assignment>,
    fn_by_file: &'a mut HashMap<PathBuf, Vec<FnId>>,
    unresolved_callees: &'a mut usize,
    current_fn: Option<FnId>,
}

impl<'a> Walker<'a> {
    fn walk(&mut self, node: Node<'_>) {
        let kind = node.kind();
        let mut entered_fn = false;

        if self.config.definition_types.contains(&kind) {
            let elixir_skip = self.config.definitions_are_calls
                && kind == "call"
                && !nodes::is_elixir_definition(&node, self.source.as_bytes());
            if !elixir_skip
                && let Some(id) = self.record_definition(node)
            {
                self.current_fn = Some(id);
                entered_fn = true;
            }
        }

        if self.config.call_types.contains(&kind)
            && let Some(fn_id) = self.current_fn
        {
            self.record_call(node, fn_id);
        }

        if self.assign_kinds.contains(&kind)
            && let Some(fn_id) = self.current_fn
        {
            self.record_assignment(node, fn_id);
        }

        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            self.walk(child);
        }

        if entered_fn {
            self.current_fn = None;
        }
    }

    fn record_definition(&mut self, node: Node<'_>) -> Option<FnId> {
        let name =
            nodes::extract_definition_name(&node, self.source.as_bytes()).unwrap_or_default();
        let body = node
            .child_by_field_name("body")
            .map(|b| b.byte_range())
            .unwrap_or_else(|| node.byte_range());
        let params = extract_parameters(node, self.source);
        let id = self.fn_defs.len();
        self.fn_defs.push(FnDef {
            file: self.rel_path.to_path_buf(),
            line: node.start_position().row + 1,
            def_range: node.byte_range(),
            body_range: body,
            name,
            params,
        });
        self.fn_by_file
            .entry(self.rel_path.to_path_buf())
            .or_default()
            .push(id);
        Some(id)
    }

    fn record_call(&mut self, node: Node<'_>, fn_id: FnId) {
        let callee = extract_callee_name(node, self.source);
        if callee.is_empty() {
            *self.unresolved_callees += 1;
        }
        let arg_idents = extract_arg_idents(node, self.source, self.config);
        let range = node.byte_range();
        let snippet = short_snippet(&self.source[range.clone()]);
        self.call_sites.push(CallSite {
            file: self.rel_path.to_path_buf(),
            line: node.start_position().row + 1,
            byte_range: range,
            in_fn: fn_id,
            callee,
            arg_idents,
            snippet,
        });
    }

    fn record_assignment(&mut self, node: Node<'_>, fn_id: FnId) {
        let lhs = extract_field_idents(node, self.source, self.config, &["name", "left"]);
        let rhs = extract_field_idents(node, self.source, self.config, &["value", "right"]);
        if lhs.is_empty() && rhs.is_empty() {
            return;
        }
        self.assignments.push(Assignment {
            line: node.start_position().row + 1,
            byte_start: node.start_byte(),
            in_fn: fn_id,
            lhs,
            rhs_idents: rhs,
        });
    }
}

// ---------------------------------------------------------------------------
// Small extraction helpers
// ---------------------------------------------------------------------------

fn extract_callee_name(node: Node<'_>, source: &str) -> String {
    if let Some(fn_field) = node.child_by_field_name("function") {
        return flatten_callee(fn_field, source);
    }
    // Fallback: first identifier / member-access child.
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        let kind = child.kind();
        if kind == "identifier" || kind == "property_identifier" {
            return source[child.byte_range()].to_string();
        }
    }
    String::new()
}

/// For `foo.bar.baz(...)` tree-sitter exposes the callee as a
/// `member_expression` / `field_expression`.  We flatten to the last
/// identifier (`baz`) — that's the name used for resolution.
fn flatten_callee(node: Node<'_>, source: &str) -> String {
    let kind = node.kind();
    if kind == "identifier" || kind == "property_identifier" || kind == "simple_identifier" {
        return source[node.byte_range()].to_string();
    }
    if let Some(p) = node.child_by_field_name("property") {
        return source[p.byte_range()].to_string();
    }
    if let Some(n) = node.child_by_field_name("name") {
        return source[n.byte_range()].to_string();
    }
    // Walk for the rightmost identifier.
    let mut cursor = node.walk();
    let mut last = String::new();
    for child in node.children(&mut cursor) {
        let k = child.kind();
        if k == "identifier" || k == "property_identifier" {
            last = source[child.byte_range()].to_string();
        }
    }
    last
}

fn extract_parameters(node: Node<'_>, source: &str) -> Vec<String> {
    let params_node = node
        .child_by_field_name("parameters")
        .or_else(|| node.child_by_field_name("formal_parameters"))
        .or_else(|| node.child_by_field_name("params"));
    let Some(pn) = params_node else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut cursor = pn.walk();
    for child in pn.children(&mut cursor) {
        // A parameter may be wrapped in `parameter`, `typed_parameter`,
        // `required_parameter`, `identifier_pattern`, etc.  The name
        // field / first identifier descendant is the parameter name.
        let name = child
            .child_by_field_name("name")
            .or_else(|| child.child_by_field_name("pattern"))
            .map(|n| source[n.byte_range()].to_string())
            .or_else(|| first_identifier_text(child, source));
        if let Some(n) = name
            && !n.is_empty()
        {
            out.push(n);
        }
    }
    out
}

fn first_identifier_text(node: Node<'_>, source: &str) -> Option<String> {
    let kind = node.kind();
    if kind == "identifier" || kind == "simple_identifier" {
        return Some(source[node.byte_range()].to_string());
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if let Some(found) = first_identifier_text(child, source) {
            return Some(found);
        }
    }
    None
}

/// Collect identifier names inside each positional argument of a call.
/// Used for positional taint binding.
pub(crate) fn extract_arg_idents(
    call_node: Node<'_>,
    source: &str,
    config: &LanguageConfig,
) -> Vec<Vec<String>> {
    let args_node = call_node
        .child_by_field_name("arguments")
        .or_else(|| call_node.child_by_field_name("argument_list"));
    let Some(args) = args_node else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut cursor = args.walk();
    for child in args.children(&mut cursor) {
        // Skip punctuation nodes.
        let k = child.kind();
        if !child.is_named() || k == "(" || k == ")" || k == "," {
            continue;
        }
        let mut idents = Vec::new();
        collect_tainted_identifiers(child, source, config, &mut idents);
        out.push(idents);
    }
    out
}

/// Collect identifiers we'd consider "data" (not callees).  For member
/// chains like `req.body.url`, take only the root object — matching the
/// tainted-symbol extraction rules.
pub(crate) fn collect_tainted_identifiers(
    node: Node<'_>,
    source: &str,
    config: &LanguageConfig,
    out: &mut Vec<String>,
) {
    let kind = node.kind();
    if config.identifier_types.contains(&kind) {
        // Skip if this identifier is the callee of a parent call expression.
        if let Some(parent) = node.parent()
            && config.call_types.contains(&parent.kind())
            && let Some(fn_field) = parent.child_by_field_name("function")
            && fn_field.id() == node.id()
        {
            return;
        }
        // Skip property_identifier in a member_expression (root-only taint).
        if kind == "property_identifier" {
            return;
        }
        let text = source[node.byte_range()].to_string();
        if !text.is_empty() && !out.contains(&text) {
            out.push(text);
        }
        return;
    }
    // For member_expression / field_expression: descend only into the object
    // side to reach the root identifier.
    if kind == "member_expression" || kind == "field_expression" || kind == "attribute" {
        if let Some(obj) = node
            .child_by_field_name("object")
            .or_else(|| node.child_by_field_name("value"))
        {
            collect_tainted_identifiers(obj, source, config, out);
        }
        return;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_tainted_identifiers(child, source, config, out);
    }
}

fn extract_field_idents(
    node: Node<'_>,
    source: &str,
    config: &LanguageConfig,
    fields: &[&str],
) -> Vec<String> {
    for f in fields {
        if let Some(child) = node.child_by_field_name(f) {
            let mut out = Vec::new();
            collect_tainted_identifiers(child, source, config, &mut out);
            return out;
        }
    }
    Vec::new()
}

fn short_snippet(s: &str) -> String {
    const MAX: usize = 80;
    let cleaned: String = s.chars().map(|c| if c == '\n' { ' ' } else { c }).collect();
    if cleaned.len() <= MAX {
        cleaned
    } else {
        let mut truncated: String = cleaned.chars().take(MAX).collect();
        truncated.push('…');
        truncated
    }
}

// ---------------------------------------------------------------------------
// Async entry — wraps sync build in spawn_blocking so the tokio runtime
// isn't starved.
// ---------------------------------------------------------------------------

pub async fn build_index(
    language: &'static LanguageConfig,
    working_dir: &Path,
) -> Result<SymbolIndex> {
    let dir = working_dir.to_path_buf();
    tokio::task::spawn_blocking(move || build_index_sync(language, &dir))
        .await
        .map_err(|e| {
            crate::error::DysonError::tool("taint_trace", format!("index build join: {e}"))
        })?
}
