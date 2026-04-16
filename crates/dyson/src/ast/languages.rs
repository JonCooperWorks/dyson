// ===========================================================================
// Language registry — maps file extensions to tree-sitter grammars.
//
// Each language gets a static LanguageConfig with:
//   - The tree-sitter Language (from the grammar crate)
//   - Identifier node types (for rename_symbol AST path)
//   - Definition node types (for list_definitions)
//   - A human-readable display name
//
// All 20 grammars (19 languages) are statically linked — no dynamic loading.
// ===========================================================================

use std::sync::LazyLock;

/// Configuration for a single language's tree-sitter grammar.
pub struct LanguageConfig {
    pub language: tree_sitter::Language,
    pub identifier_types: &'static [&'static str],
    pub definition_types: &'static [&'static str],
    #[cfg_attr(not(test), allow(dead_code))]
    pub display_name: &'static str,
    /// Whether definitions require special extraction logic (e.g., Elixir
    /// where `call` nodes wrap def/defmodule).
    pub definitions_are_calls: bool,
}

// ---------------------------------------------------------------------------
// Per-language static configs
// ---------------------------------------------------------------------------

static RUST: LazyLock<LanguageConfig> = LazyLock::new(|| LanguageConfig {
    language: tree_sitter_rust::LANGUAGE.into(),
    identifier_types: &["identifier", "type_identifier", "field_identifier"],
    definition_types: &[
        "function_item",
        "struct_item",
        "enum_item",
        "impl_item",
        "trait_item",
        "type_item",
        "const_item",
        "static_item",
        "mod_item",
        "macro_definition",
    ],
    display_name: "Rust",
    definitions_are_calls: false,
});

static PYTHON: LazyLock<LanguageConfig> = LazyLock::new(|| LanguageConfig {
    language: tree_sitter_python::LANGUAGE.into(),
    identifier_types: &["identifier"],
    definition_types: &["function_definition", "class_definition"],
    display_name: "Python",
    definitions_are_calls: false,
});

static JAVASCRIPT: LazyLock<LanguageConfig> = LazyLock::new(|| LanguageConfig {
    language: tree_sitter_javascript::LANGUAGE.into(),
    identifier_types: &[
        "identifier",
        "property_identifier",
        "shorthand_property_identifier",
    ],
    definition_types: &[
        "function_declaration",
        "class_declaration",
        "lexical_declaration",
        "variable_declaration",
    ],
    display_name: "JavaScript",
    definitions_are_calls: false,
});

static TYPESCRIPT: LazyLock<LanguageConfig> = LazyLock::new(|| LanguageConfig {
    language: tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
    identifier_types: &[
        "identifier",
        "property_identifier",
        "shorthand_property_identifier",
        "type_identifier",
    ],
    definition_types: &[
        "function_declaration",
        "class_declaration",
        "lexical_declaration",
        "interface_declaration",
        "type_alias_declaration",
        "enum_declaration",
    ],
    display_name: "TypeScript",
    definitions_are_calls: false,
});

static TSX: LazyLock<LanguageConfig> = LazyLock::new(|| LanguageConfig {
    language: tree_sitter_typescript::LANGUAGE_TSX.into(),
    identifier_types: &[
        "identifier",
        "property_identifier",
        "shorthand_property_identifier",
        "type_identifier",
    ],
    definition_types: &[
        "function_declaration",
        "class_declaration",
        "lexical_declaration",
        "interface_declaration",
        "type_alias_declaration",
        "enum_declaration",
    ],
    display_name: "TSX",
    definitions_are_calls: false,
});

static GO: LazyLock<LanguageConfig> = LazyLock::new(|| LanguageConfig {
    language: tree_sitter_go::LANGUAGE.into(),
    identifier_types: &["identifier", "type_identifier", "field_identifier"],
    definition_types: &[
        "function_declaration",
        "method_declaration",
        "type_declaration",
        "const_declaration",
        "var_declaration",
    ],
    display_name: "Go",
    definitions_are_calls: false,
});

static JAVA: LazyLock<LanguageConfig> = LazyLock::new(|| LanguageConfig {
    language: tree_sitter_java::LANGUAGE.into(),
    identifier_types: &["identifier", "type_identifier"],
    definition_types: &[
        "class_declaration",
        "method_declaration",
        "interface_declaration",
        "enum_declaration",
        "constructor_declaration",
        "record_declaration",
    ],
    display_name: "Java",
    definitions_are_calls: false,
});

static C: LazyLock<LanguageConfig> = LazyLock::new(|| LanguageConfig {
    language: tree_sitter_c::LANGUAGE.into(),
    identifier_types: &["identifier", "type_identifier", "field_identifier"],
    definition_types: &[
        "function_definition",
        "struct_specifier",
        "enum_specifier",
        "type_definition",
    ],
    display_name: "C",
    definitions_are_calls: false,
});

