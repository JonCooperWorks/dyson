// Per-language and per-framework security briefings.  Each is the reference
// material for a dedicated framework/language specialist hunter spawned by
// the security_engineer hunt stage — carried in that agent's own context,
// never concatenated into a shared prompt.  Bundled into the binary at build
// time via include_str!.

use super::{Framework, Language};

pub(crate) fn language_briefing(lang: Language) -> (&'static str, &'static str) {
    match lang {
        Language::Python => (
            "lang/python",
            include_str!("../prompts/briefings/lang/python.md"),
        ),
        Language::JavaScript => (
            "lang/javascript",
            include_str!("../prompts/briefings/lang/javascript.md"),
        ),
        Language::Go => ("lang/go", include_str!("../prompts/briefings/lang/go.md")),
        Language::Rust => (
            "lang/rust",
            include_str!("../prompts/briefings/lang/rust.md"),
        ),
        Language::Ruby => (
            "lang/ruby",
            include_str!("../prompts/briefings/lang/ruby.md"),
        ),
        Language::Java => (
            "lang/java",
            include_str!("../prompts/briefings/lang/java.md"),
        ),
        Language::Kotlin => (
            "lang/kotlin",
            include_str!("../prompts/briefings/lang/kotlin.md"),
        ),
        Language::CSharp => (
            "lang/csharp",
            include_str!("../prompts/briefings/lang/csharp.md"),
        ),
        Language::Php => ("lang/php", include_str!("../prompts/briefings/lang/php.md")),
        Language::Cpp => ("lang/cpp", include_str!("../prompts/briefings/lang/cpp.md")),
        Language::Elixir => (
            "lang/elixir",
            include_str!("../prompts/briefings/lang/elixir.md"),
        ),
        Language::Haskell => (
            "lang/haskell",
            include_str!("../prompts/briefings/lang/haskell.md"),
        ),
        Language::Swift => (
            "lang/swift",
            include_str!("../prompts/briefings/lang/swift.md"),
        ),
        Language::Ocaml => (
            "lang/ocaml",
            include_str!("../prompts/briefings/lang/ocaml.md"),
        ),
        Language::Erlang => (
            "lang/erlang",
            include_str!("../prompts/briefings/lang/erlang.md"),
        ),
        Language::Zig => ("lang/zig", include_str!("../prompts/briefings/lang/zig.md")),
        Language::Nix => ("lang/nix", include_str!("../prompts/briefings/lang/nix.md")),
        Language::Lua => ("lang/lua", include_str!("../prompts/briefings/lang/lua.md")),
    }
}

