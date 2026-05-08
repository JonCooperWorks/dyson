use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use ignore::WalkBuilder;

use super::rules::{
    scan_build_gradle, scan_cabal_file, scan_cargo_toml, scan_composer_json, scan_dotnet_project,
    scan_dune_file, scan_gemfile, scan_go_mod, scan_mix_exs, scan_package_json, scan_package_swift,
    scan_pom_xml, scan_pyproject_toml, scan_rebar_config, scan_requirements_txt, scan_rockspec,
};
use super::{Detection, Framework, Language};

/// Maximum directory depth walked when counting manifests inside the
/// scoped review root.  `ignore::WalkBuilder` counts the root itself as
/// depth 0, so 3 yields files at root + two subdir levels — enough for
/// monorepo child manifests (`packages/child/package.json`) without
/// traversing `node_modules`-sized trees.
const DOWN_WALK_DEPTH: usize = 3;

/// How many ancestors above the scoped path to probe for root-level
/// manifests.  Expensive-live reviews scope to e.g. `repo/routes/`; the
/// manifest lives in `repo/`.  5 covers typical nesting.
const UP_WALK_DEPTH: usize = 5;

/// Walk `root` (down up to [`DOWN_WALK_DEPTH`] levels) AND its ancestors
/// (up to [`UP_WALK_DEPTH`]) to find manifests.  Downward walk handles
/// repos pointed at their root; upward walk handles scoped reviews like
/// `repo/routes/` where the manifest lives in `repo/`.
pub fn detect_repo(root: &Path) -> Detection {
    let mut lang_counts: BTreeMap<Language, usize> = BTreeMap::new();
    let mut frameworks: Vec<Framework> = Vec::new();
    let mut seen_frameworks: HashSet<Framework> = HashSet::new();

    // Ancestor walk: check each ancestor dir for root-level manifests.
    // We only inspect the dir itself, not recurse — ancestors are likely
    // large (~repo root), and a single manifest file is enough signal.
    let mut ancestor = root.parent();
    for _ in 0..UP_WALK_DEPTH {
        let Some(dir) = ancestor else { break };
        inspect_dir_nonrecursive(dir, &mut lang_counts, &mut frameworks, &mut seen_frameworks);
        ancestor = dir.parent();
    }

    // Downward walk from the scoped path.
    walk_down(
        root,
        &mut lang_counts,
        &mut frameworks,
        &mut seen_frameworks,
    );

    // Rank by count desc; tiebreak by enum declaration order via Ord.
    // BTreeMap gives iteration sorted by key (Language's derived Ord);
    // a stable sort on count inherits that tiebreak.
    let mut ranked: Vec<(Language, usize)> = lang_counts.into_iter().collect();
    ranked.sort_by_key(|(_, count)| std::cmp::Reverse(*count));

    let languages: Vec<Language> = ranked.into_iter().map(|(lang, _)| lang).collect();

    Detection {
        languages,
        frameworks,
    }
}

/// Walk `dir` down to [`DOWN_WALK_DEPTH`] levels.  Uses
/// `ignore::WalkBuilder` so `.gitignore`, hidden files, and `.git` are
/// skipped automatically; supplementary [`is_skippable_dir`] covers
/// big dependency / build directories in repos shipped without a
/// `.gitignore` (tarball drops, fresh scaffolds).
fn walk_down(
    dir: &Path,
    counts: &mut BTreeMap<Language, usize>,
    frameworks: &mut Vec<Framework>,
    seen: &mut HashSet<Framework>,
) {
    let mut builder = WalkBuilder::new(dir);
    builder
        .max_depth(Some(DOWN_WALK_DEPTH))
        // `.gitignore` should apply even when the target is a bare
        // tarball extract (no `.git`); default `require_git(true)`
        // silently ignores the file in that case.
        .require_git(false)
        .filter_entry(|e| {
            // Only filter directories — files pass through.
            if e.file_type().is_some_and(|ft| ft.is_dir()) {
                !is_skippable_dir(e.path())
            } else {
                true
            }
        });
    for entry in builder.build().flatten() {
        if entry.file_type().is_some_and(|ft| ft.is_file()) {
            inspect_file(entry.path(), counts, frameworks, seen);
        }
    }
}

/// `walk_down` without recursion — used on ancestors where we only care
/// about files directly inside `dir`.
fn inspect_dir_nonrecursive(
    dir: &Path,
    counts: &mut BTreeMap<Language, usize>,
    frameworks: &mut Vec<Framework>,
    seen: &mut HashSet<Framework>,
) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if entry.file_type().is_ok_and(|ft| ft.is_file()) {
            inspect_file(&path, counts, frameworks, seen);
        }
    }
}