static CPP: LazyLock<LanguageConfig> = LazyLock::new(|| LanguageConfig {
    language: tree_sitter_cpp::LANGUAGE.into(),
    identifier_types: &[
        "identifier",
        "type_identifier",
        "field_identifier",
        "namespace_identifier",
    ],
    definition_types: &[
        "function_definition",
        "class_specifier",
        "struct_specifier",
        "enum_specifier",
        "namespace_definition",
        "type_definition",
        "template_declaration",
    ],
    display_name: "C++",
    definitions_are_calls: false,
});

static CSHARP: LazyLock<LanguageConfig> = LazyLock::new(|| LanguageConfig {
    language: tree_sitter_c_sharp::LANGUAGE.into(),
    identifier_types: &["identifier"],
    definition_types: &[
        "class_declaration",
        "struct_declaration",
        "interface_declaration",
        "enum_declaration",
        "method_declaration",
        "namespace_declaration",
        "record_declaration",
    ],
    display_name: "C#",
    definitions_are_calls: false,
});

static RUBY: LazyLock<LanguageConfig> = LazyLock::new(|| LanguageConfig {
    language: tree_sitter_ruby::LANGUAGE.into(),
    identifier_types: &["identifier", "constant"],
    definition_types: &["method", "singleton_method", "class", "module"],
    display_name: "Ruby",
    definitions_are_calls: false,
});

static KOTLIN: LazyLock<LanguageConfig> = LazyLock::new(|| LanguageConfig {
    language: tree_sitter_kotlin_ng::LANGUAGE.into(),
    identifier_types: &["simple_identifier"],
    definition_types: &[
        "function_declaration",
        "class_declaration",
        "object_declaration",
        "property_declaration",
        "type_alias",
    ],
    display_name: "Kotlin",
    definitions_are_calls: false,
});

static SWIFT: LazyLock<LanguageConfig> = LazyLock::new(|| LanguageConfig {
    language: tree_sitter_swift::LANGUAGE.into(),
    identifier_types: &["simple_identifier", "type_identifier"],
    definition_types: &[
        "function_declaration",
        "class_declaration",
        "struct_declaration",
        "enum_declaration",
        "protocol_declaration",
        "typealias_declaration",
    ],
    display_name: "Swift",
    definitions_are_calls: false,
});

static ZIG: LazyLock<LanguageConfig> = LazyLock::new(|| LanguageConfig {
    language: tree_sitter_zig::LANGUAGE.into(),
    identifier_types: &["identifier"],
    definition_types: &["fn_decl", "var_decl", "container_decl"],
    display_name: "Zig",
    definitions_are_calls: false,
});

static ELIXIR: LazyLock<LanguageConfig> = LazyLock::new(|| LanguageConfig {
    language: tree_sitter_elixir::LANGUAGE.into(),
    identifier_types: &["identifier", "atom"],
    definition_types: &["call"],
    display_name: "Elixir",
    definitions_are_calls: true,
});

static ERLANG: LazyLock<LanguageConfig> = LazyLock::new(|| LanguageConfig {
    language: tree_sitter_erlang::LANGUAGE.into(),
    identifier_types: &["atom", "variable"],
    definition_types: &[
        "function_clause",
        "type_declaration",
        "record_declaration",
        "macro_definition",
    ],
    display_name: "Erlang",
    definitions_are_calls: false,
});

static OCAML: LazyLock<LanguageConfig> = LazyLock::new(|| LanguageConfig {
    language: tree_sitter_ocaml::LANGUAGE_OCAML.into(),
    identifier_types: &[
        "value_name",
        "constructor_name",
        "type_constructor",
        "module_name",
    ],
    definition_types: &[
        "value_definition",
        "type_definition",
        "module_definition",
        "module_type_definition",
    ],
    display_name: "OCaml",
    definitions_are_calls: false,
});

static HASKELL: LazyLock<LanguageConfig> = LazyLock::new(|| LanguageConfig {
    language: tree_sitter_haskell::LANGUAGE.into(),
    identifier_types: &["variable", "constructor", "type_variable"],
    definition_types: &[
        "function",
        "type_alias",
        "newtype",
        "data_type",
        "class_declaration",
        "instance_declaration",
    ],
    display_name: "Haskell",
    definitions_are_calls: false,
});

static NIX: LazyLock<LanguageConfig> = LazyLock::new(|| LanguageConfig {
    language: tree_sitter_nix::LANGUAGE.into(),
    identifier_types: &["identifier", "attrpath"],
    definition_types: &["binding", "inherit"],
    display_name: "Nix",
    definitions_are_calls: false,
});

