# Repository Detection (`skill/subagent/repo_detect`)

## Overview
Runtime repository detection used by the `security_engineer` subagent to select specialist hunters. `detect_repo` shallow-parses build manifests to identify the languages in a review target and the frameworks those languages' dependency lists pull in; matching reference briefings are bundled at build time with `include_str!`.

## Interface
- `detect_repo(root: &Path) -> Detection` [detector.rs:detect_repo]
- `Detection { languages: Vec<Language>, frameworks: Vec<Framework> }` [types.rs:Detection]
- `Language`, `Framework` enums (re-exported `pub`) [types.rs:Language, types.rs:Framework, mod.rs]
- `language_briefing(lang)` / `framework_briefing(fw)` -> `(&'static str, &'static str)` bundled briefing text [briefings.rs:language_briefing, briefings.rs:framework_briefing]
- Manifest scanners: `scan_package_json`, `scan_pyproject_toml`, `scan_requirements_txt`, `scan_cargo_toml`, `scan_gemfile`, `scan_pom_xml`, `scan_build_gradle`, `scan_composer_json`, `scan_mix_exs`, `scan_dotnet_project`, `scan_go_mod`, `scan_package_swift`, `scan_cabal_file`, `scan_dune_file`, `scan_rebar_config`, `scan_rockspec` [rules.rs:scan_package_json]

## Invariants
- Downward walk limited to `DOWN_WALK_DEPTH = 3` levels; ancestor probe limited to `UP_WALK_DEPTH = 5` so scoped reviews (e.g. `repo/routes/`) still find root manifests [detector.rs:DOWN_WALK_DEPTH, detector.rs:UP_WALK_DEPTH]
- `languages` is ranked by manifest count descending, ties broken by stable enum order; `frameworks` preserves discovery order [types.rs:Detection]
- `Detection` is `Send + Sync`, asserted at compile time [mod.rs:assert_send_sync]

## Side Effects
- Reads directories and manifest files from the filesystem (read-only) [detector.rs:detect_repo]

## Verification
- Run: `cargo test -p dyson` (from `dyson/`)