fn is_skippable_dir(p: &Path) -> bool {
    matches!(
        p.file_name().and_then(|n| n.to_str()),
        Some(
            "node_modules"
                | "target"
                | ".git"
                | ".venv"
                | "venv"
                | "__pycache__"
                | "dist"
                | "build"
                | "vendor"
                | ".next"
                | ".cache",
        )
    )
}

/// If `path` is a recognised manifest, bump its language count and scan
/// its contents for framework markers.  Malformed files are ignored —
/// manifest detection is a heuristic, not a source of truth.
fn inspect_file(
    path: &Path,
    counts: &mut BTreeMap<Language, usize>,
    frameworks: &mut Vec<Framework>,
    seen: &mut HashSet<Framework>,
) {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return;
    };
    let lower = name.to_ascii_lowercase();

    let lang = match lower.as_str() {
        "cargo.toml" => Some(Language::Rust),
        "package.json" => Some(Language::JavaScript),
        "pyproject.toml" | "requirements.txt" => Some(Language::Python),
        "go.mod" => Some(Language::Go),
        "gemfile" | "gemfile.lock" => Some(Language::Ruby),
        // `.kts` extension is the Kotlin DSL — Kotlin-first project.
        // Plain `build.gradle` (Groovy) typically indicates a Java
        // project; keep the old mapping for that.
        "build.gradle.kts" => Some(Language::Kotlin),
        "pom.xml" | "build.gradle" => Some(Language::Java),
        "composer.json" | "composer.lock" => Some(Language::Php),
        "mix.exs" | "mix.lock" => Some(Language::Elixir),
        "rebar.config" => Some(Language::Erlang),
        "package.swift" => Some(Language::Swift),
        "stack.yaml" | "cabal.project" => Some(Language::Haskell),
        "dune-project" | "dune" => Some(Language::Ocaml),
        "build.zig" | "build.zig.zon" => Some(Language::Zig),
        "flake.nix" | "default.nix" | "shell.nix" => Some(Language::Nix),
        "conanfile.txt" | "conanfile.py" | "cmakelists.txt" => Some(Language::Cpp),
        _ => {
            if lower.starts_with("requirements") && lower.ends_with(".txt") {
                Some(Language::Python)
            } else if lower.ends_with(".csproj")
                || lower.ends_with(".fsproj")
                || lower.ends_with(".vbproj")
            {
                Some(Language::CSharp)
            } else if lower.ends_with(".cabal") {
                Some(Language::Haskell)
            } else if lower.ends_with(".rockspec") {
                Some(Language::Lua)
            } else {
                None
            }
        }
    };

    let Some(lang) = lang else { return };
    *counts.entry(lang).or_default() += 1;

    // Frameworks: only parse the few manifests where an O(1) shallow
    // match on dependency names is cheap and high-signal.
    let Ok(contents) = std::fs::read_to_string(path) else {
        return;
    };
    match (lang, lower.as_str()) {
        (Language::JavaScript, "package.json") => {
            scan_package_json(&contents, frameworks, seen);
        }
        (Language::Python, "pyproject.toml") => {
            scan_pyproject_toml(&contents, frameworks, seen);
        }
        (Language::Python, _) if lower.ends_with(".txt") => {
            scan_requirements_txt(&contents, frameworks, seen);
        }
        (Language::Rust, "cargo.toml") => {
            scan_cargo_toml(&contents, frameworks, seen);
        }
        (Language::Ruby, "gemfile") => scan_gemfile(&contents, frameworks, seen),
        (Language::Java, "pom.xml") => scan_pom_xml(&contents, frameworks, seen),
        (Language::Java, "build.gradle") => scan_build_gradle(&contents, frameworks, seen),
        (Language::Kotlin, "build.gradle.kts") => scan_build_gradle(&contents, frameworks, seen),
        (Language::Swift, "package.swift") => scan_package_swift(&contents, frameworks, seen),
        (Language::Php, "composer.json") => scan_composer_json(&contents, frameworks, seen),
        (Language::Elixir, "mix.exs") => scan_mix_exs(&contents, frameworks, seen),
        (Language::CSharp, _) => scan_dotnet_project(&contents, frameworks, seen),
        (Language::Go, "go.mod") => scan_go_mod(&contents, frameworks, seen),
        (Language::Haskell, _) if lower.ends_with(".cabal") => {
            scan_cabal_file(&contents, frameworks, seen)
        }
        (Language::Ocaml, "dune") => scan_dune_file(&contents, frameworks, seen),
        (Language::Erlang, "rebar.config") => scan_rebar_config(&contents, frameworks, seen),
        (Language::Lua, _) if lower.ends_with(".rockspec") => {
            scan_rockspec(&contents, frameworks, seen)
        }
        _ => {}
    }
}
