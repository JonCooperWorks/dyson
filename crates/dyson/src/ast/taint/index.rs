// Build a per-language SymbolIndex by walking the working directory,
// parsing every matching file, and flattening function definitions,
// call sites, and assignments into lookup-friendly lists.
//
// Runs inside `spawn_blocking` — tree-sitter parsing is sync CPU and
// would starve concurrent tool calls the security_engineer prompt
// explicitly encourages.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use tree_sitter::Node;

use crate::ast::{self, LanguageConfig, nodes};
use crate::error::Result;

use super::types::{Assignment, CallSite, FnDef, FnId, SymbolIndex};

/// Per-language index ceiling.  Deliberately larger than `ast::MAX_FILES`
/// (the per-query cap used by `ast_query` / `search_files`): taint_trace
/// builds the index once per language per session and serves many BFS
/// queries against it, so indexing more files amortises well.  At 5k
/// files / 150-byte-per-call average, hot codebases like deno/cli or
/// nushell/crates stay fully indexed; only compiler-scale repos
/// (TypeScript, swift-project) still truncate.
pub const TAINT_MAX_FILES: usize = 5000;

/// Node kinds that behave like "name = value" assignments.  Tier-1 only
/// (TS/JS/Python/Rust/Go/Java/C/C++/C#/Ruby); other languages still parse
/// calls + defs but same-frame assignment propagation silently degrades.
fn assignment_types(language: &'static str) -> &'static [&'static str] {
    match language {
        "JavaScript" | "TypeScript" | "TSX" => &["variable_declarator", "assignment_expression"],
        "Python" => &["assignment", "augmented_assignment"],
        "Rust" => &["let_declaration", "assignment_expression"],
        "Go" => &["short_var_declaration", "assignment_statement"],
        "Java" => &["local_variable_declaration", "assignment_expression"],
        "C" | "C++" => &["init_declarator", "assignment_expression"],
        "C#" => &["variable_declarator", "assignment_expression"],
        "Ruby" => &["assignment"],
        "Swift" | "Kotlin" => &["property_declaration", "assignment"],
        "Zig" => &["variable_declaration", "assignment_expression"],
        _ => &[],
    }
}

/// Whether a `definition_types` kind acts as a function-like scope for
/// taint analysis.  Data declarations (variables, properties, types) live
/// in `definition_types` for `list_definitions` but aren't scopes — and if
/// we entered them as a scope, their own assignment-kind siblings would
/// be attributed to this transient "scope" instead of the enclosing fn.
fn is_fn_scope(kind: &str) -> bool {
    !matches!(
        kind,
        "lexical_declaration"
            | "variable_declaration"
            | "property_declaration"
            | "type_alias"
            | "type_alias_declaration"
            | "const_item"
            | "static_item"
            | "type_item"
            | "type_definition"
    )
}

pub(crate) fn build_index_sync(
    language: &'static LanguageConfig,
    working_dir: &Path,
) -> Result<SymbolIndex> {
    let working_dir_canon = working_dir
        .canonicalize()
        .unwrap_or_else(|_| working_dir.to_path_buf());

    let mut builder = IndexBuilder::default();
    let assign_kinds = assignment_types(language.display_name);

    for entry in ast::walk_dir(&working_dir_canon).flatten() {
        if builder.files_seen >= TAINT_MAX_FILES {
            builder.truncated = true;
            break;
        }
        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
            continue;
        }
        let path = entry.path();
        let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
            continue;
        };
        let Some(config) = ast::config_for_extension(ext) else {
            continue;
        };
        if config.display_name != language.display_name {
            continue;
        }
        let Ok(Some((_, parsed))) = ast::try_parse_file(path, &working_dir_canon, false) else {
            continue;
        };
        builder.files_seen += 1;

        let rel = PathBuf::from(&parsed.rel_path);
        if let Ok(md) = std::fs::metadata(path)
            && let Ok(mt) = md.modified()
        {
            builder.file_mtimes.insert(rel.clone(), mt);
        }

        Walker {
            config: language,
            source: &parsed.source,
            rel_path: &rel,
            assign_kinds,
            builder: &mut builder,
            current_fn: None,
        }
        .walk(parsed.tree.root_node());
    }

    Ok(builder.finish(language.display_name))
}

