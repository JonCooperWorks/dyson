use super::*;

use std::fs;
use std::path::{Path, PathBuf};
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
/// test fails to compile — the `match` loses exhaustivity.
#[test]
fn enum_lists_are_exhaustive() {
    fn match_guard(lang: Language, fw: Framework) {
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

    for lang in ALL_LANGUAGES {
        match_guard(*lang, Framework::Django);
    }
    for fw in ALL_FRAMEWORKS {
        match_guard(Language::Python, *fw);
    }
}

/// For every variant: `language_briefing` returns a non-empty name and a
/// non-empty body that begins with the documented "Starting points"
/// opener.  Catches a forgotten `include_str!` wiring (missing
/// match arm would be a compile error — this asserts the content).
#[test]
fn every_language_has_a_nonempty_sheet() {
    for &lang in ALL_LANGUAGES {
        let (name, body) = language_briefing(lang);
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
        let (name, body) = framework_briefing(fw);
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

/// For every Language variant: a synthetic detection containing
/// just that language composes a prompt whose included-sheet list
/// names that language's sheet.  Exercises the full detection →
/// compose pipeline per variant.

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
    write(tmp.path(), "Cargo.toml", "[dependencies]\naxum = \"0.7\"\n");
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
    write(tmp.path(), "go.mod", "module example.com/x\n\ngo 1.22\n");
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
    // framework briefing binds to Java — but the language
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
    write(tmp.path(), "requirements.txt", "fastapi>=0.100\nuvicorn\n");
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
    assert!(
        detect_repo(tmp.path())
            .frameworks
            .contains(&Framework::Starlette)
    );
}

#[test]
fn detects_pyramid_via_pyproject() {
    let tmp = TempDir::new().unwrap();
    write(
        tmp.path(),
        "pyproject.toml",
        "[project]\nname = \"x\"\ndependencies = [\"pyramid>=2\"]\n",
    );
    assert!(
        detect_repo(tmp.path())
            .frameworks
            .contains(&Framework::Pyramid)
    );
}

#[test]
fn detects_falcon_via_requirements() {
    let tmp = TempDir::new().unwrap();
    write(tmp.path(), "requirements.txt", "falcon>=3\n");
    assert!(
        detect_repo(tmp.path())
            .frameworks
            .contains(&Framework::Falcon)
    );
}

#[test]
fn detects_bottle_via_requirements() {
    let tmp = TempDir::new().unwrap();
    write(tmp.path(), "requirements.txt", "bottle>=0.12\n");
    assert!(
        detect_repo(tmp.path())
            .frameworks
            .contains(&Framework::Bottle)
    );
}

#[test]
fn detects_play_via_pom() {
    let tmp = TempDir::new().unwrap();
    write(
        tmp.path(),
        "pom.xml",
        "<project><dependencies><dependency><groupId>com.typesafe.play</groupId><artifactId>play-java</artifactId></dependency></dependencies></project>\n",
    );
    assert!(
        detect_repo(tmp.path())
            .frameworks
            .contains(&Framework::Play)
    );
}

#[test]
fn detects_dropwizard_via_pom() {
    let tmp = TempDir::new().unwrap();
    write(
        tmp.path(),
        "pom.xml",
        "<project><dependencies><dependency><groupId>io.dropwizard</groupId><artifactId>dropwizard-core</artifactId></dependency></dependencies></project>\n",
    );
    assert!(
        detect_repo(tmp.path())
            .frameworks
            .contains(&Framework::Dropwizard)
    );
}

#[test]
fn detects_helidon_via_pom() {
    let tmp = TempDir::new().unwrap();
    write(
        tmp.path(),
        "pom.xml",
        "<project><dependencies><dependency><groupId>io.helidon.webserver</groupId><artifactId>helidon-webserver</artifactId></dependency></dependencies></project>\n",
    );
    assert!(
        detect_repo(tmp.path())
            .frameworks
            .contains(&Framework::Helidon)
    );
}

#[test]
fn detects_vertx_via_pom() {
    let tmp = TempDir::new().unwrap();
    write(
        tmp.path(),
        "pom.xml",
        "<project><dependencies><dependency><groupId>io.vertx</groupId><artifactId>vertx-core</artifactId></dependency></dependencies></project>\n",
    );
    assert!(
        detect_repo(tmp.path())
            .frameworks
            .contains(&Framework::Vertx)
    );
}

#[test]
fn detects_hapi_via_package_json() {
    let tmp = TempDir::new().unwrap();
    write(
        tmp.path(),
        "package.json",
        r#"{"name":"app","dependencies":{"@hapi/hapi":"^21"}}"#,
    );
    assert!(
        detect_repo(tmp.path())
            .frameworks
            .contains(&Framework::Hapi)
    );
}

#[test]
fn detects_adonis_via_package_json() {
    let tmp = TempDir::new().unwrap();
    write(
        tmp.path(),
        "package.json",
        r#"{"name":"app","dependencies":{"@adonisjs/core":"^6"}}"#,
    );
    assert!(
        detect_repo(tmp.path())
            .frameworks
            .contains(&Framework::Adonis)
    );
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
    assert!(
        detect_repo(tmp.path())
            .frameworks
            .contains(&Framework::Meteor)
    );
}

#[test]
fn detects_nuxt_via_package_json() {
    let tmp = TempDir::new().unwrap();
    write(
        tmp.path(),
        "package.json",
        r#"{"name":"app","devDependencies":{"nuxt":"^3"}}"#,
    );
    assert!(
        detect_repo(tmp.path())
            .frameworks
            .contains(&Framework::Nuxt)
    );
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
    assert!(
        detect_repo(tmp.path())
            .frameworks
            .contains(&Framework::OpenResty)
    );
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
    assert!(detect_repo(tmp.path()).frameworks.contains(&Framework::Koa));
}

#[test]
fn detects_hono_via_package_json() {
    let tmp = TempDir::new().unwrap();
    write(
        tmp.path(),
        "package.json",
        r#"{"name":"app","dependencies":{"hono":"^4"}}"#,
    );
    assert!(
        detect_repo(tmp.path())
            .frameworks
            .contains(&Framework::Hono)
    );
}

#[test]
fn detects_sveltekit_via_package_json() {
    let tmp = TempDir::new().unwrap();
    write(
        tmp.path(),
        "package.json",
        r#"{"name":"app","devDependencies":{"@sveltejs/kit":"^2"}}"#,
    );
    assert!(
        detect_repo(tmp.path())
            .frameworks
            .contains(&Framework::SvelteKit)
    );
}

#[test]
fn detects_remix_via_package_json() {
    let tmp = TempDir::new().unwrap();
    write(
        tmp.path(),
        "package.json",
        r#"{"name":"app","dependencies":{"@remix-run/node":"^2"}}"#,
    );
    assert!(
        detect_repo(tmp.path())
            .frameworks
            .contains(&Framework::Remix)
    );
}

#[test]
fn detects_graphql_via_package_json() {
    let tmp = TempDir::new().unwrap();
    write(
        tmp.path(),
        "package.json",
        r#"{"name":"app","dependencies":{"@apollo/server":"^4","graphql":"^16"}}"#,
    );
    assert!(
        detect_repo(tmp.path())
            .frameworks
            .contains(&Framework::GraphQL)
    );
}

#[test]
fn detects_tornado_via_requirements() {
    let tmp = TempDir::new().unwrap();
    write(tmp.path(), "requirements.txt", "tornado>=6.4\n");
    assert!(
        detect_repo(tmp.path())
            .frameworks
            .contains(&Framework::Tornado)
    );
}

#[test]
fn detects_sanic_via_requirements() {
    let tmp = TempDir::new().unwrap();
    write(tmp.path(), "requirements.txt", "sanic>=23\n");
    assert!(
        detect_repo(tmp.path())
            .frameworks
            .contains(&Framework::Sanic)
    );
}

#[test]
fn detects_celery_via_requirements() {
    let tmp = TempDir::new().unwrap();
    write(tmp.path(), "requirements.txt", "celery>=5.3\nredis\n");
    assert!(
        detect_repo(tmp.path())
            .frameworks
            .contains(&Framework::Celery)
    );
}

#[test]
fn detects_fiber_via_go_mod() {
    let tmp = TempDir::new().unwrap();
    write(
        tmp.path(),
        "go.mod",
        "module x\n\ngo 1.22\n\nrequire github.com/gofiber/fiber/v2 v2.52.0\n",
    );
    assert!(
        detect_repo(tmp.path())
            .frameworks
            .contains(&Framework::Fiber)
    );
}

#[test]
fn detects_gorilla_mux_via_go_mod() {
    let tmp = TempDir::new().unwrap();
    write(
        tmp.path(),
        "go.mod",
        "module x\n\ngo 1.22\n\nrequire github.com/gorilla/mux v1.8.1\n",
    );
    assert!(
        detect_repo(tmp.path())
            .frameworks
            .contains(&Framework::GorillaMux)
    );
}

#[test]
fn detects_warp_via_cargo_toml() {
    let tmp = TempDir::new().unwrap();
    write(tmp.path(), "Cargo.toml", "[dependencies]\nwarp = \"0.3\"\n");
    assert!(
        detect_repo(tmp.path())
            .frameworks
            .contains(&Framework::Warp)
    );
}

#[test]
fn detects_tonic_via_cargo_toml() {
    let tmp = TempDir::new().unwrap();
    write(
        tmp.path(),
        "Cargo.toml",
        "[dependencies]\ntonic = \"0.11\"\nprost = \"0.12\"\n",
    );
    assert!(
        detect_repo(tmp.path())
            .frameworks
            .contains(&Framework::Tonic)
    );
}

#[test]
fn detects_quarkus_via_pom_xml() {
    let tmp = TempDir::new().unwrap();
    write(
        tmp.path(),
        "pom.xml",
        "<project><dependencies>\n  <dependency><groupId>io.quarkus</groupId><artifactId>quarkus-rest</artifactId></dependency>\n</dependencies></project>\n",
    );
    assert!(
        detect_repo(tmp.path())
            .frameworks
            .contains(&Framework::Quarkus)
    );
}

#[test]
fn detects_micronaut_via_gradle() {
    let tmp = TempDir::new().unwrap();
    write(
        tmp.path(),
        "build.gradle",
        "plugins { id 'io.micronaut.application' version '4.3' }\n",
    );
    assert!(
        detect_repo(tmp.path())
            .frameworks
            .contains(&Framework::Micronaut)
    );
}

#[test]
fn detects_javalin_via_gradle() {
    let tmp = TempDir::new().unwrap();
    write(
        tmp.path(),
        "build.gradle",
        "dependencies { implementation 'io.javalin:javalin:6.0.0' }\n",
    );
    assert!(
        detect_repo(tmp.path())
            .frameworks
            .contains(&Framework::Javalin)
    );
}

#[test]
fn detects_slim_via_composer() {
    let tmp = TempDir::new().unwrap();
    write(
        tmp.path(),
        "composer.json",
        r#"{"name":"app","require":{"slim/slim":"^4"}}"#,
    );
    assert!(
        detect_repo(tmp.path())
            .frameworks
            .contains(&Framework::Slim)
    );
}

#[test]
fn detects_codeigniter_via_composer() {
    let tmp = TempDir::new().unwrap();
    write(
        tmp.path(),
        "composer.json",
        r#"{"name":"app","require":{"codeigniter4/framework":"^4"}}"#,
    );
    assert!(
        detect_repo(tmp.path())
            .frameworks
            .contains(&Framework::CodeIgniter)
    );
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
    assert!(
        detect_repo(tmp.path())
            .frameworks
            .contains(&Framework::Servant)
    );
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
    assert!(
        detect_repo(tmp.path())
            .frameworks
            .contains(&Framework::Dream)
    );
}

#[test]
fn detects_cowboy_via_rebar_config() {
    let tmp = TempDir::new().unwrap();
    write(
        tmp.path(),
        "rebar.config",
        "{deps, [\n  {cowboy, \"2.10.0\"}\n]}.\n",
    );
    assert!(
        detect_repo(tmp.path())
            .frameworks
            .contains(&Framework::Cowboy)
    );
}

#[test]
fn detects_hummingbird_via_package_swift() {
    let tmp = TempDir::new().unwrap();
    write(
        tmp.path(),
        "Package.swift",
        "// swift-tools-version:5.9\nimport PackageDescription\nlet package = Package(\n  name: \"app\",\n  dependencies: [\n    .package(url: \"https://github.com/hummingbird-project/hummingbird.git\", from: \"2.0.0\"),\n  ]\n)\n",
    );
    assert!(
        detect_repo(tmp.path())
            .frameworks
            .contains(&Framework::Hummingbird)
    );
}

#[test]
fn detects_aiohttp_via_requirements() {
    let tmp = TempDir::new().unwrap();
    write(tmp.path(), "requirements.txt", "aiohttp>=3.9\nuvloop\n");
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