static JSON: LazyLock<LanguageConfig> = LazyLock::new(|| LanguageConfig {
    language: tree_sitter_json::LANGUAGE.into(),
    identifier_types: &[], // rename not supported for JSON
    definition_types: &["pair"],
    display_name: "JSON",
    definitions_are_calls: false,
});

// ---------------------------------------------------------------------------
// Shared constants and helpers
// ---------------------------------------------------------------------------

/// Maximum file size for AST parsing (10 MB).
pub const MAX_FILE_SIZE: u64 = 10 * 1024 * 1024;

/// Maximum number of files to process in a single operation.
pub const MAX_FILES: usize = 500;

/// Parsed file ready for AST operations.
pub struct ParsedFile {
    pub tree: tree_sitter::Tree,
    pub source: String,
    pub rel_path: String,
}

/// Try to parse a file for AST operations.
///
/// Returns `None` if the file should be skipped (wrong extension, binary,
/// too large, parse failure).  Callers can pass `require_identifiers: true`
/// to skip languages with no identifier types (e.g., JSON for rename).
pub fn try_parse_file(
    path: &std::path::Path,
    working_dir_canon: &std::path::Path,
    require_identifiers: bool,
) -> crate::error::Result<Option<(&'static LanguageConfig, ParsedFile)>> {
    let ext = match path.extension().and_then(|e| e.to_str()) {
        Some(e) => e,
        None => return Ok(None),
    };
    let config = match config_for_extension(ext) {
        Some(c) => c,
        None => return Ok(None),
    };
    if require_identifiers && config.identifier_types.is_empty() {
        return Ok(None);
    }
    let metadata = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(_) => return Ok(None),
    };
    if metadata.len() > MAX_FILE_SIZE {
        return Ok(None);
    }

    let source = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(_) => return Ok(None),
    };

    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&config.language)
        .map_err(|e| crate::error::DysonError::tool("ast", format!("parser setup: {e}")))?;

    let tree = match parser.parse(&source, None) {
        Some(t) => t,
        None => return Ok(None),
    };

    let rel_path = path
        .strip_prefix(working_dir_canon)
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| path.to_string_lossy().to_string());

    Ok(Some((
        config,
        ParsedFile {
            tree,
            source,
            rel_path,
        },
    )))
}

/// Create a directory walker with standard settings (.gitignore, etc.).
pub fn walk_dir(dir: &std::path::Path) -> ignore::Walk {
    let mut builder = ignore::WalkBuilder::new(dir);
    builder.hidden(false);
    builder.git_ignore(true);
    builder.git_global(true);
    builder.build()
}

// ---------------------------------------------------------------------------
// Language name → LanguageConfig lookup
// ---------------------------------------------------------------------------