pub(crate) fn framework_briefing(fw: Framework) -> (&'static str, &'static str) {
    match fw {
        Framework::Django => (
            "framework/django",
            include_str!("../prompts/briefings/framework/django.md"),
        ),
        Framework::Flask => (
            "framework/flask",
            include_str!("../prompts/briefings/framework/flask.md"),
        ),
        Framework::FastApi => (
            "framework/fastapi",
            include_str!("../prompts/briefings/framework/fastapi.md"),
        ),
        Framework::Express => (
            "framework/express",
            include_str!("../prompts/briefings/framework/express.md"),
        ),
        Framework::NextJs => (
            "framework/nextjs",
            include_str!("../prompts/briefings/framework/nextjs.md"),
        ),
        Framework::Actix => (
            "framework/actix",
            include_str!("../prompts/briefings/framework/actix.md"),
        ),
        Framework::Axum => (
            "framework/axum",
            include_str!("../prompts/briefings/framework/axum.md"),
        ),
        Framework::Solana => (
            "framework/solana",
            include_str!("../prompts/briefings/framework/solana.md"),
        ),
        Framework::Rails => (
            "framework/rails",
            include_str!("../prompts/briefings/framework/rails.md"),
        ),
        Framework::Spring => (
            "framework/spring",
            include_str!("../prompts/briefings/framework/spring.md"),
        ),
        Framework::AspNet => (
            "framework/aspnet",
            include_str!("../prompts/briefings/framework/aspnet.md"),
        ),
        Framework::Laravel => (
            "framework/laravel",
            include_str!("../prompts/briefings/framework/laravel.md"),
        ),
        Framework::Phoenix => (
            "framework/phoenix",
            include_str!("../prompts/briefings/framework/phoenix.md"),
        ),
        Framework::Gin => (
            "framework/gin",
            include_str!("../prompts/briefings/framework/gin.md"),
        ),
        Framework::Aiohttp => (
            "framework/aiohttp",
            include_str!("../prompts/briefings/framework/aiohttp.md"),
        ),
        Framework::Fastify => (
            "framework/fastify",
            include_str!("../prompts/briefings/framework/fastify.md"),
        ),
        Framework::NestJs => (
            "framework/nestjs",
            include_str!("../prompts/briefings/framework/nestjs.md"),
        ),
        Framework::Trpc => (
            "framework/trpc",
            include_str!("../prompts/briefings/framework/trpc.md"),
        ),
        Framework::Rocket => (
            "framework/rocket",
            include_str!("../prompts/briefings/framework/rocket.md"),
        ),
        Framework::Sinatra => (
            "framework/sinatra",
            include_str!("../prompts/briefings/framework/sinatra.md"),
        ),
        Framework::Ktor => (
            "framework/ktor",
            include_str!("../prompts/briefings/framework/ktor.md"),
        ),
        Framework::Symfony => (
            "framework/symfony",
            include_str!("../prompts/briefings/framework/symfony.md"),
        ),
        Framework::Echo => (
            "framework/echo",
            include_str!("../prompts/briefings/framework/echo.md"),
        ),
        Framework::Chi => (
            "framework/chi",
            include_str!("../prompts/briefings/framework/chi.md"),
        ),
        Framework::Vapor => (
            "framework/vapor",
            include_str!("../prompts/briefings/framework/vapor.md"),
        ),
        Framework::Tornado => (
            "framework/tornado",
            include_str!("../prompts/briefings/framework/tornado.md"),
        ),
        Framework::Sanic => (
            "framework/sanic",
            include_str!("../prompts/briefings/framework/sanic.md"),
        ),
        Framework::Celery => (
            "framework/celery",
            include_str!("../prompts/briefings/framework/celery.md"),
        ),
        Framework::Koa => (
            "framework/koa",
            include_str!("../prompts/briefings/framework/koa.md"),
        ),
        Framework::Hono => (
            "framework/hono",
            include_str!("../prompts/briefings/framework/hono.md"),
        ),
        Framework::SvelteKit => (
            "framework/sveltekit",
            include_str!("../prompts/briefings/framework/sveltekit.md"),
        ),
        Framework::Remix => (
            "framework/remix",
            include_str!("../prompts/briefings/framework/remix.md"),
        ),
        Framework::GraphQL => (
            "framework/graphql",
            include_str!("../prompts/briefings/framework/graphql.md"),
        ),
        Framework::Warp => (
            "framework/warp",
            include_str!("../prompts/briefings/framework/warp.md"),
        ),
        Framework::Tonic => (
            "framework/tonic",
            include_str!("../prompts/briefings/framework/tonic.md"),
        ),
        Framework::Quarkus => (
            "framework/quarkus",
            include_str!("../prompts/briefings/framework/quarkus.md"),
        ),
        Framework::Micronaut => (
            "framework/micronaut",
            include_str!("../prompts/briefings/framework/micronaut.md"),
        ),
        Framework::Javalin => (
            "framework/javalin",
            include_str!("../prompts/briefings/framework/javalin.md"),
        ),
        Framework::Slim => (
            "framework/slim",
            include_str!("../prompts/briefings/framework/slim.md"),
        ),
        Framework::CodeIgniter => (
            "framework/codeigniter",
            include_str!("../prompts/briefings/framework/codeigniter.md"),
        ),
        Framework::Fiber => (
            "framework/fiber",
            include_str!("../prompts/briefings/framework/fiber.md"),
        ),
        Framework::GorillaMux => (
            "framework/gorilla-mux",
            include_str!("../prompts/briefings/framework/gorilla-mux.md"),
        ),
        Framework::Hummingbird => (
            "framework/hummingbird",
            include_str!("../prompts/briefings/framework/hummingbird.md"),
        ),
        Framework::Servant => (
            "framework/servant",
            include_str!("../prompts/briefings/framework/servant.md"),
        ),
        Framework::Dream => (
            "framework/dream",
            include_str!("../prompts/briefings/framework/dream.md"),
        ),
        Framework::Cowboy => (
            "framework/cowboy",
            include_str!("../prompts/briefings/framework/cowboy.md"),
        ),
        Framework::Starlette => (
            "framework/starlette",
            include_str!("../prompts/briefings/framework/starlette.md"),
        ),
        Framework::Pyramid => (
            "framework/pyramid",
            include_str!("../prompts/briefings/framework/pyramid.md"),
        ),
        Framework::Falcon => (
            "framework/falcon",
            include_str!("../prompts/briefings/framework/falcon.md"),
        ),
        Framework::Bottle => (
            "framework/bottle",
            include_str!("../prompts/briefings/framework/bottle.md"),
        ),
        Framework::Play => (
            "framework/play",
            include_str!("../prompts/briefings/framework/play.md"),
        ),
        Framework::Dropwizard => (
            "framework/dropwizard",
            include_str!("../prompts/briefings/framework/dropwizard.md"),
        ),
        Framework::Helidon => (
            "framework/helidon",
            include_str!("../prompts/briefings/framework/helidon.md"),
        ),
        Framework::Vertx => (
            "framework/vertx",
            include_str!("../prompts/briefings/framework/vertx.md"),
        ),
        Framework::Hapi => (
            "framework/hapi",
            include_str!("../prompts/briefings/framework/hapi.md"),
        ),
        Framework::Adonis => (
            "framework/adonis",
            include_str!("../prompts/briefings/framework/adonis.md"),
        ),
        Framework::Meteor => (
            "framework/meteor",
            include_str!("../prompts/briefings/framework/meteor.md"),
        ),
        Framework::Nuxt => (
            "framework/nuxt",
            include_str!("../prompts/briefings/framework/nuxt.md"),
        ),
        Framework::OpenResty => (
            "framework/openresty",
            include_str!("../prompts/briefings/framework/openresty.md"),
        ),
    }
}
