// ===========================================================================
// Language registry — maps file extensions to tree-sitter grammars.
//
// Each language gets a static LanguageConfig with:
//   - The tree-sitter Language (from the grammar crate)
//   - Identifier node types (for rename_symbol)
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
    pub display_name: &'static str,
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
});

static PYTHON: LazyLock<LanguageConfig> = LazyLock::new(|| LanguageConfig {
    language: tree_sitter_python::LANGUAGE.into(),
    identifier_types: &["identifier"],
    definition_types: &["function_definition", "class_definition"],
    display_name: "Python",
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
});

static RUBY: LazyLock<LanguageConfig> = LazyLock::new(|| LanguageConfig {
    language: tree_sitter_ruby::LANGUAGE.into(),
    identifier_types: &["identifier", "constant"],
    definition_types: &["method", "singleton_method", "class", "module"],
    display_name: "Ruby",
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
});

static ZIG: LazyLock<LanguageConfig> = LazyLock::new(|| LanguageConfig {
    language: tree_sitter_zig::LANGUAGE.into(),
    identifier_types: &["identifier"],
    definition_types: &["fn_decl", "var_decl", "container_decl"],
    display_name: "Zig",
});

static ELIXIR: LazyLock<LanguageConfig> = LazyLock::new(|| LanguageConfig {
    language: tree_sitter_elixir::LANGUAGE.into(),
    identifier_types: &["identifier", "atom"],
    definition_types: &["call"],
    display_name: "Elixir",
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
});

static NIX: LazyLock<LanguageConfig> = LazyLock::new(|| LanguageConfig {
    language: tree_sitter_nix::LANGUAGE.into(),
    identifier_types: &["identifier", "attrpath"],
    definition_types: &["binding", "inherit"],
    display_name: "Nix",
});

static JSON: LazyLock<LanguageConfig> = LazyLock::new(|| LanguageConfig {
    language: tree_sitter_json::LANGUAGE.into(),
    identifier_types: &[], // rename not supported for JSON
    definition_types: &["pair"],
    display_name: "JSON",
});

// ---------------------------------------------------------------------------
// Extension → LanguageConfig lookup
// ---------------------------------------------------------------------------

/// Map a file extension (without the leading dot) to a language config.
///
/// Returns `None` for unrecognized extensions — callers should skip silently.
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
            "rs", "py", "pyi", "js", "mjs", "cjs", "jsx", "ts", "mts", "cts", "tsx", "go",
            "java", "c", "h", "cpp", "cc", "cxx", "hpp", "hxx", "cs", "rb", "kt", "kts",
            "swift", "zig", "ex", "exs", "erl", "hrl", "ml", "mli", "hs", "nix", "json",
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
        assert_eq!(config_for_extension("ts").unwrap().display_name, "TypeScript");
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
}
