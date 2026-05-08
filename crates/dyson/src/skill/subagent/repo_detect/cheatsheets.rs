use std::path::Path;

use super::detector::detect_repo;
use super::{Detection, Framework, Language};

/// Upper bound on total injected cheatsheet content.  Past this, drop
/// frameworks first, then the second language.
pub(super) const MAX_CHEATSHEET_LINES: usize = 400;

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
    let primary_langs: Vec<Language> = detection.languages.iter().take(2).copied().collect();
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

    build_prompt(&primary_langs[..1], &[])
}

pub(super) fn line_count(s: &str) -> usize {
    if s.is_empty() { 0 } else { s.lines().count() }
}

fn build_prompt(languages: &[Language], frameworks: &[Framework]) -> (String, Vec<&'static str>) {
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

pub(super) fn lang_sheet(lang: Language) -> (&'static str, &'static str) {
    match lang {
        Language::Python => (
            "lang/python",
            include_str!("../prompts/cheatsheets/lang/python.md"),
        ),
        Language::JavaScript => (
            "lang/javascript",
            include_str!("../prompts/cheatsheets/lang/javascript.md"),
        ),
        Language::Go => ("lang/go", include_str!("../prompts/cheatsheets/lang/go.md")),
        Language::Rust => (
            "lang/rust",
            include_str!("../prompts/cheatsheets/lang/rust.md"),
        ),
        Language::Ruby => (
            "lang/ruby",
            include_str!("../prompts/cheatsheets/lang/ruby.md"),
        ),
        Language::Java => (
            "lang/java",
            include_str!("../prompts/cheatsheets/lang/java.md"),
        ),
        Language::Kotlin => (
            "lang/kotlin",
            include_str!("../prompts/cheatsheets/lang/kotlin.md"),
        ),
        Language::CSharp => (
            "lang/csharp",
            include_str!("../prompts/cheatsheets/lang/csharp.md"),
        ),
        Language::Php => (
            "lang/php",
            include_str!("../prompts/cheatsheets/lang/php.md"),
        ),
        Language::Cpp => (
            "lang/cpp",
            include_str!("../prompts/cheatsheets/lang/cpp.md"),
        ),
        Language::Elixir => (
            "lang/elixir",
            include_str!("../prompts/cheatsheets/lang/elixir.md"),
        ),
        Language::Haskell => (
            "lang/haskell",
            include_str!("../prompts/cheatsheets/lang/haskell.md"),
        ),
        Language::Swift => (
            "lang/swift",
            include_str!("../prompts/cheatsheets/lang/swift.md"),
        ),
        Language::Ocaml => (
            "lang/ocaml",
            include_str!("../prompts/cheatsheets/lang/ocaml.md"),
        ),
        Language::Erlang => (
            "lang/erlang",
            include_str!("../prompts/cheatsheets/lang/erlang.md"),
        ),
        Language::Zig => (
            "lang/zig",
            include_str!("../prompts/cheatsheets/lang/zig.md"),
        ),
        Language::Nix => (
            "lang/nix",
            include_str!("../prompts/cheatsheets/lang/nix.md"),
        ),
        Language::Lua => (
            "lang/lua",
            include_str!("../prompts/cheatsheets/lang/lua.md"),
        ),
    }
}

pub(super) fn framework_sheet(fw: Framework) -> (&'static str, &'static str) {
    match fw {
        Framework::Django => (
            "framework/django",
            include_str!("../prompts/cheatsheets/framework/django.md"),
        ),
        Framework::Flask => (
            "framework/flask",
            include_str!("../prompts/cheatsheets/framework/flask.md"),
        ),
        Framework::FastApi => (
            "framework/fastapi",
            include_str!("../prompts/cheatsheets/framework/fastapi.md"),
        ),
        Framework::Express => (
            "framework/express",
            include_str!("../prompts/cheatsheets/framework/express.md"),
        ),
        Framework::NextJs => (
            "framework/nextjs",
            include_str!("../prompts/cheatsheets/framework/nextjs.md"),
        ),
        Framework::Actix => (
            "framework/actix",
            include_str!("../prompts/cheatsheets/framework/actix.md"),
        ),
        Framework::Axum => (
            "framework/axum",
            include_str!("../prompts/cheatsheets/framework/axum.md"),
        ),
        Framework::Solana => (
            "framework/solana",
            include_str!("../prompts/cheatsheets/framework/solana.md"),
        ),
        Framework::Rails => (
            "framework/rails",
            include_str!("../prompts/cheatsheets/framework/rails.md"),
        ),
        Framework::Spring => (
            "framework/spring",
            include_str!("../prompts/cheatsheets/framework/spring.md"),
        ),
        Framework::AspNet => (
            "framework/aspnet",
            include_str!("../prompts/cheatsheets/framework/aspnet.md"),
        ),
        Framework::Laravel => (
            "framework/laravel",
            include_str!("../prompts/cheatsheets/framework/laravel.md"),
        ),
        Framework::Phoenix => (
            "framework/phoenix",
            include_str!("../prompts/cheatsheets/framework/phoenix.md"),
        ),
        Framework::Gin => (
            "framework/gin",
            include_str!("../prompts/cheatsheets/framework/gin.md"),
        ),
        Framework::Aiohttp => (
            "framework/aiohttp",
            include_str!("../prompts/cheatsheets/framework/aiohttp.md"),
        ),
        Framework::Fastify => (
            "framework/fastify",
            include_str!("../prompts/cheatsheets/framework/fastify.md"),
        ),
        Framework::NestJs => (
            "framework/nestjs",
            include_str!("../prompts/cheatsheets/framework/nestjs.md"),
        ),
        Framework::Trpc => (
            "framework/trpc",
            include_str!("../prompts/cheatsheets/framework/trpc.md"),
        ),
        Framework::Rocket => (
            "framework/rocket",
            include_str!("../prompts/cheatsheets/framework/rocket.md"),
        ),
        Framework::Sinatra => (
            "framework/sinatra",
            include_str!("../prompts/cheatsheets/framework/sinatra.md"),
        ),
        Framework::Ktor => (
            "framework/ktor",
            include_str!("../prompts/cheatsheets/framework/ktor.md"),
        ),
        Framework::Symfony => (
            "framework/symfony",
            include_str!("../prompts/cheatsheets/framework/symfony.md"),
        ),
        Framework::Echo => (
            "framework/echo",
            include_str!("../prompts/cheatsheets/framework/echo.md"),
        ),
        Framework::Chi => (
            "framework/chi",
            include_str!("../prompts/cheatsheets/framework/chi.md"),
        ),
        Framework::Vapor => (
            "framework/vapor",
            include_str!("../prompts/cheatsheets/framework/vapor.md"),
        ),
        Framework::Tornado => (
            "framework/tornado",
            include_str!("../prompts/cheatsheets/framework/tornado.md"),
        ),
        Framework::Sanic => (
            "framework/sanic",
            include_str!("../prompts/cheatsheets/framework/sanic.md"),
        ),
        Framework::Celery => (
            "framework/celery",
            include_str!("../prompts/cheatsheets/framework/celery.md"),
        ),
        Framework::Koa => (
            "framework/koa",
            include_str!("../prompts/cheatsheets/framework/koa.md"),
        ),
        Framework::Hono => (
            "framework/hono",
            include_str!("../prompts/cheatsheets/framework/hono.md"),
        ),
        Framework::SvelteKit => (
            "framework/sveltekit",
            include_str!("../prompts/cheatsheets/framework/sveltekit.md"),
        ),
        Framework::Remix => (
            "framework/remix",
            include_str!("../prompts/cheatsheets/framework/remix.md"),
        ),
        Framework::GraphQL => (
            "framework/graphql",
            include_str!("../prompts/cheatsheets/framework/graphql.md"),
        ),
        Framework::Warp => (
            "framework/warp",
            include_str!("../prompts/cheatsheets/framework/warp.md"),
        ),
        Framework::Tonic => (
            "framework/tonic",
            include_str!("../prompts/cheatsheets/framework/tonic.md"),
        ),
        Framework::Quarkus => (
            "framework/quarkus",
            include_str!("../prompts/cheatsheets/framework/quarkus.md"),
        ),
        Framework::Micronaut => (
            "framework/micronaut",
            include_str!("../prompts/cheatsheets/framework/micronaut.md"),
        ),
        Framework::Javalin => (
            "framework/javalin",
            include_str!("../prompts/cheatsheets/framework/javalin.md"),
        ),
        Framework::Slim => (
            "framework/slim",
            include_str!("../prompts/cheatsheets/framework/slim.md"),
        ),
        Framework::CodeIgniter => (
            "framework/codeigniter",
            include_str!("../prompts/cheatsheets/framework/codeigniter.md"),
        ),
        Framework::Fiber => (
            "framework/fiber",
            include_str!("../prompts/cheatsheets/framework/fiber.md"),
        ),
        Framework::GorillaMux => (
            "framework/gorilla-mux",
            include_str!("../prompts/cheatsheets/framework/gorilla-mux.md"),
        ),
        Framework::Hummingbird => (
            "framework/hummingbird",
            include_str!("../prompts/cheatsheets/framework/hummingbird.md"),
        ),
        Framework::Servant => (
            "framework/servant",
            include_str!("../prompts/cheatsheets/framework/servant.md"),
        ),
        Framework::Dream => (
            "framework/dream",
            include_str!("../prompts/cheatsheets/framework/dream.md"),
        ),
        Framework::Cowboy => (
            "framework/cowboy",
            include_str!("../prompts/cheatsheets/framework/cowboy.md"),
        ),
        Framework::Starlette => (
            "framework/starlette",
            include_str!("../prompts/cheatsheets/framework/starlette.md"),
        ),
        Framework::Pyramid => (
            "framework/pyramid",
            include_str!("../prompts/cheatsheets/framework/pyramid.md"),
        ),
        Framework::Falcon => (
            "framework/falcon",
            include_str!("../prompts/cheatsheets/framework/falcon.md"),
        ),
        Framework::Bottle => (
            "framework/bottle",
            include_str!("../prompts/cheatsheets/framework/bottle.md"),
        ),
        Framework::Play => (
            "framework/play",
            include_str!("../prompts/cheatsheets/framework/play.md"),
        ),
        Framework::Dropwizard => (
            "framework/dropwizard",
            include_str!("../prompts/cheatsheets/framework/dropwizard.md"),
        ),
        Framework::Helidon => (
            "framework/helidon",
            include_str!("../prompts/cheatsheets/framework/helidon.md"),
        ),
        Framework::Vertx => (
            "framework/vertx",
            include_str!("../prompts/cheatsheets/framework/vertx.md"),
        ),
        Framework::Hapi => (
            "framework/hapi",
            include_str!("../prompts/cheatsheets/framework/hapi.md"),
        ),
        Framework::Adonis => (
            "framework/adonis",
            include_str!("../prompts/cheatsheets/framework/adonis.md"),
        ),
        Framework::Meteor => (
            "framework/meteor",
            include_str!("../prompts/cheatsheets/framework/meteor.md"),
        ),
        Framework::Nuxt => (
            "framework/nuxt",
            include_str!("../prompts/cheatsheets/framework/nuxt.md"),
        ),
        Framework::OpenResty => (
            "framework/openresty",
            include_str!("../prompts/cheatsheets/framework/openresty.md"),
        ),
    }
}

/// Convenience: detect + compose in one call.  The tool layer uses this.
pub fn detect_and_compose(root: &Path) -> (String, Vec<&'static str>) {
    compose_cheatsheets(&detect_repo(root))
}

fn assert_send_sync<T: Send + Sync>() {}

const _: fn() = assert_send_sync::<Detection>;
