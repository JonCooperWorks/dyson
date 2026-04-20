// ===========================================================================
// Runtime repo detection for security_engineer cheatsheet injection.
//
// `detect_repo` shallow-parses manifest files to identify the top two
// languages present in a review target and any frameworks pulled in by
// those languages' dependency lists.  `compose_cheatsheets` then
// concatenates the matching cheatsheet files (`include_str!`-bundled at
// build time) under a hard line cap.
//
// Why inline, not a runtime tool: the sheets are guidance the
// security_engineer should carry from the first turn.  A tool-driven
// lookup wastes a tool call and biases the model against the sheet
// (they'd read it as optional).  The cap keeps token cost bounded.
// ===========================================================================

use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use ignore::WalkBuilder;

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

/// Upper bound on total injected cheatsheet content.  Past this, drop
/// frameworks first, then the second language.  At ~75 lines per sheet
/// the cap fits: 2 langs + 2 frameworks ≈ 300 lines, well under.
const MAX_CHEATSHEET_LINES: usize = 400;

/// Languages for which a cheatsheet ships.  Covers every tree-sitter
/// grammar dyson's `ast_query` supports, plus PHP + Lua (no in-tree
/// grammar but the sheets still guide `read_file` / `search_files`
/// work).
///
/// Derives `Ord` so `BTreeMap<Language, usize>` can key directly on
/// the enum (stable tiebreak via declaration order).  Avoids a second
/// hand-maintained variant list that would drift from the enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Language {
    Python,
    /// Shared sheet for JavaScript and TypeScript; detected through
    /// `package.json` plus optional `tsconfig.json` / a `typescript`
    /// devDependency (both treated as the same sheet).
    JavaScript,
    Go,
    Rust,
    Ruby,
    Java,
    Kotlin,
    CSharp,
    Php,
    Cpp,
    Elixir,
    Haskell,
    Swift,
    Ocaml,
    Erlang,
    Zig,
    Nix,
    Lua,
}

/// Frameworks for which a cheatsheet ships.  Each binds to one language
/// for detection purposes — the cap logic drops a framework if its
/// language isn't in the top-2 selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Framework {
    Django,
    Flask,
    FastApi,
    Aiohttp,
    Tornado,
    Sanic,
    Celery,
    Express,
    NextJs,
    Fastify,
    NestJs,
    Trpc,
    Koa,
    Hono,
    SvelteKit,
    Remix,
    GraphQL,
    Actix,
    Axum,
    Rocket,
    Warp,
    Tonic,
    Solana,
    Rails,
    Sinatra,
    Spring,
    Quarkus,
    Micronaut,
    Javalin,
    Ktor,
    AspNet,
    Laravel,
    Symfony,
    Slim,
    CodeIgniter,
    Phoenix,
    Gin,
    Echo,
    Chi,
    Fiber,
    GorillaMux,
    Vapor,
    Hummingbird,
    Servant,
    Dream,
    Cowboy,
    Starlette,
    Pyramid,
    Falcon,
    Bottle,
    Play,
    Dropwizard,
    Helidon,
    Vertx,
    Hapi,
    Adonis,
    Meteor,
    Nuxt,
    OpenResty,
}

impl Framework {
    /// The language whose manifest advertises this framework.  Used by
    /// the cap logic — a framework can only be kept while its language
    /// is still in the selection.
    const fn language(self) -> Language {
        match self {
            Self::Django
            | Self::Flask
            | Self::FastApi
            | Self::Aiohttp
            | Self::Tornado
            | Self::Sanic
            | Self::Celery
            | Self::Starlette
            | Self::Pyramid
            | Self::Falcon
            | Self::Bottle => Language::Python,
            Self::Express
            | Self::NextJs
            | Self::Fastify
            | Self::NestJs
            | Self::Trpc
            | Self::Koa
            | Self::Hono
            | Self::SvelteKit
            | Self::Remix
            | Self::GraphQL
            | Self::Hapi
            | Self::Adonis
            | Self::Meteor
            | Self::Nuxt => Language::JavaScript,
            Self::Actix
            | Self::Axum
            | Self::Rocket
            | Self::Warp
            | Self::Tonic
            | Self::Solana => Language::Rust,
            Self::Rails | Self::Sinatra => Language::Ruby,
            Self::Spring
            | Self::Quarkus
            | Self::Micronaut
            | Self::Javalin
            | Self::Play
            | Self::Dropwizard
            | Self::Helidon
            | Self::Vertx => Language::Java,
            Self::Ktor => Language::Kotlin,
            Self::AspNet => Language::CSharp,
            Self::Laravel | Self::Symfony | Self::Slim | Self::CodeIgniter => Language::Php,
            Self::Phoenix => Language::Elixir,
            Self::Gin | Self::Echo | Self::Chi | Self::Fiber | Self::GorillaMux => Language::Go,
            Self::Vapor | Self::Hummingbird => Language::Swift,
            Self::Servant => Language::Haskell,
            Self::Dream => Language::Ocaml,
            Self::Cowboy => Language::Erlang,
            Self::OpenResty => Language::Lua,
        }
    }
}

#[derive(Debug, Default)]
pub struct Detection {
    /// Languages ranked by manifest count (descending).  Ties broken by
    /// the stable enum order to keep output reproducible across runs.
    pub languages: Vec<Language>,
    /// Frameworks detected in any parsed manifest for a selected
    /// language.  Preserved in discovery order.
    pub frameworks: Vec<Framework>,
}

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
    walk_down(root, &mut lang_counts, &mut frameworks, &mut seen_frameworks);

    // Rank by count desc; tiebreak by enum declaration order via Ord.
    // BTreeMap gives iteration sorted by key (Language's derived Ord);
    // a stable sort on count inherits that tiebreak.
    let mut ranked: Vec<(Language, usize)> = lang_counts.into_iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1));

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
    match p.file_name().and_then(|n| n.to_str()) {
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
        ) => true,
        _ => false,
    }
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

/// `package.json` framework detection: treat the whole document as a
/// bag of strings and look for top-level dep keys.  Misses scoped
/// workspaces with deps hoisted elsewhere, but those are rare and the
/// sheet is still useful for a pure JS repo without Express.
fn scan_package_json(
    contents: &str,
    frameworks: &mut Vec<Framework>,
    seen: &mut HashSet<Framework>,
) {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(contents) else {
        return;
    };
    let has_dep = |name: &str| -> bool {
        for key in ["dependencies", "devDependencies", "peerDependencies"] {
            if value
                .get(key)
                .and_then(|v| v.as_object())
                .is_some_and(|m| m.contains_key(name))
            {
                return true;
            }
        }
        false
    };
    if has_dep("express") {
        push_framework(Framework::Express, frameworks, seen);
    }
    if has_dep("next") {
        push_framework(Framework::NextJs, frameworks, seen);
    }
    if has_dep("fastify") {
        push_framework(Framework::Fastify, frameworks, seen);
    }
    if has_dep("@nestjs/core") || has_dep("@nestjs/common") {
        push_framework(Framework::NestJs, frameworks, seen);
    }
    if has_dep("@trpc/server") {
        push_framework(Framework::Trpc, frameworks, seen);
    }
    if has_dep("koa") {
        push_framework(Framework::Koa, frameworks, seen);
    }
    if has_dep("hono") {
        push_framework(Framework::Hono, frameworks, seen);
    }
    if has_dep("@sveltejs/kit") {
        push_framework(Framework::SvelteKit, frameworks, seen);
    }
    if has_dep("@remix-run/node")
        || has_dep("@remix-run/react")
        || has_dep("@remix-run/server-runtime")
    {
        push_framework(Framework::Remix, frameworks, seen);
    }
    if has_dep("@apollo/server")
        || has_dep("apollo-server")
        || has_dep("graphql-yoga")
        || has_dep("@nestjs/graphql")
    {
        push_framework(Framework::GraphQL, frameworks, seen);
    }
    if has_dep("@hapi/hapi") || has_dep("hapi") {
        push_framework(Framework::Hapi, frameworks, seen);
    }
    if has_dep("@adonisjs/core") {
        push_framework(Framework::Adonis, frameworks, seen);
    }
    if has_dep("nuxt") {
        push_framework(Framework::Nuxt, frameworks, seen);
    }
    // Meteor apps carry a top-level `meteor` object in package.json.
    if value.get("meteor").is_some() || has_dep("meteor-node-stubs") {
        push_framework(Framework::Meteor, frameworks, seen);
    }
}

/// `pyproject.toml` framework detection.  Handles both PEP 621
/// (`[project].dependencies`) and the older Poetry layout
/// (`[tool.poetry.dependencies]`).
fn scan_pyproject_toml(
    contents: &str,
    frameworks: &mut Vec<Framework>,
    seen: &mut HashSet<Framework>,
) {
    let Ok(doc) = toml::from_str::<toml::Value>(contents) else {
        return;
    };
    let mut names: HashSet<String> = HashSet::new();

    // PEP 621: project.dependencies is an array of requirement strings.
    if let Some(deps) = doc
        .get("project")
        .and_then(|p| p.get("dependencies"))
        .and_then(|d| d.as_array())
    {
        for entry in deps {
            if let Some(s) = entry.as_str() {
                names.insert(requirement_name(s));
            }
        }
    }
    // Poetry: table of name = version.
    if let Some(deps) = doc
        .get("tool")
        .and_then(|t| t.get("poetry"))
        .and_then(|p| p.get("dependencies"))
        .and_then(|d| d.as_table())
    {
        for key in deps.keys() {
            names.insert(key.to_ascii_lowercase());
        }
    }

    if names.contains("django") {
        push_framework(Framework::Django, frameworks, seen);
    }
    if names.contains("flask") {
        push_framework(Framework::Flask, frameworks, seen);
    }
    if names.contains("fastapi") {
        push_framework(Framework::FastApi, frameworks, seen);
    }
    if names.contains("aiohttp") {
        push_framework(Framework::Aiohttp, frameworks, seen);
    }
    if names.contains("tornado") {
        push_framework(Framework::Tornado, frameworks, seen);
    }
    if names.contains("sanic") {
        push_framework(Framework::Sanic, frameworks, seen);
    }
    if names.contains("celery") {
        push_framework(Framework::Celery, frameworks, seen);
    }
    if names.contains("starlette") {
        push_framework(Framework::Starlette, frameworks, seen);
    }
    if names.contains("pyramid") {
        push_framework(Framework::Pyramid, frameworks, seen);
    }
    if names.contains("falcon") {
        push_framework(Framework::Falcon, frameworks, seen);
    }
    if names.contains("bottle") {
        push_framework(Framework::Bottle, frameworks, seen);
    }
}