/// Any indexed file's mtime > index-build time?  One `metadata` syscall
/// per indexed file; invalidation drops the whole language's cache.
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

pub async fn build_index(
    language: &'static LanguageConfig,
    working_dir: &Path,
) -> Result<SymbolIndex> {
    let dir = working_dir.to_path_buf();
    tokio::task::spawn_blocking(move || build_index_sync(language, &dir))
        .await
        .map_err(|e| crate::error::DysonError::tool("taint_trace", format!("index build: {e}")))?
}

// ---------------------------------------------------------------------------
// IndexBuilder — owns accumulated state across files.  Walker holds a
// single `&mut IndexBuilder` instead of six separate `&mut` fields.
// ---------------------------------------------------------------------------

#[derive(Default)]
struct IndexBuilder {
    fn_defs: Vec<FnDef>,
    call_sites: Vec<CallSite>,
    assignments: Vec<Assignment>,
    fn_by_file: HashMap<PathBuf, Vec<FnId>>,
    file_mtimes: HashMap<PathBuf, SystemTime>,
    files_seen: usize,
    truncated: bool,
    unresolved_callees: usize,
}

impl IndexBuilder {
    /// Finalize accumulated state into a ready-to-query `SymbolIndex`.
    /// Pre-sorts per-fn call-site and assignment lists so BFS skips the
    /// sort on every frame.
    fn finish(mut self, language: &'static str) -> SymbolIndex {
        let mut by_name: HashMap<String, Vec<FnId>> = HashMap::new();
        for (id, def) in self.fn_defs.iter().enumerate() {
            if !def.name.is_empty() {
                by_name.entry(def.name.clone()).or_default().push(id);
            }
        }

        let mut calls_by_fn: HashMap<FnId, Vec<usize>> = HashMap::new();
        for (i, cs) in self.call_sites.iter().enumerate() {
            calls_by_fn.entry(cs.in_fn).or_default().push(i);
        }
        for v in calls_by_fn.values_mut() {
            v.sort_by_key(|&i| self.call_sites[i].byte_range.start);
        }

        let mut assigns_by_fn: HashMap<FnId, Vec<usize>> = HashMap::new();
        for (i, a) in self.assignments.iter().enumerate() {
            assigns_by_fn.entry(a.in_fn).or_default().push(i);
        }
        for v in assigns_by_fn.values_mut() {
            v.sort_by_key(|&i| self.assignments[i].byte_start);
        }

        for ids in self.fn_by_file.values_mut() {
            ids.sort_by_key(|&id| self.fn_defs[id].def_range.start);
        }

        SymbolIndex {
            language,
            fn_defs: self.fn_defs,
            by_name,
            call_sites: self.call_sites,
            assignments: self.assignments,
            calls_by_fn,
            assigns_by_fn,
            fn_by_file: self.fn_by_file,
            file_mtimes: self.file_mtimes,
            truncated: self.truncated,
            unresolved_callees: self.unresolved_callees,
        }
    }
}

// ---------------------------------------------------------------------------
// Walker — per-file recursive descent.
// ---------------------------------------------------------------------------

struct Walker<'a> {
    config: &'static LanguageConfig,
    source: &'a str,
    rel_path: &'a Path,
    assign_kinds: &'static [&'static str],
    builder: &'a mut IndexBuilder,
    current_fn: Option<FnId>,
}

