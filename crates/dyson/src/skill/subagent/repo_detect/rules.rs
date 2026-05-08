use std::collections::HashSet;

use super::Framework;

/// `package.json` framework detection: treat the whole document as a
/// bag of strings and look for top-level dep keys.  Misses scoped
/// workspaces with deps hoisted elsewhere, but those are rare and the
/// sheet is still useful for a pure JS repo without Express.
pub(super) fn scan_package_json(
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
pub(super) fn scan_pyproject_toml(
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
pub(super) fn scan_requirements_txt(
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
        .find(['[', '=', '<', '>', '!', '~', ';', ' ', '\t'])
        .unwrap_or(req.len());
    req[..cut].trim().to_ascii_lowercase()
}

pub(super) fn scan_cargo_toml(
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
pub(super) fn scan_gemfile(
    contents: &str,
    frameworks: &mut Vec<Framework>,
    seen: &mut HashSet<Framework>,
) {
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
pub(super) fn scan_pom_xml(
    contents: &str,
    frameworks: &mut Vec<Framework>,
    seen: &mut HashSet<Framework>,
) {
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
    if lower.contains("com.typesafe.play")
        || lower.contains("play-java")
        || lower.contains("play-scala")
    {
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
pub(super) fn scan_build_gradle(
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
    if lower.contains("com.typesafe.play")
        || lower.contains("play-java")
        || lower.contains("play-scala")
    {
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
pub(super) fn scan_composer_json(
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
pub(super) fn scan_mix_exs(
    contents: &str,
    frameworks: &mut Vec<Framework>,
    seen: &mut HashSet<Framework>,
) {
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
pub(super) fn scan_dotnet_project(
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
pub(super) fn scan_go_mod(
    contents: &str,
    frameworks: &mut Vec<Framework>,
    seen: &mut HashSet<Framework>,
) {
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
pub(super) fn scan_package_swift(
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
pub(super) fn scan_cabal_file(
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
pub(super) fn scan_dune_file(
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
pub(super) fn scan_rebar_config(
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
pub(super) fn scan_rockspec(
    contents: &str,
    frameworks: &mut Vec<Framework>,
    seen: &mut HashSet<Framework>,
) {
    let lower = contents.to_ascii_lowercase();
    if lower.contains("lua-resty-") || lower.contains("openresty") {
        push_framework(Framework::OpenResty, frameworks, seen);
    }
}