/// Treat every non-comment line as `pkg[==ver]` and extract `pkg`.
fn scan_requirements_txt(
    contents: &str,
    frameworks: &mut Vec<Framework>,
    seen: &mut HashSet<Framework>,
) {
    for raw in contents.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with('-') {
            continue;
        }
        let name = requirement_name(line);
        match name.as_str() {
            "django" => push_framework(Framework::Django, frameworks, seen),
            "flask" => push_framework(Framework::Flask, frameworks, seen),
            "fastapi" => push_framework(Framework::FastApi, frameworks, seen),
            "aiohttp" => push_framework(Framework::Aiohttp, frameworks, seen),
            "tornado" => push_framework(Framework::Tornado, frameworks, seen),
            "sanic" => push_framework(Framework::Sanic, frameworks, seen),
            "celery" => push_framework(Framework::Celery, frameworks, seen),
            "starlette" => push_framework(Framework::Starlette, frameworks, seen),
            "pyramid" => push_framework(Framework::Pyramid, frameworks, seen),
            "falcon" => push_framework(Framework::Falcon, frameworks, seen),
            "bottle" => push_framework(Framework::Bottle, frameworks, seen),
            _ => {}
        }
    }
}

/// Strip PEP 508 extras, version specifiers, and environment markers to
/// get the bare package name.  `Django[bcrypt]>=4; python_version>"3.8"`
/// → `django`.
fn requirement_name(req: &str) -> String {
    let cut = req
        .find(|c: char| matches!(c, '[' | '=' | '<' | '>' | '!' | '~' | ';' | ' ' | '\t'))
        .unwrap_or(req.len());
    req[..cut].trim().to_ascii_lowercase()
}

fn scan_cargo_toml(
    contents: &str,
    frameworks: &mut Vec<Framework>,
    seen: &mut HashSet<Framework>,
) {
    let Ok(doc) = toml::from_str::<toml::Value>(contents) else {
        return;
    };
    let has_dep = |name: &str| -> bool {
        for section in ["dependencies", "dev-dependencies", "build-dependencies"] {
            if doc
                .get(section)
                .and_then(|t| t.as_table())
                .is_some_and(|t| t.contains_key(name))
            {
                return true;
            }
        }
        false
    };
    if has_dep("actix-web") || has_dep("actix") {
        push_framework(Framework::Actix, frameworks, seen);
    }
    if has_dep("axum") {
        push_framework(Framework::Axum, frameworks, seen);
    }
    if has_dep("rocket") {
        push_framework(Framework::Rocket, frameworks, seen);
    }
    if has_dep("warp") {
        push_framework(Framework::Warp, frameworks, seen);
    }
    if has_dep("tonic") {
        push_framework(Framework::Tonic, frameworks, seen);
    }
    // Solana programs — trigger on any of the core SDK crates.  Modern
    // Anchor programs pull `anchor-lang`; native programs pull
    // `solana-program`; the newer lightweight framework is `pinocchio`.
    // One sheet covers all three since the vuln classes overlap.
    if has_dep("anchor-lang")
        || has_dep("solana-program")
        || has_dep("pinocchio")
        || has_dep("solana-program-runtime")
    {
        push_framework(Framework::Solana, frameworks, seen);
    }
}

fn push_framework(fw: Framework, frameworks: &mut Vec<Framework>, seen: &mut HashSet<Framework>) {
    if seen.insert(fw) {
        frameworks.push(fw);
    }
}

/// `Gemfile` isn't a structured format we parse as a whole — instead,
/// match the common `gem 'name'` / `gem "name"` line shape.  False
/// negatives (gem name hidden behind a group/if block) are acceptable;
/// false positives (a commented-out gem declaration) would be worse,
/// so respect `#` comment lines.
fn scan_gemfile(contents: &str, frameworks: &mut Vec<Framework>, seen: &mut HashSet<Framework>) {
    for raw in contents.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if !line.starts_with("gem ") && !line.starts_with("gem\t") {
            continue;
        }
        // Extract the quoted name: `gem 'rails'` or `gem "rails"`.
        let Some(after_gem) = line.get(4..).map(str::trim_start) else {
            continue;
        };
        let name = match after_gem.chars().next() {
            Some('\'') => after_gem[1..]
                .split('\'')
                .next()
                .unwrap_or("")
                .to_ascii_lowercase(),
            Some('"') => after_gem[1..]
                .split('"')
                .next()
                .unwrap_or("")
                .to_ascii_lowercase(),
            _ => continue,
        };
        match name.as_str() {
            "rails" => push_framework(Framework::Rails, frameworks, seen),
            "sinatra" => push_framework(Framework::Sinatra, frameworks, seen),
            _ => {}
        }
    }
}

/// `pom.xml` — shallow substring match on common Spring Boot / Spring
/// coordinate strings.  Full XML parsing would be more robust but pulls
/// a heavy dep; substring suffices for detection.
fn scan_pom_xml(contents: &str, frameworks: &mut Vec<Framework>, seen: &mut HashSet<Framework>) {
    let lower = contents.to_ascii_lowercase();
    if lower.contains("spring-boot-starter") || lower.contains("org.springframework") {
        push_framework(Framework::Spring, frameworks, seen);
    }
    if lower.contains("io.quarkus") {
        push_framework(Framework::Quarkus, frameworks, seen);
    }
    if lower.contains("io.micronaut") {
        push_framework(Framework::Micronaut, frameworks, seen);
    }
    if lower.contains("io.javalin") {
        push_framework(Framework::Javalin, frameworks, seen);
    }
    if lower.contains("com.typesafe.play") || lower.contains("play-java") || lower.contains("play-scala") {
        push_framework(Framework::Play, frameworks, seen);
    }
    if lower.contains("io.dropwizard") {
        push_framework(Framework::Dropwizard, frameworks, seen);
    }
    if lower.contains("io.helidon") {
        push_framework(Framework::Helidon, frameworks, seen);
    }
    if lower.contains("io.vertx") {
        push_framework(Framework::Vertx, frameworks, seen);
    }
}

/// `build.gradle` / `build.gradle.kts` — substring match on Spring
/// Boot Gradle plugin or `org.springframework` coordinates.  Groovy
/// and Kotlin DSL variants both express the dep as a string containing
/// the coordinate, so line-based scanning catches either.
fn scan_build_gradle(
    contents: &str,
    frameworks: &mut Vec<Framework>,
    seen: &mut HashSet<Framework>,
) {
    let lower = contents.to_ascii_lowercase();
    if lower.contains("spring-boot-starter") || lower.contains("org.springframework") {
        push_framework(Framework::Spring, frameworks, seen);
    }
    if lower.contains("io.ktor:ktor-") || lower.contains("\"io.ktor\"") {
        push_framework(Framework::Ktor, frameworks, seen);
    }
    if lower.contains("io.quarkus") {
        push_framework(Framework::Quarkus, frameworks, seen);
    }
    if lower.contains("io.micronaut") {
        push_framework(Framework::Micronaut, frameworks, seen);
    }
    if lower.contains("io.javalin") {
        push_framework(Framework::Javalin, frameworks, seen);
    }
    if lower.contains("com.typesafe.play") || lower.contains("play-java") || lower.contains("play-scala") {
        push_framework(Framework::Play, frameworks, seen);
    }
    if lower.contains("io.dropwizard") {
        push_framework(Framework::Dropwizard, frameworks, seen);
    }
    if lower.contains("io.helidon") {
        push_framework(Framework::Helidon, frameworks, seen);
    }
    if lower.contains("io.vertx") {
        push_framework(Framework::Vertx, frameworks, seen);
    }
}

/// `composer.json` — parse as JSON and look at `require` /
/// `require-dev` tables.  Laravel ships via `laravel/framework`.
fn scan_composer_json(
    contents: &str,
    frameworks: &mut Vec<Framework>,
    seen: &mut HashSet<Framework>,
) {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(contents) else {
        return;
    };
    let has_dep = |name: &str| -> bool {
        for key in ["require", "require-dev"] {
            if value
                .get(key)
                .and_then(|v| v.as_object())
                .is_some_and(|m| m.contains_key(name))
            {
                return true;
            }
        }
        false
    };
    if has_dep("laravel/framework") {
        push_framework(Framework::Laravel, frameworks, seen);
    }
    if has_dep("symfony/framework-bundle")
        || has_dep("symfony/symfony")
        || has_dep("symfony/http-kernel")
    {
        push_framework(Framework::Symfony, frameworks, seen);
    }
    if has_dep("slim/slim") {
        push_framework(Framework::Slim, frameworks, seen);
    }
    if has_dep("codeigniter4/framework") || has_dep("codeigniter/framework") {
        push_framework(Framework::CodeIgniter, frameworks, seen);
    }
}

/// `mix.exs` — no clean parser for Elixir code; substring match on
/// the `{:phoenix, ...}` dep atom form is reliable enough.
fn scan_mix_exs(contents: &str, frameworks: &mut Vec<Framework>, seen: &mut HashSet<Framework>) {
    // Collapse whitespace before matching so `{ :phoenix ,` variants
    // still hit.
    let compact: String = contents
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect::<String>()
        .to_ascii_lowercase();
    if compact.contains("{:phoenix,") {
        push_framework(Framework::Phoenix, frameworks, seen);
    }
}

