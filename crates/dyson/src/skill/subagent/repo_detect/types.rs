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
    pub(super) const fn language(self) -> Language {
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
            Self::Actix | Self::Axum | Self::Rocket | Self::Warp | Self::Tonic | Self::Solana => {
                Language::Rust
            }
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