/// Map a language name or common alias to a language config.
///
/// Accepts display names (`"Rust"`, `"C++"`), lowercase names (`"rust"`,
/// `"python"`), common abbreviations (`"js"`, `"ts"`, `"rb"`), and file
/// extensions (`"rs"`, `"py"`).  Case-insensitive.
///
/// Returns `None` for unrecognized names.
pub fn config_for_language_name(name: &str) -> Option<&'static LanguageConfig> {
    match name.to_ascii_lowercase().as_str() {
        "rust" | "rs" => Some(&RUST),
        "python" | "py" | "pyi" => Some(&PYTHON),
        "javascript" | "js" | "jsx" | "mjs" | "cjs" => Some(&JAVASCRIPT),
        "typescript" | "ts" | "mts" | "cts" => Some(&TYPESCRIPT),
        "tsx" => Some(&TSX),
        "go" | "golang" => Some(&GO),
        "java" => Some(&JAVA),
        "c" | "h" => Some(&C),
        "cpp" | "c++" | "cc" | "cxx" | "hpp" | "hxx" => Some(&CPP),
        "csharp" | "c#" | "cs" => Some(&CSHARP),
        "ruby" | "rb" => Some(&RUBY),
        "kotlin" | "kt" | "kts" => Some(&KOTLIN),
        "swift" => Some(&SWIFT),
        "zig" => Some(&ZIG),
        "elixir" | "ex" | "exs" => Some(&ELIXIR),
        "erlang" | "erl" | "hrl" => Some(&ERLANG),
        "ocaml" | "ml" | "mli" => Some(&OCAML),
        "haskell" | "hs" => Some(&HASKELL),
        "nix" => Some(&NIX),
        "json" => Some(&JSON),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Extension → LanguageConfig lookup
// ---------------------------------------------------------------------------

/// Map a file extension (without the leading dot) to a language config.
///
/// Returns `None` for unrecognized extensions — callers should skip silently
/// (or use a text fallback).
pub fn config_for_extension(ext: &str) -> Option<&'static LanguageConfig> {
    match ext {
        "rs" => Some(&RUST),
        "py" | "pyi" => Some(&PYTHON),
        "js" | "mjs" | "cjs" | "jsx" => Some(&JAVASCRIPT),
        "ts" | "mts" | "cts" => Some(&TYPESCRIPT),
        "tsx" => Some(&TSX),
        "go" => Some(&GO),
        "java" => Some(&JAVA),
        "c" | "h" => Some(&C),
        "cpp" | "cc" | "cxx" | "hpp" | "hxx" => Some(&CPP),
        "cs" => Some(&CSHARP),
        "rb" => Some(&RUBY),
        "kt" | "kts" => Some(&KOTLIN),
        "swift" => Some(&SWIFT),
        "zig" => Some(&ZIG),
        "ex" | "exs" => Some(&ELIXIR),
        "erl" | "hrl" => Some(&ERLANG),
        "ml" | "mli" => Some(&OCAML),
        "hs" => Some(&HASKELL),
        "nix" => Some(&NIX),
        "json" => Some(&JSON),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_extensions_resolve() {
        let extensions = [
            "rs", "py", "pyi", "js", "mjs", "cjs", "jsx", "ts", "mts", "cts", "tsx", "go", "java",
            "c", "h", "cpp", "cc", "cxx", "hpp", "hxx", "cs", "rb", "kt", "kts", "swift", "zig",
            "ex", "exs", "erl", "hrl", "ml", "mli", "hs", "nix", "json",
        ];
        for ext in extensions {
            assert!(
                config_for_extension(ext).is_some(),
                "expected config for extension '.{ext}'"
            );
        }
    }

    #[test]
    fn unknown_extension_returns_none() {
        assert!(config_for_extension("csv").is_none());
        assert!(config_for_extension("txt").is_none());
        assert!(config_for_extension("md").is_none());
        assert!(config_for_extension("").is_none());
    }

    #[test]
    fn display_names_correct() {
        assert_eq!(config_for_extension("rs").unwrap().display_name, "Rust");
        assert_eq!(config_for_extension("py").unwrap().display_name, "Python");
        assert_eq!(
            config_for_extension("ts").unwrap().display_name,
            "TypeScript"
        );
        assert_eq!(config_for_extension("tsx").unwrap().display_name, "TSX");
        assert_eq!(config_for_extension("cs").unwrap().display_name, "C#");
        assert_eq!(config_for_extension("kt").unwrap().display_name, "Kotlin");
        assert_eq!(config_for_extension("hs").unwrap().display_name, "Haskell");
    }

    #[test]
    fn json_has_no_identifier_types() {
        let config = config_for_extension("json").unwrap();
        assert!(config.identifier_types.is_empty());
    }

    // -------------------------------------------------------------------
    // config_for_language_name tests
    // -------------------------------------------------------------------

    #[test]
    fn language_name_resolves_all_languages() {
        let names = [
            "rust", "python", "javascript", "typescript", "tsx", "go", "java",
            "c", "cpp", "csharp", "ruby", "kotlin", "swift", "zig", "elixir",
            "erlang", "ocaml", "haskell", "nix", "json",
        ];
        for name in names {
            assert!(
                config_for_language_name(name).is_some(),
                "expected config for language name '{name}'"
            );
        }
    }

    #[test]
    fn language_name_case_insensitive() {
        assert!(config_for_language_name("Rust").is_some());
        assert!(config_for_language_name("PYTHON").is_some());
        assert!(config_for_language_name("JavaScript").is_some());
        assert!(config_for_language_name("C++").is_some());
        assert!(config_for_language_name("C#").is_some());
    }

    #[test]
    fn language_name_aliases() {
        // Common aliases should resolve to the same language.
        assert_eq!(
            config_for_language_name("js").unwrap().display_name,
            "JavaScript"
        );
        assert_eq!(
            config_for_language_name("ts").unwrap().display_name,
            "TypeScript"
        );
        assert_eq!(
            config_for_language_name("py").unwrap().display_name,
            "Python"
        );
        assert_eq!(
            config_for_language_name("rb").unwrap().display_name,
            "Ruby"
        );
        assert_eq!(
            config_for_language_name("rs").unwrap().display_name,
            "Rust"
        );
        assert_eq!(
            config_for_language_name("golang").unwrap().display_name,
            "Go"
        );
        assert_eq!(
            config_for_language_name("kt").unwrap().display_name,
            "Kotlin"
        );
        assert_eq!(
            config_for_language_name("hs").unwrap().display_name,
            "Haskell"
        );
    }

    #[test]
    fn language_name_unknown_returns_none() {
        assert!(config_for_language_name("fortran").is_none());
        assert!(config_for_language_name("brainfuck").is_none());
        assert!(config_for_language_name("").is_none());
    }
}