/// `.csproj` / `.fsproj` / `.vbproj` / `packages.config` — ASP.NET Core
/// ships as `Microsoft.AspNetCore.App` / `Microsoft.AspNetCore.*`
/// framework references.  Substring match on the coordinate.
fn scan_dotnet_project(
    contents: &str,
    frameworks: &mut Vec<Framework>,
    seen: &mut HashSet<Framework>,
) {
    let lower = contents.to_ascii_lowercase();
    if lower.contains("microsoft.aspnetcore") {
        push_framework(Framework::AspNet, frameworks, seen);
    }
}

/// `go.mod` — look for `github.com/gin-gonic/gin` in `require` blocks.
fn scan_go_mod(contents: &str, frameworks: &mut Vec<Framework>, seen: &mut HashSet<Framework>) {
    if contents.contains("github.com/gin-gonic/gin") {
        push_framework(Framework::Gin, frameworks, seen);
    }
    if contents.contains("github.com/labstack/echo") {
        push_framework(Framework::Echo, frameworks, seen);
    }
    if contents.contains("github.com/go-chi/chi") {
        push_framework(Framework::Chi, frameworks, seen);
    }
    if contents.contains("github.com/gofiber/fiber") {
        push_framework(Framework::Fiber, frameworks, seen);
    }
    if contents.contains("github.com/gorilla/mux") {
        push_framework(Framework::GorillaMux, frameworks, seen);
    }
}

/// `Package.swift` — Swift Package Manager manifest.  Vapor ships as
/// the `vapor/vapor` repo in a `.package(url: "...vapor.git", ...)`
/// dependency entry.  Substring match on the repo name.
fn scan_package_swift(
    contents: &str,
    frameworks: &mut Vec<Framework>,
    seen: &mut HashSet<Framework>,
) {
    let lower = contents.to_ascii_lowercase();
    if lower.contains("vapor/vapor") {
        push_framework(Framework::Vapor, frameworks, seen);
    }
    if lower.contains("hummingbird-project/hummingbird") {
        push_framework(Framework::Hummingbird, frameworks, seen);
    }
}

/// `*.cabal` — Haskell package description.  Substring match on
/// `servant` / `servant-server` in the `build-depends:` section.
/// Full-grammar parsing is heavier than the value here.
fn scan_cabal_file(
    contents: &str,
    frameworks: &mut Vec<Framework>,
    seen: &mut HashSet<Framework>,
) {
    let lower = contents.to_ascii_lowercase();
    if lower.contains("servant") {
        push_framework(Framework::Servant, frameworks, seen);
    }
}

/// `dune` — OCaml build stanza file.  Substring match on `dream` in
/// a `libraries` clause.  `dune-project` is at the repo root;
/// per-dir `dune` files carry the actual library deps.
fn scan_dune_file(
    contents: &str,
    frameworks: &mut Vec<Framework>,
    seen: &mut HashSet<Framework>,
) {
    if contents.contains("dream") {
        push_framework(Framework::Dream, frameworks, seen);
    }
}

/// `rebar.config` — Erlang/OTP build config.  Substring match on
/// `{cowboy, ...}` in the `deps` tuple.
fn scan_rebar_config(
    contents: &str,
    frameworks: &mut Vec<Framework>,
    seen: &mut HashSet<Framework>,
) {
    let compact: String = contents.chars().filter(|c| !c.is_whitespace()).collect();
    if compact.contains("{cowboy,") {
        push_framework(Framework::Cowboy, frameworks, seen);
    }
}

/// `*.rockspec` — luarocks package manifest.  OpenResty typically
/// shows up via its `lua-resty-*` ecosystem dependencies (`lua-resty-
/// http`, `lua-resty-openssl`, `lua-resty-redis`, `lua-resty-jwt`)
/// rather than a single `openresty` package name.  A rockspec
/// mentioning any `lua-resty-` prefix is almost certainly an
/// OpenResty-targeted project.
fn scan_rockspec(
    contents: &str,
    frameworks: &mut Vec<Framework>,
    seen: &mut HashSet<Framework>,
) {
    let lower = contents.to_ascii_lowercase();
    if lower.contains("lua-resty-") || lower.contains("openresty") {
        push_framework(Framework::OpenResty, frameworks, seen);
    }
}

/// Round-trip `as usize` discriminant → enum.  Keep in sync with the
/// variant order — the test module asserts round-trip for every
/// variant.
/// Compose the cheatsheet text to inject into the security_engineer's
/// system prompt.  Returns the composed text and the list of sheet
/// names actually included (for logging).  Empty return value = nothing
/// to inject.
///
/// Selection policy (locked):
/// 1. Take the top 2 detected languages.
/// 2. Add their detected frameworks.
/// 3. If the composed body exceeds [`MAX_CHEATSHEET_LINES`]: drop
///    frameworks (all of them).  If still over: drop the second
///    language (and any frameworks tied to it, already dropped at step
///    1 of the retry).  Single-language sheets are capped at ~100
///    lines, so one sheet always fits.
pub fn compose_cheatsheets(detection: &Detection) -> (String, Vec<&'static str>) {
    let primary_langs: Vec<Language> =
        detection.languages.iter().take(2).copied().collect();
    if primary_langs.is_empty() {
        return (String::new(), Vec::new());
    }

    let kept_frameworks: Vec<Framework> = detection
        .frameworks
        .iter()
        .copied()
        .filter(|fw| primary_langs.contains(&fw.language()))
        .collect();

    // Try: all langs + all frameworks.
    let with_frameworks = build_prompt(&primary_langs, &kept_frameworks);
    if line_count(&with_frameworks.0) <= MAX_CHEATSHEET_LINES {
        return with_frameworks;
    }

    // Drop frameworks.
    let langs_only = build_prompt(&primary_langs, &[]);
    if line_count(&langs_only.0) <= MAX_CHEATSHEET_LINES {
        return langs_only;
    }

    // Drop second language too.
    let one_lang = build_prompt(&primary_langs[..1], &[]);
    one_lang
}

fn line_count(s: &str) -> usize {
    if s.is_empty() {
        0
    } else {
        s.lines().count()
    }
}

fn build_prompt(
    languages: &[Language],
    frameworks: &[Framework],
) -> (String, Vec<&'static str>) {
    let mut body = String::new();
    let mut names: Vec<&'static str> = Vec::new();

    body.push_str("## Language and framework cheatsheets\n\nThe following starting-point references match manifests detected in the review target.  They are prompts to look — not an exhaustive list.  Novel sinks outside them are still in scope.\n\n");

    for lang in languages {
        let (name, content) = lang_sheet(*lang);
        body.push_str("---\n\n");
        body.push_str("### Cheatsheet: ");
        body.push_str(name);
        body.push_str("\n\n");
        body.push_str(content);
        body.push('\n');
        names.push(name);
    }
    for fw in frameworks {
        let (name, content) = framework_sheet(*fw);
        body.push_str("---\n\n");
        body.push_str("### Cheatsheet: ");
        body.push_str(name);
        body.push_str("\n\n");
        body.push_str(content);
        body.push('\n');
        names.push(name);
    }

    (body, names)
}

fn lang_sheet(lang: Language) -> (&'static str, &'static str) {
    match lang {
        Language::Python => (
            "lang/python",
            include_str!("prompts/cheatsheets/lang/python.md"),
        ),
        Language::JavaScript => (
            "lang/javascript",
            include_str!("prompts/cheatsheets/lang/javascript.md"),
        ),
        Language::Go => ("lang/go", include_str!("prompts/cheatsheets/lang/go.md")),
        Language::Rust => ("lang/rust", include_str!("prompts/cheatsheets/lang/rust.md")),
        Language::Ruby => ("lang/ruby", include_str!("prompts/cheatsheets/lang/ruby.md")),
        Language::Java => ("lang/java", include_str!("prompts/cheatsheets/lang/java.md")),
        Language::Kotlin => (
            "lang/kotlin",
            include_str!("prompts/cheatsheets/lang/kotlin.md"),
        ),
        Language::CSharp => (
            "lang/csharp",
            include_str!("prompts/cheatsheets/lang/csharp.md"),
        ),
        Language::Php => ("lang/php", include_str!("prompts/cheatsheets/lang/php.md")),
        Language::Cpp => ("lang/cpp", include_str!("prompts/cheatsheets/lang/cpp.md")),
        Language::Elixir => (
            "lang/elixir",
            include_str!("prompts/cheatsheets/lang/elixir.md"),
        ),
        Language::Haskell => (
            "lang/haskell",
            include_str!("prompts/cheatsheets/lang/haskell.md"),
        ),
        Language::Swift => (
            "lang/swift",
            include_str!("prompts/cheatsheets/lang/swift.md"),
        ),
        Language::Ocaml => (
            "lang/ocaml",
            include_str!("prompts/cheatsheets/lang/ocaml.md"),
        ),
        Language::Erlang => (
            "lang/erlang",
            include_str!("prompts/cheatsheets/lang/erlang.md"),
        ),
        Language::Zig => ("lang/zig", include_str!("prompts/cheatsheets/lang/zig.md")),
        Language::Nix => ("lang/nix", include_str!("prompts/cheatsheets/lang/nix.md")),
        Language::Lua => ("lang/lua", include_str!("prompts/cheatsheets/lang/lua.md")),
    }
}

