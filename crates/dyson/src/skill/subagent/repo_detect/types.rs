/// Languages for which a security briefing ships.  Covers every tree-sitter
/// grammar dyson's `ast_query` supports, plus PHP + Lua (no in-tree
/// grammar but the briefings still guide `read_file` / `search_files`
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

/// Frameworks for which a security briefing ships.  Detection scopes
/// detected frameworks to the selected languages, and the hunt stage
/// spawns one specialist hunter per detected framework.
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

#[derive(Debug, Default)]
pub struct Detection {
    /// Languages ranked by manifest count (descending).  Ties broken by
    /// the stable enum order to keep output reproducible across runs.
    pub languages: Vec<Language>,
    /// Frameworks detected in any parsed manifest for a selected
    /// language.  Preserved in discovery order.
    pub frameworks: Vec<Framework>,
}