impl Walker<'_> {
    fn walk(&mut self, node: Node<'_>) {
        let kind = node.kind();
        let saved_fn = self.current_fn;

        if self.config.definition_types.contains(&kind) && is_fn_scope(kind) {
            let elixir_skip = self.config.definitions_are_calls
                && kind == "call"
                && !nodes::is_elixir_definition(&node, self.source.as_bytes());
            if !elixir_skip
                && let Some(id) = self.record_definition(node)
            {
                self.current_fn = Some(id);
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

        self.current_fn = saved_fn;
    }

    fn record_definition(&mut self, node: Node<'_>) -> Option<FnId> {
        let id = self.builder.fn_defs.len();
        let mut params = extract_parameters(node, self.source);
        // Python exposes `self`/`cls` as ordinary first parameters, but
        // method dispatch `obj.foo(x)` passes the receiver implicitly —
        // so callers never supply it at position 0.  Dropping here lines
        // Python up with the Rust/Go convention where `self_parameter`
        // already sits outside the params list, keeping positional
        // binding consistent across languages.
        if self.config.display_name == "Python"
            && let Some(first) = params.first()
            && (first == "self" || first == "cls")
        {
            params.remove(0);
        }
        self.builder.fn_defs.push(FnDef {
            file: self.rel_path.to_path_buf(),
            line: node.start_position().row + 1,
            end_line: node.end_position().row + 1,
            def_range: node.byte_range(),
            body_range: node
                .child_by_field_name("body")
                .map(|b| b.byte_range())
                .unwrap_or_else(|| node.byte_range()),
            name: nodes::extract_definition_name(&node, self.source.as_bytes())
                .unwrap_or_default(),
            params,
        });
        self.builder
            .fn_by_file
            .entry(self.rel_path.to_path_buf())
            .or_default()
            .push(id);
        Some(id)
    }

    fn record_call(&mut self, node: Node<'_>, fn_id: FnId) {
        let callee = extract_callee_name(node, self.source);
        if callee.is_empty() {
            self.builder.unresolved_callees += 1;
        }
        self.builder.call_sites.push(CallSite {
            file: self.rel_path.to_path_buf(),
            line: node.start_position().row + 1,
            byte_range: node.byte_range(),
            in_fn: fn_id,
            callee,
            arg_idents: extract_arg_idents(node, self.source, self.config),
        });
    }

    fn record_assignment(&mut self, node: Node<'_>, fn_id: FnId) {
        let mut lhs = extract_field_idents(
            node,
            self.source,
            self.config,
            &["name", "left", "pattern", "target"],
        );
        let mut rhs = extract_field_idents(
            node,
            self.source,
            self.config,
            &["value", "right", "result"],
        );
        // Field-less fallback: Kotlin's `property_declaration` has no named
        // fields; LHS sits inside a `variable_declaration` child and RHS
        // is the trailing expression.
        if lhs.is_empty() && rhs.is_empty() {
            extract_kinded_children(node, self.source, self.config, &mut lhs, &mut rhs);
        }
        if lhs.is_empty() && rhs.is_empty() {
            return;
        }
        self.builder.assignments.push(Assignment {
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
    callee_name(node, source).unwrap_or_default().to_string()
}

fn callee_name<'a>(node: Node<'_>, source: &'a str) -> Option<&'a str> {
    // `function` (most languages) / `macro` (Rust macro_invocation).
    for field in ["function", "macro"] {
        if let Some(fn_field) = node.child_by_field_name(field) {
            return flatten_callee(fn_field, source);
        }
    }
    // Field-less calls (Swift): the first named non-arg child is the callee
    // expression — a bare identifier OR a member-access wrapper like
    // `navigation_expression`.  `flatten_callee` handles both.
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if !child.is_named() {
            continue;
        }
        if matches!(
            child.kind(),
            "call_suffix" | "arguments" | "argument_list" | "value_arguments"
        ) {
            continue;
        }
        if let Some(name) = flatten_callee(child, source) {
            return Some(name);
        }
    }
    None
}

/// `foo.bar.baz(...)` is a `member_expression` (JS/TS), `field_expression`
/// (Rust/Go), `attribute` (Python), `navigation_expression → navigation_suffix`
/// (Swift), etc.  Flatten to the rightmost identifier — that's the name used
/// for call resolution.
/// Returns a `&str` into `source` so recursion doesn't allocate a fresh
/// `String` at every level — callers `.to_string()` once at the leaf.
fn flatten_callee<'a>(node: Node<'_>, source: &'a str) -> Option<&'a str> {
    if is_identifier_kind(node.kind()) {
        return Some(&source[node.byte_range()]);
    }
    // Recurse through wrapper fields.  `suffix` handles Swift's
    // navigation_expression chain; `attrpath` handles Nix dotted paths;
    // the rest cover JS/TS, Python, Rust, Go, OCaml.
    for field in ["property", "field", "attribute", "name", "suffix", "attrpath"] {
        if let Some(n) = node.child_by_field_name(field)
            && let Some(name) = flatten_callee(n, source)
        {
            return Some(name);
        }
    }
    // Fallback: rightmost identifier-like child.
    let mut cursor = node.walk();
    let mut last = None;
    for child in node.children(&mut cursor) {
        if is_identifier_kind(child.kind()) {
            last = Some(&source[child.byte_range()]);
        }
    }
    last
}

fn is_identifier_kind(kind: &str) -> bool {
    matches!(
        kind,
        // Common
        "identifier"
            | "property_identifier"
            | "simple_identifier"
            | "field_identifier"
            | "type_identifier"
            // Erlang / Elixir
            | "atom"
            | "variable"
            // Ruby
            | "constant"
            // OCaml
            | "value_name"
            | "constructor_name"
            | "module_name"
            | "type_constructor"
            // Haskell
            | "variable_identifier"
            | "constructor_identifier"
    )
}

pub(crate) fn extract_parameters(node: Node<'_>, source: &str) -> Vec<String> {
    // The wrapper varies by language: `parameters`/`formal_parameters`/`params`
    // as named fields, or an unnamed child of one of a handful of kinds
    // (Swift, Kotlin).  Swift has no wrapper at all — `parameter` nodes
    // sit directly under function_declaration.  C / C++ park the
    // parameter_list inside `declarator → parameters`; without the
    // declarator descent the top-level field probe misses it entirely.
    let wrapper = ["parameters", "formal_parameters", "params"]
        .iter()
        .find_map(|f| node.child_by_field_name(f))
        .or_else(|| {
            node.child_by_field_name("declarator")
                .and_then(|d| d.child_by_field_name("parameters"))
        })
        .or_else(|| {
            find_child_of_kind(
                node,
                &["function_value_parameters", "parameter_clause", "parameter_list"],
            )
        })
        .unwrap_or(node);

    let mut out = Vec::new();
    let mut cursor = wrapper.walk();
    for child in wrapper.children(&mut cursor) {
        // Skip punctuation, but don't filter on kind — Python exposes
        // parameters as bare `identifier` children; Rust/Java wrap them
        // in `parameter`/`formal_parameter` nodes; Swift uses `parameter`.
        if !child.is_named() {
            continue;
        }
        let name = child
            .child_by_field_name("name")
            .or_else(|| child.child_by_field_name("pattern"))
            .map(|n| &source[n.byte_range()])
            .or_else(|| first_identifier_text(child, source));
        if let Some(n) = name
            && !n.is_empty()
        {
            out.push(n.to_string());
        }
    }
    out
}

fn find_child_of_kind<'a>(node: Node<'a>, kinds: &[&str]) -> Option<Node<'a>> {
    let mut cursor = node.walk();
    node.children(&mut cursor).find(|c| kinds.contains(&c.kind()))
}

fn first_identifier_text<'a>(node: Node<'_>, source: &'a str) -> Option<&'a str> {
    let kind = node.kind();
    if kind == "identifier" || kind == "simple_identifier" {
        return Some(&source[node.byte_range()]);
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if let Some(found) = first_identifier_text(child, source) {
            return Some(found);
        }
    }
    None
}

pub(crate) fn extract_arg_idents(
    call_node: Node<'_>,
    source: &str,
    config: &LanguageConfig,
) -> Vec<Vec<String>> {
    let args = call_node
        .child_by_field_name("arguments")
        .or_else(|| call_node.child_by_field_name("argument_list"))
        .or_else(|| {
            find_child_of_kind(
                call_node,
                &["arguments", "argument_list", "value_arguments", "call_suffix"],
            )
        });
    // Swift nests `value_arguments` inside a `call_suffix`.  Descend one
    // level to get the real arg list.
    let args = args.and_then(|n| {
        if n.kind() == "call_suffix" {
            find_child_of_kind(n, &["value_arguments"])
        } else {
            Some(n)
        }
    });
    let Some(args) = args else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut cursor = args.walk();
    for child in args.children(&mut cursor) {
        if !child.is_named() {
            continue;
        }
        let mut idents = Vec::new();
        collect_tainted_identifiers(child, source, config, &mut idents);
        out.push(idents);
    }
    out
}

/// Maximum number of segments kept when flattening a field chain like
/// `a.b.c.d.e.f.g` into a tainted path.  Segments past the cap are
/// dropped from the leaf end so the root remains intact — the BFS
/// prefix check matches from the root, so keeping it preserves reach.
pub const MAX_FIELD_DEPTH: usize = 5;

/// Collect tainted identifiers or field paths for the given subtree.
/// Pure chains of identifiers + field selectors — `obj.a.b`, `req.body`
/// — become a single dotted path; anything else (calls, subscripts,
/// operators) recurses through children.  Property identifiers outside
/// a chain are dropped (they'd match unrelated repo-wide vars).
pub(crate) fn collect_tainted_identifiers(
    node: Node<'_>,
    source: &str,
    config: &LanguageConfig,
    out: &mut Vec<String>,
) {
    let kind = node.kind();
    if config.identifier_types.contains(&kind) {
        if kind == "property_identifier" {
            return;
        }
        if let Some(parent) = node.parent()
            && config.call_types.contains(&parent.kind())
            && let Some(fn_field) = parent.child_by_field_name("function")
            && fn_field.id() == node.id()
        {
            return;
        }
        let text = source[node.byte_range()].to_string();
        if !text.is_empty() && !out.contains(&text) {
            out.push(text);
        }
        return;
    }
    if is_member_chain_kind(kind) {
        if let Some(path) = flatten_field_chain(node, source, MAX_FIELD_DEPTH)
            && !path.is_empty()
        {
            if !out.contains(&path) {
                out.push(path);
            }
            return;
        }
        // Not a pure chain (contains a call / subscript).  Descend into
        // the object only — this preserves the pre-path behaviour of
        // surfacing the chain's root identifier.
        if let Some(obj) = node
            .child_by_field_name("object")
            .or_else(|| node.child_by_field_name("value"))
            .or_else(|| node.child_by_field_name("operand"))
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

/// Member-chain node kinds across the covered languages.  Order matches
/// the field-name probe order in `collect_chain_segments`.
fn is_member_chain_kind(kind: &str) -> bool {
    matches!(
        kind,
        "member_expression" | "field_expression" | "selector_expression" | "attribute"
    )
}

/// If `node` is a pure chain of identifiers and field selectors,
/// returns the dotted path (capped at `max_depth` segments from the
/// root).  Returns `None` if the chain contains a call, subscript,
/// or other non-chain node.
fn flatten_field_chain(node: Node<'_>, source: &str, max_depth: usize) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
    collect_chain_segments(node, source, &mut parts)?;
    if parts.is_empty() {
        return None;
    }
    let end = parts.len().min(max_depth);
    Some(parts[..end].join("."))
}

fn collect_chain_segments<'a>(
    node: Node<'_>,
    source: &'a str,
    out: &mut Vec<&'a str>,
) -> Option<()> {
    let kind = node.kind();
    if is_identifier_kind(kind) {
        let text = &source[node.byte_range()];
        if !text.is_empty() {
            out.push(text);
        }
        return Some(());
    }
    if !is_member_chain_kind(kind) {
        return None;
    }
    let object = node
        .child_by_field_name("object")
        .or_else(|| node.child_by_field_name("value"))
        .or_else(|| node.child_by_field_name("operand"))?;
    collect_chain_segments(object, source, out)?;
    let property = node
        .child_by_field_name("property")
        .or_else(|| node.child_by_field_name("field"))
        .or_else(|| node.child_by_field_name("attribute"))?;
    if !is_identifier_kind(property.kind()) {
        return None;
    }
    let text = &source[property.byte_range()];
    if !text.is_empty() {
        out.push(text);
    }
    Some(())
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

/// Child-kind fallback for field-less nodes (Kotlin `property_declaration`).
/// Treats `variable_declaration` children as LHS and the tail `expression`
/// child as RHS.
fn extract_kinded_children(
    node: Node<'_>,
    source: &str,
    config: &LanguageConfig,
    lhs: &mut Vec<String>,
    rhs: &mut Vec<String>,
) {
    let mut cursor = node.walk();
    let mut last_expr: Option<Node<'_>> = None;
    for child in node.children(&mut cursor) {
        let kind = child.kind();
        if kind == "variable_declaration" {
            collect_tainted_identifiers(child, source, config, lhs);
        } else if kind == "expression" || config.identifier_types.contains(&kind) {
            last_expr = Some(child);
        }
    }
    if let Some(r) = last_expr {
        collect_tainted_identifiers(r, source, config, rhs);
    }
}

#[cfg(test)]
mod tests {
    use crate::ast::config_for_language_name;

    /// Count nodes whose kind is in `config.call_types`.  Panics with a
    /// descriptive message on any failure — we don't hide bad grammars.
    fn count_call_nodes(language: &str, source: &str) -> usize {
        let config = config_for_language_name(language)
            .unwrap_or_else(|| panic!("no config for {language}"));
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&config.language)
            .unwrap_or_else(|e| panic!("{language}: set_language failed: {e}"));
        let tree = parser
            .parse(source, None)
            .unwrap_or_else(|| panic!("{language}: parser returned no tree for `{source}`"));
        let mut count = 0usize;
        let mut stack = vec![tree.root_node()];
        while let Some(n) = stack.pop() {
            if config.call_types.contains(&n.kind()) {
                count += 1;
            }
            let mut cursor = n.walk();
            for c in n.children(&mut cursor) {
                stack.push(c);
            }
        }
        count
    }

    /// Every non-empty `call_types` entry must match at least one node in
    /// a minimal call sample.  Safety net for the exotic languages
    /// (Haskell, Nix, OCaml, etc.) where node names were guessed.
    #[test]
    fn call_types_match_real_parses() {
        let samples: &[(&str, &str)] = &[
            ("rust", "fn f() { g(); }"),
            ("python", "g()"),
            ("javascript", "g();"),
            ("typescript", "g();"),
            ("tsx", "const x = g();"),
            ("go", "package p\nfunc f() { g() }"),
            ("java", "class C { void f() { g(); } }"),
            ("c", "int f() { g(); return 0; }"),
            ("cpp", "int f() { g(); return 0; }"),
            ("csharp", "class C { void f() { g(); } }"),
            ("ruby", "g()"),
            ("kotlin", "fun f() { g() }"),
            ("swift", "func f() { g() }"),
            ("zig", "fn f() void { _ = g(); }"),
            ("elixir", "g()"),
            ("erlang", "-module(m).\nf() -> g()."),
            ("ocaml", "let _ = g ()"),
            ("haskell", "f = g 1"),
            ("nix", "g 1"),
        ];
        let mut failed = Vec::new();
        for (lang, src) in samples {
            let n = count_call_nodes(lang, src);
            if n == 0 {
                failed.push(format!("{lang} (0 matches in `{src}`)"));
            }
        }
        assert!(
            failed.is_empty(),
            "call_types did not match real parses: {failed:?}",
        );
    }

    #[test]
    fn json_has_empty_call_types() {
        let config = config_for_language_name("json").unwrap();
        assert!(config.call_types.is_empty());
    }
}