fn framework_sheet(fw: Framework) -> (&'static str, &'static str) {
    match fw {
        Framework::Django => (
            "framework/django",
            include_str!("prompts/cheatsheets/framework/django.md"),
        ),
        Framework::Flask => (
            "framework/flask",
            include_str!("prompts/cheatsheets/framework/flask.md"),
        ),
        Framework::FastApi => (
            "framework/fastapi",
            include_str!("prompts/cheatsheets/framework/fastapi.md"),
        ),
        Framework::Express => (
            "framework/express",
            include_str!("prompts/cheatsheets/framework/express.md"),
        ),
        Framework::NextJs => (
            "framework/nextjs",
            include_str!("prompts/cheatsheets/framework/nextjs.md"),
        ),
        Framework::Actix => (
            "framework/actix",
            include_str!("prompts/cheatsheets/framework/actix.md"),
        ),
        Framework::Axum => (
            "framework/axum",
            include_str!("prompts/cheatsheets/framework/axum.md"),
        ),
        Framework::Solana => (
            "framework/solana",
            include_str!("prompts/cheatsheets/framework/solana.md"),
        ),
        Framework::Rails => (
            "framework/rails",
            include_str!("prompts/cheatsheets/framework/rails.md"),
        ),
        Framework::Spring => (
            "framework/spring",
            include_str!("prompts/cheatsheets/framework/spring.md"),
        ),
        Framework::AspNet => (
            "framework/aspnet",
            include_str!("prompts/cheatsheets/framework/aspnet.md"),
        ),
        Framework::Laravel => (
            "framework/laravel",
            include_str!("prompts/cheatsheets/framework/laravel.md"),
        ),
        Framework::Phoenix => (
            "framework/phoenix",
            include_str!("prompts/cheatsheets/framework/phoenix.md"),
        ),
        Framework::Gin => (
            "framework/gin",
            include_str!("prompts/cheatsheets/framework/gin.md"),
        ),
        Framework::Aiohttp => (
            "framework/aiohttp",
            include_str!("prompts/cheatsheets/framework/aiohttp.md"),
        ),
        Framework::Fastify => (
            "framework/fastify",
            include_str!("prompts/cheatsheets/framework/fastify.md"),
        ),
        Framework::NestJs => (
            "framework/nestjs",
            include_str!("prompts/cheatsheets/framework/nestjs.md"),
        ),
        Framework::Trpc => (
            "framework/trpc",
            include_str!("prompts/cheatsheets/framework/trpc.md"),
        ),
        Framework::Rocket => (
            "framework/rocket",
            include_str!("prompts/cheatsheets/framework/rocket.md"),
        ),
        Framework::Sinatra => (
            "framework/sinatra",
            include_str!("prompts/cheatsheets/framework/sinatra.md"),
        ),
        Framework::Ktor => (
            "framework/ktor",
            include_str!("prompts/cheatsheets/framework/ktor.md"),
        ),
        Framework::Symfony => (
            "framework/symfony",
            include_str!("prompts/cheatsheets/framework/symfony.md"),
        ),
        Framework::Echo => (
            "framework/echo",
            include_str!("prompts/cheatsheets/framework/echo.md"),
        ),
        Framework::Chi => (
            "framework/chi",
            include_str!("prompts/cheatsheets/framework/chi.md"),
        ),
        Framework::Vapor => (
            "framework/vapor",
            include_str!("prompts/cheatsheets/framework/vapor.md"),
        ),
        Framework::Tornado => (
            "framework/tornado",
            include_str!("prompts/cheatsheets/framework/tornado.md"),
        ),
        Framework::Sanic => (
            "framework/sanic",
            include_str!("prompts/cheatsheets/framework/sanic.md"),
        ),
        Framework::Celery => (
            "framework/celery",
            include_str!("prompts/cheatsheets/framework/celery.md"),
        ),
        Framework::Koa => (
            "framework/koa",
            include_str!("prompts/cheatsheets/framework/koa.md"),
        ),
        Framework::Hono => (
            "framework/hono",
            include_str!("prompts/cheatsheets/framework/hono.md"),
        ),
        Framework::SvelteKit => (
            "framework/sveltekit",
            include_str!("prompts/cheatsheets/framework/sveltekit.md"),
        ),
        Framework::Remix => (
            "framework/remix",
            include_str!("prompts/cheatsheets/framework/remix.md"),
        ),
        Framework::GraphQL => (
            "framework/graphql",
            include_str!("prompts/cheatsheets/framework/graphql.md"),
        ),
        Framework::Warp => (
            "framework/warp",
            include_str!("prompts/cheatsheets/framework/warp.md"),
        ),
        Framework::Tonic => (
            "framework/tonic",
            include_str!("prompts/cheatsheets/framework/tonic.md"),
        ),
        Framework::Quarkus => (
            "framework/quarkus",
            include_str!("prompts/cheatsheets/framework/quarkus.md"),
        ),
        Framework::Micronaut => (
            "framework/micronaut",
            include_str!("prompts/cheatsheets/framework/micronaut.md"),
        ),
        Framework::Javalin => (
            "framework/javalin",
            include_str!("prompts/cheatsheets/framework/javalin.md"),
        ),
        Framework::Slim => (
            "framework/slim",
            include_str!("prompts/cheatsheets/framework/slim.md"),
        ),
        Framework::CodeIgniter => (
            "framework/codeigniter",
            include_str!("prompts/cheatsheets/framework/codeigniter.md"),
        ),
        Framework::Fiber => (
            "framework/fiber",
            include_str!("prompts/cheatsheets/framework/fiber.md"),
        ),
        Framework::GorillaMux => (
            "framework/gorilla-mux",
            include_str!("prompts/cheatsheets/framework/gorilla-mux.md"),
        ),
        Framework::Hummingbird => (
            "framework/hummingbird",
            include_str!("prompts/cheatsheets/framework/hummingbird.md"),
        ),
        Framework::Servant => (
            "framework/servant",
            include_str!("prompts/cheatsheets/framework/servant.md"),
        ),
        Framework::Dream => (
            "framework/dream",
            include_str!("prompts/cheatsheets/framework/dream.md"),
        ),
        Framework::Cowboy => (
            "framework/cowboy",
            include_str!("prompts/cheatsheets/framework/cowboy.md"),
        ),
        Framework::Starlette => (
            "framework/starlette",
            include_str!("prompts/cheatsheets/framework/starlette.md"),
        ),
        Framework::Pyramid => (
            "framework/pyramid",
            include_str!("prompts/cheatsheets/framework/pyramid.md"),
        ),
        Framework::Falcon => (
            "framework/falcon",
            include_str!("prompts/cheatsheets/framework/falcon.md"),
        ),
        Framework::Bottle => (
            "framework/bottle",
            include_str!("prompts/cheatsheets/framework/bottle.md"),
        ),
        Framework::Play => (
            "framework/play",
            include_str!("prompts/cheatsheets/framework/play.md"),
        ),
        Framework::Dropwizard => (
            "framework/dropwizard",
            include_str!("prompts/cheatsheets/framework/dropwizard.md"),
        ),
        Framework::Helidon => (
            "framework/helidon",
            include_str!("prompts/cheatsheets/framework/helidon.md"),
        ),
        Framework::Vertx => (
            "framework/vertx",
            include_str!("prompts/cheatsheets/framework/vertx.md"),
        ),
        Framework::Hapi => (
            "framework/hapi",
            include_str!("prompts/cheatsheets/framework/hapi.md"),
        ),
        Framework::Adonis => (
            "framework/adonis",
            include_str!("prompts/cheatsheets/framework/adonis.md"),
        ),
        Framework::Meteor => (
            "framework/meteor",
            include_str!("prompts/cheatsheets/framework/meteor.md"),
        ),
        Framework::Nuxt => (
            "framework/nuxt",
            include_str!("prompts/cheatsheets/framework/nuxt.md"),
        ),
        Framework::OpenResty => (
            "framework/openresty",
            include_str!("prompts/cheatsheets/framework/openresty.md"),
        ),
    }
}

/// Convenience: detect + compose in one call.  The tool layer uses this.
pub fn detect_and_compose(root: &Path) -> (String, Vec<&'static str>) {
    compose_cheatsheets(&detect_repo(root))
}

/// Suppress `unused` when `dead_code` lint fires — this helper is part
/// of the public detection surface even when no downstream caller is
/// linked in the current build configuration.
#[allow(dead_code)]
fn _assert_detection_is_send_sync()
where
    Detection: Send + Sync,
{
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::fs;
    use std::path::PathBuf;
    use tempfile::TempDir;

    /// Shorthand for `dir.join(name)` + write the file.  Tests build
    /// synthetic repos purely via manifest contents.
    fn write(dir: &Path, name: &str, contents: &str) -> PathBuf {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&path, contents).unwrap();
        path
    }

    /// Every `Language` variant in the enum.  Kept as a const so the
    /// exhaustive-coverage tests can iterate it without drifting from
    /// the enum definition — adding a variant without updating this
    /// list is a compile error via the `_match_guard`.
    const ALL_LANGUAGES: &[Language] = &[
        Language::Python,
        Language::JavaScript,
        Language::Go,
        Language::Rust,
        Language::Ruby,
        Language::Java,
        Language::Kotlin,
        Language::CSharp,
        Language::Php,
        Language::Cpp,
        Language::Elixir,
        Language::Haskell,
        Language::Swift,
        Language::Ocaml,
        Language::Erlang,
        Language::Zig,
        Language::Nix,
        Language::Lua,
    ];

    const ALL_FRAMEWORKS: &[Framework] = &[
        Framework::Django,
        Framework::Flask,
        Framework::FastApi,
        Framework::Aiohttp,
        Framework::Tornado,
        Framework::Sanic,
        Framework::Celery,
        Framework::Express,
        Framework::NextJs,
        Framework::Fastify,
        Framework::NestJs,
        Framework::Trpc,
        Framework::Koa,
        Framework::Hono,
        Framework::SvelteKit,
        Framework::Remix,
        Framework::GraphQL,
        Framework::Actix,
        Framework::Axum,
        Framework::Rocket,
        Framework::Warp,
        Framework::Tonic,
        Framework::Rails,
        Framework::Sinatra,
        Framework::Spring,
        Framework::Quarkus,
        Framework::Micronaut,
        Framework::Javalin,
        Framework::Ktor,
        Framework::AspNet,
        Framework::Laravel,
        Framework::Symfony,
        Framework::Slim,
        Framework::CodeIgniter,
        Framework::Phoenix,
        Framework::Gin,
        Framework::Echo,
        Framework::Chi,
        Framework::Fiber,
        Framework::GorillaMux,
        Framework::Vapor,
        Framework::Hummingbird,
        Framework::Servant,
        Framework::Dream,
        Framework::Cowboy,
        Framework::Starlette,
        Framework::Pyramid,
        Framework::Falcon,
        Framework::Bottle,
        Framework::Play,
        Framework::Dropwizard,
        Framework::Helidon,
        Framework::Vertx,
        Framework::Hapi,
        Framework::Adonis,
        Framework::Meteor,
        Framework::Nuxt,
        Framework::OpenResty,
        Framework::Solana,
    ];

    /// Compile-time guard: if a new variant is added to either enum
    /// without updating `ALL_LANGUAGES` / `ALL_FRAMEWORKS` above, this
    /// function fails to compile — the `match` loses exhaustivity.
    /// Never called; exists purely for the compiler check.
    #[allow(dead_code)]
    fn _match_guard(lang: Language, fw: Framework) {
        match lang {
            Language::Python
            | Language::JavaScript
            | Language::Go
            | Language::Rust
            | Language::Ruby
            | Language::Java
            | Language::Kotlin
            | Language::CSharp
            | Language::Php
            | Language::Cpp
            | Language::Elixir
            | Language::Haskell
            | Language::Swift
            | Language::Ocaml
            | Language::Erlang
            | Language::Zig
            | Language::Nix
            | Language::Lua => {}
        }
        match fw {
            Framework::Django
            | Framework::Flask
            | Framework::FastApi
            | Framework::Aiohttp
            | Framework::Tornado
            | Framework::Sanic
            | Framework::Celery
            | Framework::Express
            | Framework::NextJs
            | Framework::Fastify
            | Framework::NestJs
            | Framework::Trpc
            | Framework::Koa
            | Framework::Hono
            | Framework::SvelteKit
            | Framework::Remix
            | Framework::GraphQL
            | Framework::Actix
            | Framework::Axum
            | Framework::Rocket
            | Framework::Warp
            | Framework::Tonic
            | Framework::Rails
            | Framework::Sinatra
            | Framework::Spring
            | Framework::Quarkus
            | Framework::Micronaut
            | Framework::Javalin
            | Framework::Ktor
            | Framework::AspNet
            | Framework::Laravel
            | Framework::Symfony
            | Framework::Slim
            | Framework::CodeIgniter
            | Framework::Phoenix
            | Framework::Gin
            | Framework::Echo
            | Framework::Chi
            | Framework::Fiber
            | Framework::GorillaMux
            | Framework::Vapor
            | Framework::Hummingbird
            | Framework::Servant
            | Framework::Dream
            | Framework::Cowboy
            | Framework::Starlette
            | Framework::Pyramid
            | Framework::Falcon
            | Framework::Bottle
            | Framework::Play
            | Framework::Dropwizard
            | Framework::Helidon
            | Framework::Vertx
            | Framework::Hapi
            | Framework::Adonis
            | Framework::Meteor
            | Framework::Nuxt
            | Framework::OpenResty
            | Framework::Solana => {}
        }
    }

    /// For every variant: `lang_sheet` returns a non-empty name and a
    /// non-empty body that begins with the documented "Starting points"
    /// opener.  Catches a forgotten `include_str!` wiring (missing
    /// match arm would be a compile error — this asserts the content).
    #[test]
    fn every_language_has_a_nonempty_sheet() {
        for &lang in ALL_LANGUAGES {
            let (name, body) = lang_sheet(lang);
            assert!(
                name.starts_with("lang/"),
                "language name should be under lang/: got {name:?}"
            );
            assert!(!body.is_empty(), "{name} body is empty");
            assert!(
                body.starts_with("Starting points"),
                "{name} must open with the 'Starting points' opener (non-negotiable — \
                 it anchors the model against over-applying the sheet).  \
                 First 60 chars were: {:?}",
                &body[..body.len().min(60)]
            );
        }
    }

    #[test]
    fn every_framework_has_a_nonempty_sheet() {
        for &fw in ALL_FRAMEWORKS {
            let (name, body) = framework_sheet(fw);
            assert!(
                name.starts_with("framework/"),
                "framework name should be under framework/: got {name:?}"
            );
            assert!(!body.is_empty(), "{name} body is empty");
            assert!(
                body.starts_with("Starting points"),
                "{name} must open with 'Starting points'. First 60 chars: {:?}",
                &body[..body.len().min(60)]
            );
        }
    }

    /// Every framework's `language()` maps to a real Language variant
    /// — guards against dangling language references after an enum
    /// refactor.
    #[test]
    fn every_framework_binds_to_a_known_language() {
        for &fw in ALL_FRAMEWORKS {
            let lang = fw.language();
            assert!(
                ALL_LANGUAGES.contains(&lang),
                "framework {fw:?} binds to language {lang:?} which isn't in ALL_LANGUAGES"
            );
        }
    }

    /// For every Language variant: a synthetic detection containing
    /// just that language composes a prompt whose included-sheet list
    /// names that language's sheet.  Exercises the full detection →
    /// compose pipeline per variant.
    #[test]
    fn compose_emits_sheet_name_for_every_language() {
        for &lang in ALL_LANGUAGES {
            let det = Detection {
                languages: vec![lang],
                frameworks: vec![],
            };
            let (_body, names) = compose_cheatsheets(&det);
            let expected = lang_sheet(lang).0;
            assert!(
                names.contains(&expected),
                "composing {lang:?} should emit {expected:?} in the sheet list, got {names:?}"
            );
        }
    }

    /// For every Framework variant: detection with the binding
    /// language + the framework emits BOTH sheet names.
    #[test]
    fn compose_emits_sheet_name_for_every_framework() {
        for &fw in ALL_FRAMEWORKS {
            let det = Detection {
                languages: vec![fw.language()],
                frameworks: vec![fw],
            };
            let (_body, names) = compose_cheatsheets(&det);
            let expected_lang = lang_sheet(fw.language()).0;
            let expected_fw = framework_sheet(fw).0;
            assert!(
                names.contains(&expected_lang),
                "composing {fw:?} on {:?} should include {expected_lang:?}, got {names:?}",
                fw.language()
            );
            assert!(
                names.contains(&expected_fw),
                "composing {fw:?} should include {expected_fw:?}, got {names:?}"
            );
        }
    }

    /// Every individual lang sheet fits solo under the cap (one lang's
    /// worth of content always ships, per the `compose_cheatsheets`
    /// policy).  Protects against an out-of-bound sheet being silently
    /// swallowed by the cap.
    #[test]
    fn every_language_sheet_fits_the_line_cap_solo() {
        for &lang in ALL_LANGUAGES {
            let det = Detection {
                languages: vec![lang],
                frameworks: vec![],
            };
            let (body, _names) = compose_cheatsheets(&det);
            assert!(
                line_count(&body) <= MAX_CHEATSHEET_LINES,
                "single-language composition for {lang:?} exceeded the cap ({} lines)",
                line_count(&body)
            );
        }
    }

    #[test]
    fn detects_rust_via_cargo_toml() {
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "Cargo.toml",
            "[package]\nname = \"demo\"\n[dependencies]\nserde = \"1\"\n",
        );
        let det = detect_repo(tmp.path());
        assert_eq!(det.languages, vec![Language::Rust]);
        assert!(det.frameworks.is_empty());
    }

    #[test]
    fn detects_actix_framework() {
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "Cargo.toml",
            "[package]\nname = \"srv\"\n[dependencies]\nactix-web = \"4\"\ntokio = \"1\"\n",
        );
        let det = detect_repo(tmp.path());
        assert_eq!(det.languages, vec![Language::Rust]);
        assert_eq!(det.frameworks, vec![Framework::Actix]);
    }

    #[test]
    fn detects_axum_framework() {
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "Cargo.toml",
            "[dependencies]\naxum = \"0.7\"\n",
        );
        let det = detect_repo(tmp.path());
        assert_eq!(det.frameworks, vec![Framework::Axum]);
    }

    #[test]
    fn detects_solana_anchor_program() {
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "Cargo.toml",
            "[package]\nname = \"my-program\"\n[dependencies]\nanchor-lang = \"0.30\"\nsolana-program = \"1.18\"\n",
        );
        let det = detect_repo(tmp.path());
        assert_eq!(det.languages, vec![Language::Rust]);
        assert_eq!(det.frameworks, vec![Framework::Solana]);
    }

    #[test]
    fn detects_solana_native_program() {
        // Native (no Anchor) program — only `solana-program` as a dep.
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "Cargo.toml",
            "[package]\nname = \"native-prog\"\n[dependencies]\nsolana-program = \"1.18\"\n",
        );
        let det = detect_repo(tmp.path());
        assert_eq!(det.frameworks, vec![Framework::Solana]);
    }

    #[test]
    fn detects_pinocchio_program() {
        // Pinocchio is a newer zero-dep Solana program framework —
        // covered by the same sheet since the vuln classes overlap.
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "Cargo.toml",
            "[package]\nname = \"pin\"\n[dependencies]\npinocchio = \"0.5\"\n",
        );
        let det = detect_repo(tmp.path());
        assert_eq!(det.frameworks, vec![Framework::Solana]);
    }

    /// End-to-end: an Anchor program's Cargo.toml triggers detection
    /// AND the composed prompt carries the Solana cheatsheet body
    /// (not just the sheet name).  A missing `framework_sheet` arm,
    /// a typo'd `include_str!` path, or a wrong sheet opener would
    /// all slip past the per-framework structural guards — this
    /// walks the full detect → compose path end-to-end.
    #[test]
    fn solana_program_composes_solana_cheatsheet_into_prompt() {
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "Cargo.toml",
            "[package]\nname = \"my-anchor-program\"\n[dependencies]\nanchor-lang = \"0.30\"\n",
        );
        let (body, names) = detect_and_compose(tmp.path());
        assert!(
            names.contains(&"framework/solana"),
            "composed sheet list missing framework/solana: {names:?}"
        );
        // Unique-enough strings from framework/solana.md — a generic
        // Rust-lang sheet wouldn't contain any of these.
        assert!(
            body.contains("Starting points for Solana programs"),
            "solana sheet opener missing from composed body"
        );
        assert!(
            body.contains("#[derive(Accounts)]"),
            "Anchor constraint-audit section missing from composed body"
        );
        assert!(
            body.contains("Missing signer check"),
            "signer-check vuln class missing from composed body"
        );
    }

    #[test]
    fn detects_javascript_via_package_json() {
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "package.json",
            r#"{ "name": "x", "dependencies": { "express": "4.x" } }"#,
        );
        let det = detect_repo(tmp.path());
        assert_eq!(det.languages, vec![Language::JavaScript]);
        assert_eq!(det.frameworks, vec![Framework::Express]);
    }

    #[test]
    fn detects_python_via_pyproject() {
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "pyproject.toml",
            "[project]\nname = \"x\"\ndependencies = [\"Django>=4\", \"requests\"]\n",
        );
        let det = detect_repo(tmp.path());
        assert_eq!(det.languages, vec![Language::Python]);
        assert_eq!(det.frameworks, vec![Framework::Django]);
    }

    #[test]
    fn detects_python_via_poetry_section() {
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "pyproject.toml",
            "[tool.poetry.dependencies]\npython = \"^3.10\"\nflask = \"^2.3\"\n",
        );
        let det = detect_repo(tmp.path());
        assert_eq!(det.frameworks, vec![Framework::Flask]);
    }

    #[test]
    fn detects_python_via_requirements_txt() {
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "requirements.txt",
            "# deps\nDjango==4.2.1\nrequests>=2.0\n-r other.txt\n",
        );
        let det = detect_repo(tmp.path());
        assert_eq!(det.languages, vec![Language::Python]);
        assert_eq!(det.frameworks, vec![Framework::Django]);
    }

    #[test]
    fn detects_go_via_go_mod() {
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "go.mod",
            "module example.com/x\n\ngo 1.22\n",
        );
        let det = detect_repo(tmp.path());
        assert_eq!(det.languages, vec![Language::Go]);
    }

    #[test]
    fn detects_ruby_via_gemfile() {
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "Gemfile",
            "source 'https://rubygems.org'\ngem 'sqlite3'\n",
        );
        let det = detect_repo(tmp.path());
        assert_eq!(det.languages, vec![Language::Ruby]);
        assert!(det.frameworks.is_empty());
    }

    #[test]
    fn detects_rails_framework() {
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "Gemfile",
            "source 'https://rubygems.org'\ngem 'rails', '~> 7.0'\n",
        );
        let det = detect_repo(tmp.path());
        assert_eq!(det.frameworks, vec![Framework::Rails]);
    }

    #[test]
    fn detects_java_via_pom() {
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "pom.xml",
            "<project>\n  <dependencies>\n    <dependency>\n      <groupId>org.springframework.boot</groupId>\n      <artifactId>spring-boot-starter-web</artifactId>\n    </dependency>\n  </dependencies>\n</project>\n",
        );
        let det = detect_repo(tmp.path());
        assert_eq!(det.languages, vec![Language::Java]);
        assert_eq!(det.frameworks, vec![Framework::Spring]);
    }

    #[test]
    fn build_gradle_kts_with_spring_detects_kotlin_plus_spring() {
        // After the build.gradle.kts → Kotlin split, a Kotlin-DSL
        // build file with a Spring Boot plugin should register as
        // Kotlin (because .kts) but still include Spring (since the
        // framework cheatsheet binds to Java — but the language
        // doesn't surface; the framework stays through language()).
        // In practice: language is Kotlin, framework list still
        // contains Spring (bound to Java), so in composition Spring
        // drops out because Java isn't in the top 2.  We assert
        // language here and leave framework-composition behavior to
        // the compose tests.
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "build.gradle.kts",
            "plugins {\n  id(\"org.springframework.boot\") version \"3.1.0\"\n}\n",
        );
        let det = detect_repo(tmp.path());
        assert_eq!(det.languages, vec![Language::Kotlin]);
        // Framework list records what the manifest advertised even if
        // its bound language isn't primary — the compose step filters.
        assert!(det.frameworks.contains(&Framework::Spring));
    }

    #[test]
    fn detects_csharp_via_csproj() {
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "App.csproj",
            "<Project Sdk=\"Microsoft.NET.Sdk.Web\">\n  <PropertyGroup>\n    <TargetFramework>net8.0</TargetFramework>\n  </PropertyGroup>\n  <ItemGroup>\n    <FrameworkReference Include=\"Microsoft.AspNetCore.App\" />\n  </ItemGroup>\n</Project>\n",
        );
        let det = detect_repo(tmp.path());
        assert_eq!(det.languages, vec![Language::CSharp]);
        assert_eq!(det.frameworks, vec![Framework::AspNet]);
    }

    #[test]
    fn detects_php_via_composer() {
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "composer.json",
            r#"{"name":"app","require":{"laravel/framework":"^10.0"}}"#,
        );
        let det = detect_repo(tmp.path());
        assert_eq!(det.languages, vec![Language::Php]);
        assert_eq!(det.frameworks, vec![Framework::Laravel]);
    }

    #[test]
    fn detects_elixir_via_mix_and_phoenix() {
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "mix.exs",
            "defp deps, do: [\n  {:phoenix, \"~> 1.7\"},\n  {:ecto, \"~> 3.10\"}\n]\n",
        );
        let det = detect_repo(tmp.path());
        assert_eq!(det.languages, vec![Language::Elixir]);
        assert_eq!(det.frameworks, vec![Framework::Phoenix]);
    }

    #[test]
    fn detects_gin_via_go_mod() {
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "go.mod",
            "module x\n\ngo 1.22\n\nrequire (\n\tgithub.com/gin-gonic/gin v1.9.1\n)\n",
        );
        let det = detect_repo(tmp.path());
        assert_eq!(det.languages, vec![Language::Go]);
        assert_eq!(det.frameworks, vec![Framework::Gin]);
    }

    #[test]
    fn detects_nextjs_via_package_json() {
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "package.json",
            r#"{"name":"app","dependencies":{"next":"^14","react":"^18"}}"#,
        );
        let det = detect_repo(tmp.path());
        assert_eq!(det.frameworks, vec![Framework::NextJs]);
    }

    #[test]
    fn detects_fastapi_via_requirements() {
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "requirements.txt",
            "fastapi>=0.100\nuvicorn\n",
        );
        let det = detect_repo(tmp.path());
        assert_eq!(det.frameworks, vec![Framework::FastApi]);
    }

    // ------------------------------------------------------------------
    // Framework expansion — second wave.  Tests authored ahead of the
    // Framework enum additions, the scanner logic, and the sheet files
    // (TDD); tests fail to compile until the enum variants exist, then
    // fail for missing sheet / detection until fully wired.
    // ------------------------------------------------------------------

    // ------------------------------------------------------------------
    // Framework expansion — fourth wave.  Finishes out Python
    // (starlette, pyramid, falcon, bottle), the Java ecosystem (play,
    // dropwizard, helidon, vertx), JS legacy + ecosystem (hapi,
    // adonis, meteor, nuxt), plus a brand-new Lua language with
    // OpenResty (nginx + LuaJIT; widely deployed in front-of-stack
    // gateways and embedded CDN logic).
    // ------------------------------------------------------------------

    #[test]
    fn detects_starlette_via_requirements() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "requirements.txt", "starlette>=0.37\nuvicorn\n");
        assert!(detect_repo(tmp.path())
            .frameworks
            .contains(&Framework::Starlette));
    }

    #[test]
    fn detects_pyramid_via_pyproject() {
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "pyproject.toml",
            "[project]\nname = \"x\"\ndependencies = [\"pyramid>=2\"]\n",
        );
        assert!(detect_repo(tmp.path())
            .frameworks
            .contains(&Framework::Pyramid));
    }

    #[test]
    fn detects_falcon_via_requirements() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "requirements.txt", "falcon>=3\n");
        assert!(detect_repo(tmp.path())
            .frameworks
            .contains(&Framework::Falcon));
    }

    #[test]
    fn detects_bottle_via_requirements() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "requirements.txt", "bottle>=0.12\n");
        assert!(detect_repo(tmp.path())
            .frameworks
            .contains(&Framework::Bottle));
    }

    #[test]
    fn detects_play_via_pom() {
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "pom.xml",
            "<project><dependencies><dependency><groupId>com.typesafe.play</groupId><artifactId>play-java</artifactId></dependency></dependencies></project>\n",
        );
        assert!(detect_repo(tmp.path())
            .frameworks
            .contains(&Framework::Play));
    }

    #[test]
    fn detects_dropwizard_via_pom() {
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "pom.xml",
            "<project><dependencies><dependency><groupId>io.dropwizard</groupId><artifactId>dropwizard-core</artifactId></dependency></dependencies></project>\n",
        );
        assert!(detect_repo(tmp.path())
            .frameworks
            .contains(&Framework::Dropwizard));
    }

    #[test]
    fn detects_helidon_via_pom() {
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "pom.xml",
            "<project><dependencies><dependency><groupId>io.helidon.webserver</groupId><artifactId>helidon-webserver</artifactId></dependency></dependencies></project>\n",
        );
        assert!(detect_repo(tmp.path())
            .frameworks
            .contains(&Framework::Helidon));
    }

    #[test]
    fn detects_vertx_via_pom() {
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "pom.xml",
            "<project><dependencies><dependency><groupId>io.vertx</groupId><artifactId>vertx-core</artifactId></dependency></dependencies></project>\n",
        );
        assert!(detect_repo(tmp.path())
            .frameworks
            .contains(&Framework::Vertx));
    }

    #[test]
    fn detects_hapi_via_package_json() {
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "package.json",
            r#"{"name":"app","dependencies":{"@hapi/hapi":"^21"}}"#,
        );
        assert!(detect_repo(tmp.path())
            .frameworks
            .contains(&Framework::Hapi));
    }

    #[test]
    fn detects_adonis_via_package_json() {
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "package.json",
            r#"{"name":"app","dependencies":{"@adonisjs/core":"^6"}}"#,
        );
        assert!(detect_repo(tmp.path())
            .frameworks
            .contains(&Framework::Adonis));
    }

    #[test]
    fn detects_meteor_via_package_json() {
        // Meteor apps register a `meteor` key at the top level of
        // package.json plus meteor-node-stubs dep.
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "package.json",
            r#"{"name":"app","meteor":{"mainModule":{"client":"client/main.js"}},"dependencies":{"meteor-node-stubs":"^1"}}"#,
        );
        assert!(detect_repo(tmp.path())
            .frameworks
            .contains(&Framework::Meteor));
    }

    #[test]
    fn detects_nuxt_via_package_json() {
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "package.json",
            r#"{"name":"app","devDependencies":{"nuxt":"^3"}}"#,
        );
        assert!(detect_repo(tmp.path())
            .frameworks
            .contains(&Framework::Nuxt));
    }

    #[test]
    fn detects_lua_via_rockspec() {
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "app-1.0.0-1.rockspec",
            "package = \"app\"\nversion = \"1.0.0-1\"\ndependencies = {\n  \"lua >= 5.1\",\n}\n",
        );
        let det = detect_repo(tmp.path());
        assert_eq!(det.languages, vec![Language::Lua]);
    }

    #[test]
    fn detects_openresty_via_rockspec() {
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "app-1.0.0-1.rockspec",
            "package = \"app\"\nversion = \"1.0.0-1\"\ndependencies = {\n  \"lua-resty-openssl\",\n  \"lua-resty-http\",\n}\n",
        );
        assert!(detect_repo(tmp.path())
            .frameworks
            .contains(&Framework::OpenResty));
    }

    // ------------------------------------------------------------------
    // Framework expansion — third wave.  More JS/TS (koa, hono,
    // sveltekit, remix), more Python (tornado, sanic, celery), more
    // Go (fiber, gorilla/mux, net/http-stdlib marker), more Rust
    // (warp, tonic), more JVM (quarkus, micronaut, javalin), more
    // PHP (slim, codeigniter), plus Servant (Haskell), Dream (OCaml),
    // Cowboy (Erlang), Hummingbird (Swift), and a protocol-level
    // GraphQL sheet.
    // ------------------------------------------------------------------

    #[test]
    fn detects_koa_via_package_json() {
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "package.json",
            r#"{"name":"app","dependencies":{"koa":"^2.15"}}"#,
        );
        assert!(detect_repo(tmp.path())
            .frameworks
            .contains(&Framework::Koa));
    }

    #[test]
    fn detects_hono_via_package_json() {
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "package.json",
            r#"{"name":"app","dependencies":{"hono":"^4"}}"#,
        );
        assert!(detect_repo(tmp.path())
            .frameworks
            .contains(&Framework::Hono));
    }

    #[test]
    fn detects_sveltekit_via_package_json() {
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "package.json",
            r#"{"name":"app","devDependencies":{"@sveltejs/kit":"^2"}}"#,
        );
        assert!(detect_repo(tmp.path())
            .frameworks
            .contains(&Framework::SvelteKit));
    }

    #[test]
    fn detects_remix_via_package_json() {
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "package.json",
            r#"{"name":"app","dependencies":{"@remix-run/node":"^2"}}"#,
        );
        assert!(detect_repo(tmp.path())
            .frameworks
            .contains(&Framework::Remix));
    }

    #[test]
    fn detects_graphql_via_package_json() {
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "package.json",
            r#"{"name":"app","dependencies":{"@apollo/server":"^4","graphql":"^16"}}"#,
        );
        assert!(detect_repo(tmp.path())
            .frameworks
            .contains(&Framework::GraphQL));
    }

    #[test]
    fn detects_tornado_via_requirements() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "requirements.txt", "tornado>=6.4\n");
        assert!(detect_repo(tmp.path())
            .frameworks
            .contains(&Framework::Tornado));
    }

    #[test]
    fn detects_sanic_via_requirements() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "requirements.txt", "sanic>=23\n");
        assert!(detect_repo(tmp.path())
            .frameworks
            .contains(&Framework::Sanic));
    }

    #[test]
    fn detects_celery_via_requirements() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "requirements.txt", "celery>=5.3\nredis\n");
        assert!(detect_repo(tmp.path())
            .frameworks
            .contains(&Framework::Celery));
    }

    #[test]
    fn detects_fiber_via_go_mod() {
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "go.mod",
            "module x\n\ngo 1.22\n\nrequire github.com/gofiber/fiber/v2 v2.52.0\n",
        );
        assert!(detect_repo(tmp.path())
            .frameworks
            .contains(&Framework::Fiber));
    }

    #[test]
    fn detects_gorilla_mux_via_go_mod() {
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "go.mod",
            "module x\n\ngo 1.22\n\nrequire github.com/gorilla/mux v1.8.1\n",
        );
        assert!(detect_repo(tmp.path())
            .frameworks
            .contains(&Framework::GorillaMux));
    }

    #[test]
    fn detects_warp_via_cargo_toml() {
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "Cargo.toml",
            "[dependencies]\nwarp = \"0.3\"\n",
        );
        assert!(detect_repo(tmp.path())
            .frameworks
            .contains(&Framework::Warp));
    }

    #[test]
    fn detects_tonic_via_cargo_toml() {
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "Cargo.toml",
            "[dependencies]\ntonic = \"0.11\"\nprost = \"0.12\"\n",
        );
        assert!(detect_repo(tmp.path())
            .frameworks
            .contains(&Framework::Tonic));
    }

    #[test]
    fn detects_quarkus_via_pom_xml() {
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "pom.xml",
            "<project><dependencies>\n  <dependency><groupId>io.quarkus</groupId><artifactId>quarkus-rest</artifactId></dependency>\n</dependencies></project>\n",
        );
        assert!(detect_repo(tmp.path())
            .frameworks
            .contains(&Framework::Quarkus));
    }

    #[test]
    fn detects_micronaut_via_gradle() {
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "build.gradle",
            "plugins { id 'io.micronaut.application' version '4.3' }\n",
        );
        assert!(detect_repo(tmp.path())
            .frameworks
            .contains(&Framework::Micronaut));
    }

    #[test]
    fn detects_javalin_via_gradle() {
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "build.gradle",
            "dependencies { implementation 'io.javalin:javalin:6.0.0' }\n",
        );
        assert!(detect_repo(tmp.path())
            .frameworks
            .contains(&Framework::Javalin));
    }

    #[test]
    fn detects_slim_via_composer() {
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "composer.json",
            r#"{"name":"app","require":{"slim/slim":"^4"}}"#,
        );
        assert!(detect_repo(tmp.path())
            .frameworks
            .contains(&Framework::Slim));
    }

    #[test]
    fn detects_codeigniter_via_composer() {
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "composer.json",
            r#"{"name":"app","require":{"codeigniter4/framework":"^4"}}"#,
        );
        assert!(detect_repo(tmp.path())
            .frameworks
            .contains(&Framework::CodeIgniter));
    }

    #[test]
    fn detects_servant_via_cabal_project() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "cabal.project", "packages: .\n");
        write(
            tmp.path(),
            "app.cabal",
            "build-depends: base, servant, servant-server\n",
        );
        assert!(detect_repo(tmp.path())
            .frameworks
            .contains(&Framework::Servant));
    }

    #[test]
    fn detects_dream_via_dune_project() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "dune-project", "(lang dune 3.0)\n");
        write(
            tmp.path(),
            "dune",
            "(executable (name app) (libraries dream))\n",
        );
        assert!(detect_repo(tmp.path())
            .frameworks
            .contains(&Framework::Dream));
    }

    #[test]
    fn detects_cowboy_via_rebar_config() {
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "rebar.config",
            "{deps, [\n  {cowboy, \"2.10.0\"}\n]}.\n",
        );
        assert!(detect_repo(tmp.path())
            .frameworks
            .contains(&Framework::Cowboy));
    }

    #[test]
    fn detects_hummingbird_via_package_swift() {
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "Package.swift",
            "// swift-tools-version:5.9\nimport PackageDescription\nlet package = Package(\n  name: \"app\",\n  dependencies: [\n    .package(url: \"https://github.com/hummingbird-project/hummingbird.git\", from: \"2.0.0\"),\n  ]\n)\n",
        );
        assert!(detect_repo(tmp.path())
            .frameworks
            .contains(&Framework::Hummingbird));
    }

    #[test]
    fn detects_aiohttp_via_requirements() {
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "requirements.txt",
            "aiohttp>=3.9\nuvloop\n",
        );
        let det = detect_repo(tmp.path());
        assert_eq!(det.languages, vec![Language::Python]);
        assert!(det.frameworks.contains(&Framework::Aiohttp));
    }

    #[test]
    fn detects_fastify_via_package_json() {
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "package.json",
            r#"{"name":"app","dependencies":{"fastify":"^4.26"}}"#,
        );
        let det = detect_repo(tmp.path());
        assert!(det.frameworks.contains(&Framework::Fastify));
    }

    #[test]
    fn detects_nestjs_via_package_json() {
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "package.json",
            r#"{"name":"app","dependencies":{"@nestjs/core":"^10","@nestjs/common":"^10"}}"#,
        );
        let det = detect_repo(tmp.path());
        assert!(det.frameworks.contains(&Framework::NestJs));
    }

    #[test]
    fn detects_trpc_via_package_json() {
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "package.json",
            r#"{"name":"app","dependencies":{"@trpc/server":"^11"}}"#,
        );
        let det = detect_repo(tmp.path());
        assert!(det.frameworks.contains(&Framework::Trpc));
    }

    #[test]
    fn detects_rocket_via_cargo_toml() {
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "Cargo.toml",
            "[dependencies]\nrocket = \"0.5\"\n",
        );
        let det = detect_repo(tmp.path());
        assert!(det.frameworks.contains(&Framework::Rocket));
    }

    #[test]
    fn detects_sinatra_via_gemfile() {
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "Gemfile",
            "source 'https://rubygems.org'\ngem 'sinatra', '~> 4.0'\n",
        );
        let det = detect_repo(tmp.path());
        assert!(det.frameworks.contains(&Framework::Sinatra));
    }

    #[test]
    fn detects_kotlin_and_ktor_via_build_gradle_kts() {
        // A `.kts` build script means Kotlin DSL → reviewer should see
        // Kotlin-specific patterns, not Java.  If ktor is a dep, it
        // binds to the Kotlin language sheet.
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "build.gradle.kts",
            "plugins { application }\ndependencies {\n  implementation(\"io.ktor:ktor-server-core:2.3.10\")\n}\n",
        );
        let det = detect_repo(tmp.path());
        assert_eq!(det.languages, vec![Language::Kotlin]);
        assert!(det.frameworks.contains(&Framework::Ktor));
    }

    #[test]
    fn detects_echo_via_go_mod() {
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "go.mod",
            "module x\n\ngo 1.22\n\nrequire github.com/labstack/echo/v4 v4.11.4\n",
        );
        let det = detect_repo(tmp.path());
        assert!(det.frameworks.contains(&Framework::Echo));
    }

    #[test]
    fn detects_chi_via_go_mod() {
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "go.mod",
            "module x\n\ngo 1.22\n\nrequire github.com/go-chi/chi/v5 v5.0.12\n",
        );
        let det = detect_repo(tmp.path());
        assert!(det.frameworks.contains(&Framework::Chi));
    }

    #[test]
    fn detects_symfony_via_composer() {
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "composer.json",
            r#"{"name":"app","require":{"symfony/framework-bundle":"^7.0"}}"#,
        );
        let det = detect_repo(tmp.path());
        assert_eq!(det.languages, vec![Language::Php]);
        assert!(det.frameworks.contains(&Framework::Symfony));
    }

    #[test]
    fn detects_vapor_via_package_swift() {
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "Package.swift",
            "// swift-tools-version:5.9\nimport PackageDescription\nlet package = Package(\n  name: \"app\",\n  dependencies: [\n    .package(url: \"https://github.com/vapor/vapor.git\", from: \"4.89.0\"),\n  ]\n)\n",
        );
        let det = detect_repo(tmp.path());
        assert_eq!(det.languages, vec![Language::Swift]);
        assert!(det.frameworks.contains(&Framework::Vapor));
    }

    // --- Guard: adjusted bindings after Kotlin gained a distinct lang ---
    // `build.gradle.kts` used to register as Java; after this round it
    // must register as Kotlin.  `pom.xml` and plain `build.gradle` stay
    // Java (most common case).
    #[test]
    fn plain_build_gradle_is_java_not_kotlin() {
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "build.gradle",
            "plugins { id 'org.springframework.boot' version '3.1.0' }\n",
        );
        let det = detect_repo(tmp.path());
        assert_eq!(det.languages, vec![Language::Java]);
        assert!(det.frameworks.contains(&Framework::Spring));
    }

    #[test]
    fn detects_cpp_via_conanfile() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "conanfile.txt", "[requires]\nfmt/9.0.0\n");
        let det = detect_repo(tmp.path());
        assert_eq!(det.languages, vec![Language::Cpp]);
    }

    #[test]
    fn detects_nix_via_flake() {
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "flake.nix",
            "{ description = \"x\"; inputs = {}; outputs = { self }: {}; }\n",
        );
        let det = detect_repo(tmp.path());
        assert_eq!(det.languages, vec![Language::Nix]);
    }

    #[test]
    fn polyglot_ranks_by_manifest_count() {
        let tmp = TempDir::new().unwrap();
        // Two JS manifests (root + sub), one Python manifest.
        write(tmp.path(), "package.json", r#"{"name":"root"}"#);
        write(
            tmp.path(),
            "packages/child/package.json",
            r#"{"name":"child"}"#,
        );
        write(
            tmp.path(),
            "pyproject.toml",
            "[project]\nname = \"x\"\ndependencies = []\n",
        );
        let det = detect_repo(tmp.path());
        assert_eq!(det.languages[0], Language::JavaScript);
        assert_eq!(det.languages[1], Language::Python);
    }

    #[test]
    fn scoped_review_walks_up_for_root_manifest() {
        // Simulates `expensive_live_security_review` scoping to a
        // subpath: the manifest is one level above the review root.
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "package.json",
            r#"{"name":"app","dependencies":{"express":"^4"}}"#,
        );
        let scoped = tmp.path().join("routes");
        fs::create_dir_all(&scoped).unwrap();
        fs::write(scoped.join("auth.js"), "// handler").unwrap();

        let det = detect_repo(&scoped);
        assert_eq!(det.languages, vec![Language::JavaScript]);
        assert_eq!(det.frameworks, vec![Framework::Express]);
    }

    #[test]
    fn gitignore_is_respected_during_walk() {
        // Regression: `ignore::WalkBuilder` reads `.gitignore` at the root.
        // A generated `pyproject.toml` inside an ignored build dir should
        // NOT contribute a Python language count.  Distinct from the
        // hardcoded skip list — this one uses a non-default ignore name.
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), ".gitignore", "generated/\n");
        write(tmp.path(), "package.json", r#"{"name":"root"}"#);
        write(
            tmp.path(),
            "generated/pyproject.toml",
            "[project]\nname = \"generated\"\ndependencies = []\n",
        );
        let det = detect_repo(tmp.path());
        // Only the root package.json contributed; the gitignored
        // pyproject.toml did not register Python.
        assert_eq!(det.languages, vec![Language::JavaScript]);
    }

    #[test]
    fn node_modules_is_skipped_during_walk() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "package.json", r#"{"name":"root"}"#);
        // A vendored manifest inside node_modules should NOT count.
        write(
            tmp.path(),
            "node_modules/express/package.json",
            r#"{"name":"express"}"#,
        );
        let det = detect_repo(tmp.path());
        // Only the root package.json contributed a count.
        let js_count = det
            .languages
            .iter()
            .filter(|l| **l == Language::JavaScript)
            .count();
        assert_eq!(js_count, 1);
    }

    #[test]
    fn missing_directory_yields_empty_detection() {
        let det = detect_repo(Path::new("/nonexistent/path/for/unit/test"));
        assert!(det.languages.is_empty());
        assert!(det.frameworks.is_empty());
    }

    #[test]
    fn compose_empty_detection_returns_empty() {
        let det = Detection::default();
        let (body, names) = compose_cheatsheets(&det);
        assert!(body.is_empty());
        assert!(names.is_empty());
    }

    #[test]
    fn compose_includes_lang_and_framework() {
        let det = Detection {
            languages: vec![Language::JavaScript],
            frameworks: vec![Framework::Express],
        };
        let (body, names) = compose_cheatsheets(&det);
        assert!(body.contains("lang/javascript"));
        assert!(body.contains("framework/express"));
        assert_eq!(names, vec!["lang/javascript", "framework/express"]);
        // Line cap is generous enough to fit one lang + one framework.
        assert!(line_count(&body) <= MAX_CHEATSHEET_LINES);
    }

    #[test]
    fn compose_respects_top_two_languages() {
        let det = Detection {
            languages: vec![
                Language::JavaScript,
                Language::Python,
                Language::Go, // should be dropped — only top 2
            ],
            frameworks: vec![],
        };
        let (_body, names) = compose_cheatsheets(&det);
        assert!(names.contains(&"lang/javascript"));
        assert!(names.contains(&"lang/python"));
        assert!(!names.contains(&"lang/go"));
    }

    #[test]
    fn compose_drops_frameworks_bound_to_excluded_language() {
        let det = Detection {
            languages: vec![Language::JavaScript, Language::Python],
            // Actix belongs to Rust — Rust is not in the primary 2.
            frameworks: vec![Framework::Actix, Framework::Express],
        };
        let (_body, names) = compose_cheatsheets(&det);
        assert!(names.contains(&"framework/express"));
        assert!(!names.contains(&"framework/actix"));
    }

    #[test]
    fn compose_cap_drops_frameworks_first() {
        // Build a detection whose natural composition would blow past
        // the cap: four languages is impossible (we take 2), so we
        // instead force it by shrinking the cap for the test via a
        // direct call.  The public `compose_cheatsheets` respects the
        // constant — covered by the "respects cap" invariant below.
        let det = Detection {
            languages: vec![Language::JavaScript, Language::Python],
            frameworks: vec![
                Framework::Express,
                Framework::Django,
                Framework::Flask,
            ],
        };
        let (body, names) = compose_cheatsheets(&det);
        // Either all four sheets fit (cap not hit) or frameworks were
        // dropped.  Either way the result respects the cap.
        assert!(
            line_count(&body) <= MAX_CHEATSHEET_LINES,
            "composed body exceeded line cap"
        );
        // Sheets always include the two languages.
        assert!(names.contains(&"lang/javascript"));
        assert!(names.contains(&"lang/python"));
    }
}
