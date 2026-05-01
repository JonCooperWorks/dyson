// Expensive live security-review harness.  Drives the real
// `security_engineer` orchestrator against fixed, deliberately-
// vulnerable codebases and writes the LLM's full report to disk.
//
// **This costs real money.**  It spins up the complete subagent stack
// — direct tools + inner planner/researcher/coder/verifier — and makes
// billable LLM calls against the provider in your `dyson.json`.  Unlike
// the `smoke_*` examples (which exercise tool functions in isolation
// and cost nothing to run), a full sweep here can run tens of thousands
// of tokens per target.  There is no free fallback — the example will
// just error if the API rejects the model or runs out of credits.
//
// Each target is shallow-cloned into `$TMPDIR/dyson-smoke-repos/`
// (shared with the other smoke tests).  Reports land in
// `test-output/dyson-security-review-<name>.md` by default, relative
// to CWD (the repo root when invoked via `cargo run`).  Override with
// `--output-dir`.
//
// This is NOT a cargo test.  Run explicitly with:
//     cargo run -p dyson --example expensive_live_security_review \
//         --release -- \
//         --config /path/to/dyson.json \
//         [--model <id>] \
//         (--target <name> | --expensive-scan-all-targets)

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use clap::Parser;
use serde_json::json;

use dyson::config::loader::load_settings;
use dyson::controller::ClientRegistry;
use dyson::sandbox::create_sandbox;
use dyson::skill::create_skills;
use dyson::tool::{Tool, ToolContext};

#[derive(Parser)]
#[command(about = "Run the Dyson security_engineer against fixed vulnerable targets.")]
struct Args {
    /// Path to dyson.json config file.  Providers, API keys, and rate
    /// limits all come from here — the example overrides nothing it
    /// doesn't have to.
    #[arg(long)]
    config: PathBuf,

    /// Optional override for `agent.model`.  By default the example
    /// uses whatever `dyson.json` resolves to (the active provider's
    /// first configured model).  Pass this only when you want to swap
    /// in a different model for a single run without editing the config.
    #[arg(long)]
    model: Option<String>,

    /// Run only one target by short name (e.g. `juice-shop`).
    #[arg(long)]
    target: Option<String>,

    /// Run every entry in `TARGETS`.  The name is deliberately long
    /// because the full sweep is billable — you're shallow-cloning
    /// several vulnerable repos and running a real LLM review against
    /// each.  Mutually exclusive with `--target`.
    #[arg(long)]
    expensive_scan_all_targets: bool,

    /// Optional suffix appended to report filenames
    /// (`<output-dir>/dyson-security-review-<target>[-<suffix>].md`).
    /// Use this to keep multiple runs against the same target from
    /// overwriting each other — particularly when measuring run-to-run
    /// variance.
    #[arg(long)]
    report_suffix: Option<String>,

    /// Directory to write reports into.  Defaults to `test-output` in
    /// CWD (the repo root when invoked via `cargo run`).  Created if
    /// missing.  Kept out of git via `.gitignore`.
    #[arg(long, default_value = "test-output")]
    output_dir: PathBuf,

    /// Override the git ref (tag, branch, or commit SHA) checked out
    /// for the target.  Takes precedence over `Target::git_ref`.  Use
    /// this to review a specific historical version — particularly for
    /// reproducing a published CVE against the exact vulnerable release.
    /// Example: `--target juice-shop --ref v15.0.0`.  Cache directory
    /// includes the ref so different versions of the same target don't
    /// collide.  Use full 40-character SHAs; GitHub rejects short SHAs
    /// in the upload-pack protocol (`couldn't find remote ref`).
    #[arg(long = "ref")]
    git_ref: Option<String>,

    /// Toggle the security_engineer's language / framework cheatsheet
    /// injection.  Default `on` — the orchestrator detects manifests in
    /// the review root and appends matching sheets (lang/framework)
    /// onto the child agent's system prompt before the first turn.
    /// Pass `off` to disable injection for a run; pairs with
    /// `--report-suffix` so A/B diffs are straightforward.
    ///
    /// Implemented by setting `DYSON_SECURITY_ENGINEER_CHEATSHEETS` in
    /// the example process's environment — `OrchestratorTool` checks
    /// that variable at `run()` time.  Env-gating keeps the example
    /// from having to rebuild the OrchestratorConfig, which is shipped
    /// as an `Arc<dyn Tool>` via `create_skills`.
    #[arg(long, value_enum, default_value_t = CheatsheetMode::On)]
    cheatsheets: CheatsheetMode,

    /// Pass the target's `description` string (which for CVE-repro
    /// targets names the specific CVE and sometimes the vulnerable API)
    /// into the orchestrator's `context` input.  Default `off` — the
    /// context is empty, and the only thing the agent knows about its
    /// target is the scoped `path`.  Off is the right default for
    /// measuring whether the agent can INDEPENDENTLY rediscover a
    /// published CVE; flipping it on is useful when debugging a failing
    /// run against a known bug and you want the agent to start from the
    /// hint ("Target: log4j 2.14.1 — CVE-2021-44228 via JndiManager.lookup")
    /// rather than from scratch.
    #[arg(long, value_enum, default_value_t = HintsMode::Off)]
    hints: HintsMode,
}

#[derive(Clone, Copy, Debug, clap::ValueEnum)]
enum CheatsheetMode {
    On,
    Off,
}

#[derive(Clone, Copy, Debug, clap::ValueEnum, PartialEq, Eq)]
enum HintsMode {
    On,
    Off,
}

struct Target {
    /// Short name used for `--target` and the report filename.
    name: &'static str,
    /// `org/repo` on github.com.
    slug: &'static str,
    /// Subpath inside the repo to scope the review.  Keeps the attack
    /// surface small enough that the agent can enumerate it within its
    /// iteration budget.
    sub: &'static str,
    /// Spoiler-laden description used when `--hints on`.  For CVE-repro
    /// targets this names the specific CVE and the vulnerable API.
    description: &'static str,
    /// Neutral one-line summary used when `--hints off` (the default).
    /// Library name + what it is.  No version, no CVE ref, no specific
    /// API mention, no "expected finding" prose — the point is to let
    /// the agent know what kind of codebase it's in without telling it
    /// where the bug is.
    summary: &'static str,
    /// Optional git ref (tag, branch, or commit SHA) to check out.
    /// `None` = shallow-clone the default branch head (latest).  `Some`
    /// pins to a specific version — useful for reproducing published
    /// CVEs against the exact vulnerable release.  Overridden by the
    /// `--ref` CLI flag when present.
    git_ref: Option<&'static str>,
}

const TARGETS: &[Target] = &[
    Target {
        name: "juice-shop",
        slug: "juice-shop/juice-shop",
        sub: "routes",
        description: "OWASP Juice Shop - deliberately vulnerable Node/Express app",
        summary: "Node/Express web application.",
        git_ref: None,
    },
    Target {
        name: "nodegoat",
        slug: "OWASP/NodeGoat",
        sub: "app",
        description: "OWASP NodeGoat - deliberately vulnerable Node/Express app for OWASP Top 10",
        summary: "Node/Express web application.",
        git_ref: None,
    },
    Target {
        name: "railsgoat",
        slug: "OWASP/railsgoat",
        sub: "app",
        description: "OWASP RailsGoat - deliberately vulnerable Ruby on Rails app",
        summary: "Ruby on Rails web application.",
        git_ref: None,
    },
    Target {
        name: "dyson",
        slug: "joncooperworks/dyson",
        sub: "",
        description: "Rust based agent - review the app for AI and rust vulnerabilities",
        summary: "Rust AI-agent framework.",
        git_ref: None,
    },
    Target {
        name: "pygoat",
        slug: "adeyosemanputra/pygoat",
        sub: "introduction",
        description: "PyGoat - deliberately vulnerable Django app teaching OWASP Top 10 \
                      (CSRF, XSS, SQLi, broken auth, deserialization, SSRF)",
        summary: "Python/Django web application.",
        git_ref: None,
    },
    // --- CVE-reproduction targets -----------------------------------------
    //
    // Real-world OSS pinned to versions with documented published CVEs.
    // The goal here is NOT to review teaching code — it's to measure
    // whether the security_engineer can INDEPENDENTLY rediscover a
    // known CVE path, given only the vulnerable source.  Successful
    // runs find the bug without being told where to look; grading is
    // against the CVE advisory.
    //
    // react-server-19.2.0 (above) was the first of this kind (ReactFlight
    // prototype-walk).  These add variety: JVM deserialization, JS
    // prototype pollution, JS template RCE, Python YAML RCE.
    Target {
        name: "log4j-2.14.1",
        slug: "apache/logging-log4j2",
        sub: "log4j-core/src/main/java/org/apache/logging/log4j/core/net",
        description: "Apache Log4j 2.14.1 - CVE-2021-44228 (Log4Shell).  `JndiManager.lookup` \
                      fetches attacker-supplied URLs via JNDI when a log message contains a \
                      `${jndi:...}` lookup.  Expected finding: JNDI `lookup(name)` on \
                      attacker-controlled `name` → LDAP/RMI deserialization → class \
                      loading → RCE.  `max_depth=32` on taint_trace recommended; the JNDI \
                      chain spans several indirection layers.",
        summary: "Apache Log4j — Java logging library.",
        git_ref: Some("rel/2.14.1"),
    },
    Target {
        name: "spring-beans-5.3.17",
        slug: "spring-projects/spring-framework",
        sub: "spring-beans/src/main/java/org/springframework/beans",
        description: "Spring Framework 5.3.17 - CVE-2022-22965 (Spring4Shell).  JavaBean \
                      property binding walks `class.module.classLoader` on JDK 9+, reaching \
                      Tomcat's `WebappClassLoader` and writing arbitrary JSP via access-log \
                      properties.  Expected finding: `CachedIntrospectionResults` missing \
                      an allowlist for introspected properties; the bug IS the absence of \
                      the filter that shipped in 5.3.18.",
        summary: "Spring Framework beans module — Java IoC container / bean binding.",
        git_ref: Some("v5.3.17"),
    },
    Target {
        name: "jackson-databind-2.12.6",
        slug: "FasterXML/jackson-databind",
        sub: "src/main/java/com/fasterxml/jackson/databind/deser",
        description: "jackson-databind 2.12.6 - polymorphic-deserialization CVE class \
                      (CVE-2022-42003 etc.).  When `enableDefaultTyping()` or \
                      `@JsonTypeInfo(use = CLASS)` is enabled, the deserializer instantiates \
                      classes named by attacker input, reaching gadget chains (JNDI \
                      managers, template engines, etc.) → RCE.  Expected finding: \
                      `BeanDeserializerFactory` / `StdDeserializer` path that resolves \
                      class names from wire format without an allowlist.",
        summary: "jackson-databind — Java JSON serialisation library.",
        git_ref: Some("jackson-databind-2.12.6"),
    },
    Target {
        name: "lodash-4.17.11",
        slug: "lodash/lodash",
        sub: "",
        description: "lodash 4.17.11 - CVE-2019-10744 (prototype pollution).  \
                      `_.defaultsDeep(target, source)` walks `source`'s keys into `target` \
                      without filtering `constructor` / `__proto__` / `prototype`.  Pollution \
                      of `Object.prototype` propagates to unrelated objects across the \
                      process.  Expected finding: the `defaultsDeep` / `merge` / `set` \
                      property-walk lacks the reflection-name blocklist.",
        summary: "lodash — JavaScript utility library.",
        git_ref: Some("4.17.11"),
    },
    Target {
        name: "ejs-3.1.6",
        slug: "mde/ejs",
        sub: "lib",
        description: "EJS 3.1.6 - CVE-2022-29078 (server-side template injection → RCE).  \
                      `ejs.compile(template, options)` interpolates `options.outputFunctionName` \
                      into the generated function source without escaping.  An attacker who \
                      controls that option writes arbitrary JavaScript executed at render \
                      time.  Expected finding: the option-to-source concatenation in \
                      `lib/ejs.js`; the prompt's JS cheatsheet covers `new Function`-family \
                      RCE primitives which this maps onto.",
        summary: "EJS — embedded JavaScript template engine.",
        git_ref: Some("v3.1.6"),
    },
    Target {
        name: "pyyaml-5.3",
        slug: "yaml/pyyaml",
        sub: "lib3/yaml",
        description: "PyYAML 5.3 - CVE-2020-1747 (FullLoader RCE).  `yaml.FullLoader` was \
                      billed as safe but accepted `python/object/new:SUBCLASS` tags that \
                      instantiate arbitrary Python classes via their `__init__`, reaching \
                      `subprocess.Popen` gadgets etc.  Expected finding: `FullLoader`'s \
                      tag-to-constructor map includes unsafe constructors that the \
                      advisory-fixed `SafeLoader` omits.",
        summary: "PyYAML — Python YAML parser / serialiser.",
        git_ref: Some("5.3"),
    },
    Target {
        name: "nextjs-14.0.0",
        slug: "vercel/next.js",
        sub: "packages/next/src/server/web",
        description: "Next.js 14.0.0 - CVE-2025-29927 (middleware authorization bypass).  \
                      The `x-middleware-subrequest` header tells Next's runtime to skip \
                      registered middleware for an incoming request — a legitimate internal \
                      signal, but trusted from the client side.  An attacker who sets the \
                      header bypasses every auth / rate-limit / role check implemented as a \
                      middleware.  Expected finding: the request-handling path that reads \
                      `x-middleware-subrequest` from an external request and short-circuits \
                      the middleware pipeline without origin verification.",
        summary: "Next.js — React web framework (server runtime).",
        git_ref: Some("v14.0.0"),
    },
    Target {
        name: "rails-6.0.4.7",
        slug: "rails/rails",
        sub: "activesupport/lib/active_support",
        description: "Rails 6.0.4.7 - CVE-2022-32224 (Marshal RCE via YAML encoded columns).  \
                      ActiveRecord serialized-attribute columns defaulted to YAML encoding; \
                      when the DB row was loaded, `YAML.safe_load` in older Psych still \
                      accepted `!ruby/object:` tags that instantiated arbitrary Ruby \
                      objects, reaching gadget chains.  Expected finding: the \
                      serialization layer in ActiveSupport / ActiveRecord that passes \
                      untrusted bytes through an unsafe YAML loader.",
        summary: "Ruby on Rails ActiveSupport — Ruby web framework core utilities.",
        git_ref: Some("v6.0.4.7"),
    },
    Target {
        name: "django-3.2.14",
        slug: "django/django",
        sub: "django/db/models/functions",
        description: "Django 3.2.14 - CVE-2022-34265 (SQL injection via Trunc / Extract).  \
                      `Trunc(..., kind=user_input)` and `Extract(..., lookup_name=user_input)` \
                      let an attacker-controlled string reach a raw SQL fragment that \
                      builds the truncation / extraction expression.  Expected finding: \
                      the lookup-name handling in `functions/datetime.py` (or similar) \
                      concatenates user-derived `kind` into SQL without validating against \
                      an allowlist.",
        summary: "Django ORM functions — Python web framework query builder.",
        git_ref: Some("3.2.14"),
    },
    Target {
        name: "commons-text-1.9",
        slug: "apache/commons-text",
        sub: "src/main/java/org/apache/commons/text",
        description: "Apache Commons Text 1.9 - CVE-2022-42889 (Text4Shell).  \
                      `StringSubstitutor.createInterpolator()` enables the `script:`, \
                      `dns:`, and `url:` lookups by default.  An attacker who reaches a \
                      substitution call with a string like `${script:javascript:...}` \
                      executes arbitrary JS (via Nashorn) — full RCE.  Expected finding: \
                      the default interpolator configuration that registers active \
                      (side-effectful) lookups by default rather than opt-in.",
        summary: "Apache Commons Text — Java string manipulation library.",
        git_ref: Some("rel/commons-text-1.9"),
    },
    Target {
        name: "minimist-1.2.5",
        slug: "minimistjs/minimist",
        sub: "",
        description: "minimist 1.2.5 - CVE-2021-44906 (prototype pollution via CLI args).  \
                      The argument-path walker accepts `--__proto__.polluted=yes` style \
                      flags and writes through to `Object.prototype` because the walk \
                      does not filter reflection-relevant segment names.  Expected \
                      finding: the nested-object walk in `index.js` that lacks a \
                      `constructor`/`__proto__`/`prototype` blocklist before descent.",
        summary: "minimist — Node.js CLI argument parser.",
        git_ref: Some("v1.2.5"),
    },
    Target {
        name: "urllib3-1.26.14",
        slug: "urllib3/urllib3",
        sub: "src/urllib3",
        description: "urllib3 1.26.14 - CVE-2023-43804 (cookie leakage across cross-origin \
                      redirects).  When the client followed a redirect to a different \
                      host, the `Cookie` header set on the initial request was not \
                      stripped — leaking session cookies to the redirect target.  \
                      Expected finding: the redirect-follow path in \
                      `connectionpool.py` / `connection.py` that preserves the Cookie \
                      header across a host change without an allowlist check.",
        summary: "urllib3 — Python HTTP client library.",
        git_ref: Some("1.26.14"),
    },
    // --- Real OSS web applications (framework-based) ---------------------
    //
    // Full-stack apps with published CVEs.  Different from the
    // library-level CVE-repro targets above: these exercise framework-
    // integration code (routes, controllers, sanitizers, file upload /
    // download pipelines) and test whether the agent can find a bug
    // embedded in a much larger codebase.  Scoped subpaths keep the
    // review tractable within budget.
    Target {
        name: "ghost-5.59.0",
        slug: "TryGhost/Ghost",
        sub: "ghost/core/core/server/api",
        description: "Ghost 5.59.0 - CVE-2023-40028 (arbitrary file read via symlinked \
                      files in import).  The content-files import endpoint extracts an \
                      archive and reads uploaded files without checking for symlinks, \
                      letting an authenticated attacker read arbitrary host files by \
                      including a symlink in the import archive.  Expected finding: \
                      the import / file-read handler that does not `lstat` / \
                      canonicalize archive entries before reading.",
        summary: "Ghost — Node.js / Express headless CMS.",
        git_ref: Some("v5.59.0"),
    },
    Target {
        name: "mastodon-4.0.2",
        slug: "mastodon/mastodon",
        sub: "app/lib",
        description: "Mastodon 4.0.2 - CVE-2023-36462 (HTML injection in toot \
                      rendering).  The status formatter / sanitizer whitelist lets \
                      attacker-controlled content through — crafted toots can inject \
                      interactive HTML / script-equivalent content rendered on other \
                      users' timelines.  Expected finding: the formatter / sanitizer \
                      tag-attribute allowlist that is too permissive for its context.",
        summary: "Mastodon — Ruby on Rails federated social network.",
        git_ref: Some("v4.0.2"),
    },
    Target {
        name: "gitea-1.17.3",
        slug: "go-gitea/gitea",
        sub: "modules/markup",
        description: "Gitea 1.17.3 - CVE-2022-42968 (SSRF via SVG image rendering in \
                      markup).  The markup renderer fetches SVG resources from \
                      attacker-supplied URLs when rendering Markdown, without \
                      validating the destination — attacker can pivot to internal \
                      services.  Expected finding: the image-proxy / SVG-fetch path \
                      that pulls a user-supplied URL without host allowlist.",
        summary: "Gitea — Go self-hosted Git service.",
        git_ref: Some("v1.17.3"),
    },
    Target {
        name: "strapi-4.4.5",
        slug: "strapi/strapi",
        sub: "packages/core/admin/server",
        description: "Strapi 4.4.5 - CVE-2023-22894 (SSRF in webhook endpoint).  The \
                      admin-panel webhook preview endpoint fetches attacker-supplied \
                      URLs without host-allowlist or metadata-endpoint blocking, \
                      letting an authenticated admin pivot to cloud IMDS / internal \
                      services.  Expected finding: the webhook fetch handler that \
                      calls out to a URL from request body without restriction.",
        summary: "Strapi — Node.js / Koa headless CMS.",
        git_ref: Some("v4.4.5"),
    },
    // --- Concern-scoped targets: sub path points at one subsystem -------
    //
    // Hypothesis from the iter5-8 sweep: the agent finds more when the
    // scope is narrowed to the subsystem where the bug actually lives
    // (AJP protocol handler, path matcher, session store), rather than
    // a broad package root.  These targets test that by pinning `sub`
    // at the concern, not the project root.
    Target {
        name: "tomcat-9.0.30",
        slug: "apache/tomcat",
        sub: "java/org/apache/coyote/ajp",
        description: "Apache Tomcat 9.0.30 - CVE-2020-1938 (Ghostcat).  The AJP \
                      connector processes attacker-supplied request attributes that \
                      allow file inclusion from the webapp root — any file readable \
                      by the Tomcat process can be read / included as a JSP.  \
                      Expected finding: the AJP handler does not restrict attacker-\
                      controlled `javax.servlet.include.*` request attributes, \
                      letting any absolute path be served as a JSP file.",
        summary: "Apache Tomcat AJP connector — Java servlet container protocol handler.",
        git_ref: Some("9.0.30"),
    },
    Target {
        name: "spring-security-5.6.2",
        slug: "spring-projects/spring-security",
        sub: "web/src/main/java/org/springframework/security/web/util/matcher",
        description: "Spring Security 5.6.2 - CVE-2022-22978 (regex authorization \
                      bypass via RegexRequestMatcher).  `RegexRequestMatcher` \
                      intended for exact path matching treated regex metachars \
                      including `.` as literal matches.  Expected finding: the \
                      request-matcher class that compiles a user-supplied-looking \
                      pattern without anchor enforcement or partial-match \
                      disambiguation.",
        summary: "Spring Security web util matcher — Java auth filter path-matching helpers.",
        git_ref: Some("5.6.2"),
    },
    Target {
        name: "node-forge-1.2.1",
        slug: "digitalbazaar/forge",
        sub: "lib",
        description: "node-forge 1.2.1 - CVE-2022-24771 (RSA-PKCS1v1.5 signature \
                      verification allows low-level digest substitution).  The ASN.1 \
                      parse of the DigestInfo payload accepts unexpected algorithm \
                      parameters and digest values, enabling forged signatures.  \
                      Expected finding: the signature verify path that parses \
                      DigestInfo and does not strictly require the expected OID / \
                      NULL params / digest-length match.",
        summary: "node-forge — Node.js TLS / crypto primitives library.",
        git_ref: Some("v1.2.1"),
    },
    Target {
        name: "airflow-2.4.0",
        slug: "apache/airflow",
        sub: "airflow/www",
        description: "Apache Airflow 2.4.0 - CVE-2022-27949 (stored XSS in task \
                      instance detail view).  Attacker-controlled fields rendered \
                      by the Flask webapp without escaping — any user with DAG-\
                      authoring privileges can store XSS that executes for other \
                      users.  Expected finding: a Jinja template or view handler \
                      that interpolates user-stored strings without `|e` or \
                      equivalent escape.",
        summary: "Apache Airflow webserver — Python / Flask workflow orchestrator UI.",
        git_ref: Some("2.4.0"),
    },
    Target {
        name: "grafana-9.3.6",
        slug: "grafana/grafana",
        sub: "pkg/services/dashboards",
        description: "Grafana 9.3.6 - CVE-2023-0594 (stored XSS in dashboard panel \
                      metadata).  A panel's title / description fields accept raw \
                      HTML that is rendered back into the admin console without \
                      sanitization.  Expected finding: the dashboard-save handler \
                      that stores user-supplied string fields verbatim, paired \
                      with the render path that does not encode them.",
        summary: "Grafana dashboards service — Go observability platform backend.",
        git_ref: Some("v9.3.6"),
    },
    Target {
        name: "keycloak-22.0.0",
        slug: "keycloak/keycloak",
        sub: "services/src/main/java/org/keycloak/services/resources/admin",
        description: "Keycloak 22.0.0 - CVE-2023-6134 (reflected XSS in admin \
                      console via SAML login URL parameter).  A SAML flow URL \
                      parameter echoed in an error-page response without encoding \
                      lets an attacker deliver XSS against an authenticated admin.  \
                      Expected finding: the admin-resource endpoint / error handler \
                      that interpolates a request-derived string into an HTML \
                      response body or redirect.",
        summary: "Keycloak admin services — Java / Quarkus identity & access management.",
        git_ref: Some("22.0.0"),
    },
    Target {
        name: "bookstack-23.10",
        slug: "BookStackApp/BookStack",
        sub: "app",
        description: "BookStack v23.10 - CVE-2023-44399 (stored XSS via template \
                      comments).  An authenticated editor user can inject script \
                      content into a template that renders unescaped when another \
                      user views a page referencing the template.  Expected \
                      finding: the template-render / comment-handling path in \
                      the Laravel controllers that does not escape comment \
                      content before interpolating into HTML.",
        summary: "BookStack — PHP / Laravel self-hosted knowledge-base application.",
        git_ref: Some("v23.10"),
    },
    Target {
        name: "meilisearch-1.4.0",
        slug: "meilisearch/meilisearch",
        sub: "meilisearch",
        description: "Meilisearch v1.4.0 - CVE-2023-47626 (missing API-key validation \
                      on administrative routes).  Several administrative endpoints \
                      fail to validate the master API key, letting an unauthenticated \
                      attacker invoke index-manipulation or config-read operations \
                      over the network.  Expected finding: the actix-web route / \
                      middleware wiring that registers an admin-capable handler \
                      without the API-key guard.",
        summary: "Meilisearch — Rust / actix-web search engine HTTP server.",
        git_ref: Some("v1.4.0"),
    },
    Target {
        name: "rocketchat-6.0.0",
        slug: "RocketChat/Rocket.Chat",
        sub: "apps/meteor/server/methods",
        description: "Rocket.Chat 6.0.0 - CVE-2023-28359 (sensitive data leak via \
                      subscription record exposure).  Meteor DDP subscription \
                      handlers return records containing fields the subscriber \
                      should not see (password-hash metadata, internal user \
                      attributes) because the publish function does not project \
                      / filter fields by role.  Expected finding: a Meteor method \
                      / publish handler that returns a collection query result \
                      without trimming sensitive fields based on the caller's \
                      permission level.",
        summary: "Rocket.Chat — Node.js / Meteor team-chat platform.",
        git_ref: Some("6.0.0"),
    },
    Target {
        name: "filebrowser-2.23.0",
        slug: "filebrowser/filebrowser",
        sub: "http",
        description: "File Browser v2.23.0 - path-traversal class (CVE-2021-46102 \
                      and similar).  The download / archive / zip endpoints \
                      accept user-supplied paths that, without canonicalisation \
                      + prefix check, traverse outside the configured root \
                      directory.  Expected finding: a Gin handler that joins \
                      a request-supplied path to the configured root without \
                      `filepath.Clean` + `strings.HasPrefix(realRoot)` guard.",
        summary: "File Browser — Go / Gin self-hosted file-management web UI.",
        git_ref: Some("v2.23.0"),
    },
    Target {
        name: "plausible-2.0.0",
        slug: "plausible/analytics",
        sub: "lib/plausible_web",
        description: "Plausible Analytics v2.0.0 - web-application review scoped to \
                      the Phoenix controllers + views layer (`PlausibleWeb`).  No \
                      single published CVE pinned — the target exercises the \
                      Phoenix cheatsheet against a real production Elixir app.  \
                      Expected-finding class: controller authorization gaps, \
                      session-token handling, untemplated user input reaching \
                      `raw/.html.heex` rendering, unsafe `String.to_atom` / \
                      `:erlang.binary_to_term` on user input.",
        summary: "Plausible Analytics — Elixir / Phoenix privacy-focused web analytics.",
        git_ref: Some("v2.0.0"),
    },
    // --- Novel-target runs (latest releases, no specific CVE staged) -----
    //
    // Pointed at current stable OSS releases, not pinned vulnerable
    // versions.  The goal is to see whether dyson finds genuine issues
    // against code that has been through the project's normal review
    // cycle.  `description` carries no CVE spoiler; `--hints off` is
    // the expected mode.
    Target {
        name: "appwrite-1.9.0",
        slug: "appwrite/appwrite",
        sub: "app/controllers/api",
        description: "Appwrite 1.9.0 - latest stable release, PHP Backend-as-a-Service.  \
                      Review scoped to the API controllers (`account.php`, `users.php`, \
                      `teams.php`, `projects.php`, etc.) where authentication, session \
                      management, team invites, and user-facing CRUD live.  No specific \
                      CVE pinned — this is a novel-target run against code that has \
                      been through the project's normal review cycle.  Prior CVEs in \
                      this codebase (CVE-2023-27159, CVE-2024-55875) suggest real \
                      surface in account / session handling.",
        summary: "Appwrite — PHP Backend-as-a-Service platform.",
        git_ref: Some("1.9.0"),
    },
    Target {
        name: "outline-1.6.1",
        slug: "outline/outline",
        sub: "server/routes/api",
        description: "Outline 1.6.1 - latest stable release, TypeScript / Koa team \
                      knowledge-base application.  Review scoped to the API route \
                      handlers where authentication, document access control, \
                      attachments, OAuth, and collaboration endpoints live.  No \
                      specific CVE pinned — novel-target run.  Outline has real-time \
                      collaboration + OIDC SSO + file upload surface that is less \
                      reviewed than Notion alternatives.",
        summary: "Outline — TypeScript / Koa self-hosted team knowledge base.",
        git_ref: Some("v1.6.1"),
    },
    // --- Deliberately-vulnerable teaching targets -------------------------
    //
    // Clear, well-documented intended vulnerabilities (no pinned CVE
    // numbers, but the project IS the ground truth — every exploit
    // path is documented in the project's own lesson materials).
    // Distinct from the CVE-repro targets above which test the agent's
    // ability to rediscover a published bug.  These test end-to-end
    // find-something-of-quality behavior on framework-specific surface.
    Target {
        name: "dvga",
        slug: "dolevf/Damn-Vulnerable-GraphQL-Application",
        sub: "core",
        description: "Damn Vulnerable GraphQL Application - Python Flask + Graphene. \
                      Exercises framework/graphql sheet: introspection abuse, batching DoS, \
                      alias-based auth bypass, SQLi / command injection in resolvers, \
                      field-level authorization gaps.  Committed passwords + hardcoded secrets.",
        summary: "Python/Flask + Graphene GraphQL API.",
        git_ref: None,
    },
    Target {
        name: "webgoat-sqli",
        slug: "WebGoat/WebGoat",
        sub: "src/main/java/org/owasp/webgoat/lessons/sqlinjection",
        description: "OWASP WebGoat - SQL injection lessons (Java Spring).  \
                      Multiple deliberately-vulnerable JDBC + JPA patterns across \
                      string-concat Statement / PreparedStatement misuse / JPQL injection.  \
                      Exercises lang/java + framework/spring.",
        summary: "Java Spring web application.",
        git_ref: None,
    },
    Target {
        name: "crapi-workshop",
        slug: "OWASP/crAPI",
        sub: "services/workshop",
        description: "OWASP crAPI - completely ridiculous API, workshop microservice \
                      (Python/Django).  BOLA / IDOR, mass-assignment, SSRF via attacker URL \
                      fetches, JWT weaknesses.  Smaller scope than pygoat but tests the same \
                      lang/python + framework/django pair on code that looks closer to \
                      production than a teaching toy.",
        summary: "Python/Django API microservice.",
        git_ref: None,
    },
    // --- Pinned-version targets for CVE-reproduction runs -----------------
    //
    // These entries pin a specific release so the reviewer can be
    // compared against published advisories.  Keep the sub scoped tight —
    // React's monorepo is too large to review wholesale in the 20-iter
    // budget.  `packages/react-dom/src/server` is the historical CVE
    // hotspot (SSR HTML escape bugs).
    Target {
        name: "react-19.2.0",
        slug: "facebook/react",
        sub: "packages/react-dom/src/server",
        description: "React 19.2.0 - packages/react-dom/src/server (SSR render / HTML escape path) for CVE repro",
        summary: "React DOM server-side rendering.",
        git_ref: Some("v19.2.0"),
    },
    Target {
        name: "react-server-19.2.0",
        slug: "facebook/react",
        sub: "packages/react-server/src",
        description: "React 19.2.0 - packages/react-server/src (Fizz streaming SSR + RSC protocol core - HTML escape logic lives here)",
        summary: "React server package (SSR / RSC runtime).",
        git_ref: Some("v19.2.0"),
    },
    Target {
        name: "react-server-dom-webpack-19.2.0",
        slug: "facebook/react",
        sub: "packages/react-server-dom-webpack/src",
        description: "React 19.2.0 - packages/react-server-dom-webpack/src (RSC + Server Actions over Webpack - new attack surface)",
        summary: "React server-components Webpack adapter.",
        git_ref: Some("v19.2.0"),
    },
    // Solana / Anchor — sealevel-attacks is the canonical teaching corpus
    // for Solana vulnerability classes (one program per class under
    // `programs/`, each in `insecure`/`recommended` pairs).  Grading
    // signal: the reviewer should flag every `insecure` program's
    // planted bug (missing signer check, missing owner check, PDA bump
    // canonicalization, account-type confusion, etc.) while leaving the
    // `recommended` programs in `Checked and Cleared`.  Stress-tests
    // whether the Solana cheatsheet actually fires the constraint-audit
    // posture the sheet prescribes.
    Target {
        name: "sealevel-attacks",
        slug: "coral-xyz/sealevel-attacks",
        sub: "programs",
        description: "sealevel-attacks - deliberately vulnerable Anchor programs, one per attack class (signer-authorization, account-data-matching, owner-checks, type-cosplay, bump-seed-canonicalization, pda-sharing, closing-accounts, duplicate-mutable-accounts, arbitrary-cpi). Every `insecure/` subdir should produce a CRITICAL; every `recommended/` subdir should be clean.",
        summary: "Solana / Anchor on-chain program collection.",
        git_ref: None,
    },
    // Wormhole token bridge (Solana side).  The Feb 2022 $320M exploit
    // was a missing sysvar-account check in `verify_signature` — the
    // program used the deprecated `load_instruction_at` which trusted
    // whichever account was passed as the instructions sysvar instead
    // of validating its address.  Attacker substituted a sysvar they
    // controlled, forged the signature list, and the bridge accepted
    // the forged VAA.  Fix commit `e8b9181` (2022-02-02) switched to
    // `load_instruction_at_checked`; pinning to the parent commit
    // reproduces the vulnerable state.
    Target {
        name: "wormhole-solana",
        slug: "wormhole-foundation/wormhole",
        sub: "solana/bridge/program",
        description: "Wormhole token bridge, Solana side.  Historical CVE (Feb 2022, $320M): missing sysvar-account validation in `verify_signature.rs` — `load_instruction_at` trusted an attacker-supplied account.  Pinned to parent of fix commit e8b9181 so the vulnerable code is present.",
        summary: "Solana cross-chain bridge program.",
        git_ref: Some("79ab522f802ccc5ba34278d3c648fa62e06f4f1c"),
    },
    // Mango Markets v3.  Oct 2022 $114M drain was technically an
    // oracle-manipulation economic attack rather than a missing-check
    // bug — attacker inflated MNGO spot price on thin books, then
    // borrowed against the inflated collateral.  A rigorous review
    // should still flag "protocol trusts on-chain spot price of a
    // thin-book token as collateral oracle" as HIGH/CRITICAL even if
    // no individual line is broken.  Stress-test for whether the
    // reviewer can spot economic bugs in addition to code bugs.
    Target {
        name: "mango-v3",
        slug: "blockworks-foundation/mango-v3",
        sub: "program/src",
        description: "Mango Markets v3.  Historical incident: oracle manipulation via thin-book MNGO spot price inflation, $114M drain (Oct 2022).  Economic attack, not a missing-check bug - a good reviewer still flags the oracle design as CRITICAL.",
        summary: "Solana DeFi margin / perps protocol.",
        git_ref: None,
    },
    // Cashio.  March 2022 $52M infinite-mint hack.  The `print_cash`
    // instruction accepted a `crate_collateral_tokens` PDA whose
    // `saber_swap.arrow` account wasn't validated — attacker crafted
    // a fake arrow account pointing at a worthless token and minted
    // 2B CASH against it.  Classic missing-owner-check / account-
    // spoofing primitive, textbook case for the Solana cheatsheet.
    // Fix commit (2022-03-23, `7df6581`) patched `print_cash`/
    // `burn_cash` to bail — by then the funds were gone.  Pinned to
    // the commit immediately before the patch to keep the vulnerable
    // code in place for review.
    Target {
        name: "cashio",
        slug: "cashioapp/cashio",
        sub: "programs/brrr",
        description: "Cashio stablecoin (Solana).  Historical CVE (Mar 2022, $52M): missing account validation on `saber_swap.arrow` in `print_cash` allowed infinite-mint via fake collateral PDAs.  Pinned to last pre-hack commit (parent of `7df6581` fix).",
        summary: "Solana algorithmic-stablecoin program.",
        git_ref: Some("a51c3c59d544a5763b64abb4a8d82c49b0abd6d0"),
    },
    // Solana Program Library — the production Rust programs for SPL
    // Token, Token-Swap, Token-Lending, Stake Pool, etc.  Widely
    // deployed infrastructure; multiple historical advisories (rounding
    // bugs in swap math, stake-pool fee calculation).  Scoped to
    // token-lending because lending programs have the richest
    // constraint surface (collateral accounts, interest accrual,
    // liquidation paths) and are where most real Solana DeFi bugs live.
    Target {
        name: "spl-token-lending",
        slug: "solana-labs/solana-program-library",
        sub: "token-lending/program/src",
        description: "SPL Token Lending.  Production Solana DeFi infrastructure.  Constraint-audit target: collateral account validation, interest math overflow, liquidation authority checks, oracle integration.  Historical advisories around rounding and fee math.",
        summary: "Solana reference lending program.",
        git_ref: None,
    },
    // Kamino lending — live $1.5M Immunefi bug-bounty target as of
    // 2026-04, the largest in Solana DeFi.  Anchor-based lending
    // program with collateral accounts, oracle integration, vault
    // PDAs, and CPIs into both klend itself and SPL Token — the full
    // constraint-audit surface that the `solana.md` cheatsheet
    // (incl. the post-Cashio "unanchored validation chain" rule)
    // is built to attack.  Run against HEAD because it's a live
    // target, not a pinned-CVE rediscovery.
    Target {
        name: "klend",
        slug: "Kamino-Finance/klend",
        sub: "programs/klend/src",
        description: "Kamino Lending (klend).  Live Immunefi bounty: up to $1.5M (10% of funds-at-risk, $150k floor) for critical smart-contract bugs.  Anchor-based lending program; constraint-audit target — collateral account validation, oracle CPIs, vault PDAs, liquidation authority checks.  Run against HEAD (no pinned CVE).",
        summary: "Solana lending and borrowing protocol.",
        git_ref: None,
    },
    // Kamino Vault (kvault) — sister program to klend.  Same $1.5M
    // Immunefi bounty.  Smaller code base (~5.8k SLOC vs klend's 26k);
    // its primary risk surface is the *cross-program CPI to klend*
    // — exactly where unanchored validation chains tend to hide.
    // Run against HEAD.
    Target {
        name: "kvault",
        slug: "Kamino-Finance/kvault",
        sub: "programs/kvault/src",
        description: "Kamino Vault (kvault).  Live Immunefi bounty under the same Kamino program as klend.  Anchor vault that earns yield by lending into klend via CPI — primary attack surface is cross-program account validation across the kvault → klend boundary.",
        summary: "Solana yield-bearing vault.",
        git_ref: None,
    },
    // Drift Protocol v2 — perpetuals DEX on Solana.  Live bug bounty
    // (max $500k, 10% of hack value), PoC on a privately deployed
    // mainnet contract required for critical and moderate.  Scoped
    // to `programs/drift/src` — the perps program itself, ~149k
    // SLOC across instructions/, state/, controller/, math/, and
    // validation/.  Excludes the pyth/switchboard/openbook adapter
    // programs in the same repo.
    Target {
        name: "drift-v2",
        slug: "drift-labs/protocol-v2",
        sub: "programs/drift/src",
        description: "Drift Protocol v2 perps DEX (Solana).  Live bug bounty: up to $500k (10% of hack value).  Perpetual futures + spot, multiple liquidity mechanisms (vAMM, JIT auctions, DLOB).  Constraint-audit target — large surface across instructions/, state/, controller/, math/, validation/.  Run against HEAD (no pinned CVE).",
        summary: "Solana perpetuals decentralised exchange.",
        git_ref: None,
    },
    // Pyth Network Solana receiver program — live Immunefi bounty up
    // to $250k.  Consumes cross-chain Pythnet price updates and
    // writes them to on-chain accounts.  Classic Solana constraint-
    // audit surface: sysvar usage for signature verification, account
    // ownership, account-data integrity (Wormhole VAA parsing).  Run
    // against HEAD.
    Target {
        name: "pyth-solana-receiver",
        slug: "pyth-network/pyth-crosschain",
        sub: "target_chains/solana/programs/pyth-solana-receiver/src",
        description: "Pyth Network Solana receiver program.  Live Immunefi bounty up to $250k.  Reads Wormhole VAAs carrying Pythnet price updates and writes them to on-chain accounts.  Attack surface includes signature-verification instruction parsing (sysvar access), VAA integrity, account owner checks, and price-update replay.",
        summary: "Solana oracle price update receiver.",
        git_ref: None,
    },
    // Marinade liquid-staking-program — live Immunefi bounty (amount
    // varies, PoC required).  First-on-mainnet liquid-staking protocol
    // on Solana.  Moderate size (~7.6k SLOC); structural-validation
    // surface includes stake account delegation, unstake queues,
    // liquidity pool add/remove, mSOL mint authority, validator list
    // management.  Run against HEAD.
    Target {
        name: "marinade",
        slug: "marinade-finance/liquid-staking-program",
        sub: "programs/marinade-finance/src",
        description: "Marinade liquid-staking-program.  Live Immunefi bounty (amount varies, PoC required for all severities).  First-on-mainnet Solana liquid-staking protocol.  Constraint-audit surface: stake delegation, unstake queues, liquidity pool add/remove, mSOL mint authority, validator list management.",
        summary: "Solana liquid-staking protocol.",
        git_ref: None,
    },
    // Jito (Re)staking program — live Immunefi bounty up to $250k.
    // The restaking program itself is a node-consensus-network +
    // operator registry (~1.7k SLOC).  Small and focused.  Paired
    // with the vault_program (~3.1k SLOC) which holds the actual
    // staked assets.  The restaking_program is the governance /
    // registration layer; the vault_program is where funds live.
    // Run both against HEAD.
    Target {
        name: "jito-restaking-program",
        slug: "jito-foundation/restaking",
        sub: "restaking_program/src",
        description: "Jito (Re)staking — restaking_program.  Live Immunefi bounty up to $250k.  Node-consensus-network and operator registry layer.  Small focused program (~1.7k SLOC).  Attack surface is access control on NCN / operator registration and slashing authority.",
        summary: "Solana restaking registry program.",
        git_ref: None,
    },
    Target {
        name: "jito-vault-program",
        slug: "jito-foundation/restaking",
        sub: "vault_program/src",
        description: "Jito (Re)staking — vault_program.  Live Immunefi bounty up to $250k.  The vault holds the actual staked assets (stSOL, JitoSOL-VRT, etc.).  Constraint-audit surface: deposit / withdraw invariants, VRT (vault receipt token) mint authority, slashing CPI from restaking_program, delegation to NCNs.",
        summary: "Solana restaking vault program.",
        git_ref: None,
    },
    // Rocket.Chat latest-stable run — pinned version 6.0.0 already
    // hit in iter11.  Running the newest release to look for any
    // method-level authorisation regressions added since, or CVEs
    // since-known but not in 6.0.0.  Scope matches the iter11 run.
    Target {
        name: "rocketchat-8.3.2",
        slug: "RocketChat/Rocket.Chat",
        sub: "apps/meteor/server/methods",
        description: "Rocket.Chat 8.3.2 — latest stable.  Public HackerOne bounty up to $7500.  Scoped to Meteor DDP method handlers.  Paired with iter11's 6.0.0 run; this one exists to look for regressions / newly-introduced methods without auth gates.",
        summary: "Rocket.Chat — Node.js / Meteor team-chat platform.",
        git_ref: Some("8.3.2"),
    },
    Target {
        name: "ghost-6.9.3",
        slug: "TryGhost/Ghost",
        sub: "ghost/core/core/server/api",
        description: "Ghost 6.9.3 — latest stable.  Public HackerOne bounty up to $1500.  Scoped to admin / content API endpoints.  Paired with iter8's 5.59.0 run; this one exists to look for auth / validation regressions, novel endpoints added since 5.59, and filter-parameter SQL injection in new query paths.",
        summary: "Ghost — Node.js content management platform.",
        git_ref: Some("v6.9.3"),
    },
    // Rocket.Chat subsystem-targeted runs against latest 8.3.2.  The
    // strategy: broad-scope iter1 found nothing actionable, but the
    // CVE history clusters in three subsystems.  Pointing Dyson at
    // each one in isolation gives the agent a smaller surface to
    // navigate and more iterations per file.
    //
    // 1. REST API v1 — historical NoSQL-injection class (CVE-2021-22911
    //    `getPasswordPolicy`, `users.list`, `listEmojiCustom` — all in
    //    this directory family).  91 endpoints, every one parses
    //    `query` / `filter` from the request and feeds it to a model
    //    method.  The bug class is "model method takes raw object,
    //    Mongo treats $-keys as operators."  Same class still lives
    //    here unless every endpoint sanitises.
    Target {
        name: "rocketchat-8.3.2-api-v1",
        slug: "RocketChat/Rocket.Chat",
        sub: "apps/meteor/app/api/server/v1",
        description: "Rocket.Chat 8.3.2 REST API v1 endpoints — historical NoSQL-injection cluster (CVE-2021-22911 family).  Public HackerOne bounty up to $7500.  91 endpoint files; the recurring pattern is `query`/`filter`/`fields` parameters from the request body flowing into MongoDB query construction without `$`-key stripping.",
        summary: "Rocket.Chat REST API v1 endpoint handlers.",
        git_ref: Some("8.3.2"),
    },
    // 2. SAML — historical auth bypass class (CVE-2020-29594) plus the
    //    `@xmldom/xmldom@0.8.11` XML-injection CVE (GHSA-wh4c-j3r5-mjhp)
    //    flagged by dependency_review on the iter1 run.  XML signature
    //    parsing + assertion handling + relay-state are all classic
    //    SAML attack surfaces.
    Target {
        name: "rocketchat-8.3.2-saml",
        slug: "RocketChat/Rocket.Chat",
        sub: "apps/meteor/app/meteor-accounts-saml/server",
        description: "Rocket.Chat 8.3.2 SAML server — historical auth-bypass class (CVE-2020-29594) plus reachable @xmldom/xmldom XML-injection (GHSA-wh4c-j3r5-mjhp).  Public HackerOne bounty up to $7500.  Attack surface: signature verification, assertion parsing, RelayState handling, identity-mapping, logout request/response.",
        summary: "Rocket.Chat SAML SSO authentication server.",
        git_ref: Some("8.3.2"),
    },
    // 3. Integrations / webhooks — historical SSRF class (CVE-2024-39713
    //    Twilio webhook).  External services post to these endpoints
    //    with attacker-influenceable URLs / payloads.
    Target {
        name: "rocketchat-8.3.2-integrations",
        slug: "RocketChat/Rocket.Chat",
        sub: "apps/meteor/app/integrations/server",
        description: "Rocket.Chat 8.3.2 integrations / webhooks server — historical SSRF class (CVE-2024-39713 Twilio webhook).  Public HackerOne bounty up to $7500.  Attack surface: outgoing webhook URL handling, incoming-webhook script execution sandbox, header/body forwarding.",
        summary: "Rocket.Chat integrations and webhook handlers.",
        git_ref: Some("8.3.2"),
    },
];

/// Task body.  The target path is no longer interpolated — it's passed
/// to the orchestrator via the `path` input, which scopes the child
/// agent's working directory.  All tool calls (including `bash`) now
/// resolve relative paths against that scope, matching how the
/// `coder` subagent works.
const REVIEW_PROMPT: &str = "\
Perform a security review of this codebase.  Focus on server-side \
vulnerabilities: authentication flaws, authorization bypasses, \
injection (SQL/NoSQL/command/XSS), insecure deserialization, unsafe \
file handling, hardcoded secrets, and insecure defaults.  Apply the \
Finding Gate strictly - only report findings with concrete attack \
paths and real impact.  Output a markdown report with one section \
per finding: severity, location (file:line), attack path, and \
recommended fix.";

fn main() -> std::process::ExitCode {
    let args = Args::parse();
    init_tracing();

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime");

    match rt.block_on(run(args)) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn init_tracing() {
    let filter = std::env::var("RUST_LOG").unwrap_or_else(|_| "dyson=info".into());
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .try_init();
}

async fn run(args: Args) -> Result<(), Box<dyn std::error::Error>> {
    // --- Load + minimally override settings ---------------------------------
    let mut settings = load_settings(Some(&args.config))?;
    if let Some(m) = args.model {
        settings.agent.model = m;
    }

    // Propagate the --cheatsheets flag into the env so the orchestrator
    // picks it up.  Must happen BEFORE any OrchestratorTool runs.
    match args.cheatsheets {
        CheatsheetMode::On => {
            // SAFETY: single-threaded startup; no concurrent env reads.
            // The reqwest/tokio machinery hasn't spun up background
            // threads that read env yet.
            unsafe { std::env::set_var("DYSON_SECURITY_ENGINEER_CHEATSHEETS", "on") };
        }
        CheatsheetMode::Off => {
            unsafe { std::env::set_var("DYSON_SECURITY_ENGINEER_CHEATSHEETS", "off") };
        }
    }
    println!("cheatsheets: {:?}", args.cheatsheets);
    // The example clones read-only source trees into $TMPDIR and the
    // security_engineer only needs read + ast access.  Skip the macOS
    // container sandbox check — if the user wanted it enforced they'd
    // be driving dyson proper, not an example.
    settings.dangerous_no_sandbox = true;

    // --- Build the same machinery `build_agent` uses ------------------------
    let sandbox = create_sandbox(&settings.sandbox, true);
    let registry = ClientRegistry::new(&settings, None);
    let skills = create_skills(&settings, None, Arc::clone(&sandbox), None, &registry).await;

    // security_engineer is an OrchestratorTool registered inside the
    // SubagentSkill — flatten all skills' tools and find it by name.
    let sec_eng = skills
        .iter()
        .flat_map(|s| s.tools().iter().cloned())
        .find(|t| t.name() == "security_engineer")
        .ok_or("security_engineer tool not registered - check dyson.json `skills`")?;

    println!(
        "using provider={:?} model={}",
        settings.agent.provider, settings.agent.model
    );

    // --- Target cache (shared with smoke tests) -----------------------------
    let cache = std::env::temp_dir().join("dyson-smoke-repos");
    std::fs::create_dir_all(&cache)?;

    // Pick the run list.  `--target X` → just X.  `--expensive-scan-all-targets`
    // → everything.  Neither → fail with a hint; we don't want an
    // accidental invocation to silently fan out across billable runs.
    let selected: Vec<&Target> = match (args.target.as_deref(), args.expensive_scan_all_targets) {
        (Some(_), true) => {
            return Err("--target and --expensive-scan-all-targets are mutually exclusive".into());
        }
        (Some(name), false) => {
            let matched: Vec<&Target> = TARGETS.iter().filter(|t| t.name == name).collect();
            if matched.is_empty() {
                let known: Vec<&str> = TARGETS.iter().map(|t| t.name).collect();
                return Err(format!("unknown target {name:?}; known: {known:?}").into());
            }
            matched
        }
        (None, true) => TARGETS.iter().collect(),
        (None, false) => {
            let known: Vec<&str> = TARGETS.iter().map(|t| t.name).collect();
            return Err(format!(
                "specify either --target <name> or --expensive-scan-all-targets. \
                 known targets: {known:?}"
            )
            .into());
        }
    };

    let suffix = args.report_suffix.as_deref();
    let ref_override = args.git_ref.as_deref();
    let output_dir = args.output_dir.clone();
    let hints_on = args.hints == HintsMode::On;
    std::fs::create_dir_all(&output_dir)
        .map_err(|e| format!("create output dir {}: {}", output_dir.display(), e))?;
    for t in selected {
        run_target(
            t,
            &cache,
            &sec_eng,
            suffix,
            ref_override,
            &output_dir,
            hints_on,
        )
        .await?;
    }
    Ok(())
}

async fn run_target(
    t: &Target,
    cache: &Path,
    sec_eng: &Arc<dyn Tool>,
    report_suffix: Option<&str>,
    ref_override: Option<&str>,
    output_dir: &Path,
    hints_on: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    // Resolve the effective ref: CLI flag wins, else baked-in `git_ref`,
    // else None (clone default branch head).
    let effective_ref: Option<&str> = ref_override.or(t.git_ref);

    // Cache directory includes the ref so `juice-shop@v15.0.0` and
    // `juice-shop@HEAD` don't share a checkout.  Slashes and other
    // filesystem-unfriendly chars in refs get replaced.
    let cache_key = match effective_ref {
        Some(r) => format!(
            "{}__{}",
            t.slug.replace('/', "__"),
            sanitize_ref_for_path(r)
        ),
        None => t.slug.replace('/', "__"),
    };
    let repo_dir = cache.join(&cache_key);
    if !repo_dir.exists() {
        match effective_ref {
            Some(r) => println!("-> cloning {} @ {} ...", t.slug, r),
            None => println!("-> cloning {} ...", t.slug),
        }
        shallow_clone(t.slug, &repo_dir, effective_ref)?;
    }
    let review_root = repo_dir.join(t.sub);
    if !review_root.exists() {
        return Err(format!("subpath missing: {}", review_root.display()).into());
    }
    // Canonicalize so the `path` we hand to the orchestrator is clean —
    // no `..` segments, no symlink wobble between parent and child.
    let review_root = review_root.canonicalize()?;

    match effective_ref {
        Some(r) => println!(
            "\n=== {} [{}] @ {} @ {} ===",
            t.slug,
            t.name,
            r,
            review_root.display()
        ),
        None => println!(
            "\n=== {} [{}] @ {} ===",
            t.slug,
            t.name,
            review_root.display()
        ),
    }

    // ToolContext's working_dir is irrelevant now — the orchestrator's
    // `path` input overrides it for the child.
    let mut ctx = ToolContext::from_cwd()?;
    ctx.dangerous_no_sandbox = true;

    // Build the context string.  Gated by `--hints`: the `description`
    // field on CVE-repro targets names the specific CVE and sometimes
    // the vulnerable API, which compromises the "independent
    // rediscovery" framing of the sweep.  Default is `off` — pass only
    // the neutral `summary` (library name + what it is, no version, no
    // CVE ref, no API mention), so the agent knows the kind of codebase
    // without being told where the bug is.  Flip on when debugging a
    // known failing case and you want the agent to start from the hint.
    let context = if hints_on {
        match effective_ref {
            Some(r) => format!(
                "Target: {} (pinned to {}).\nReview scope: `{}` subpath of {} at {}.",
                t.description, r, t.sub, t.slug, r
            ),
            None => format!(
                "Target: {}.\nReview scope: `{}` subpath of {}.",
                t.description, t.sub, t.slug
            ),
        }
    } else {
        format!("Target: {}", t.summary)
    };

    let input = json!({
        "task": REVIEW_PROMPT,
        "context": context,
        "path": review_root.display().to_string(),
    });

    let started = std::time::Instant::now();
    let output = sec_eng.run(&input, &ctx).await?;
    let elapsed = started.elapsed();

    let filename = match report_suffix {
        Some(s) => format!("dyson-security-review-{}-{s}.md", t.name),
        None => format!("dyson-security-review-{}.md", t.name),
    };
    let report_path = output_dir.join(filename);
    std::fs::write(&report_path, &output.content)?;

    println!(
        "  finished in {:.1}s | {} bytes | report -> {}{}",
        elapsed.as_secs_f32(),
        output.content.len(),
        report_path.display(),
        if output.is_error { " [TOOL ERROR]" } else { "" },
    );
    Ok(())
}

fn shallow_clone(slug: &str, dest: &Path, git_ref: Option<&str>) -> Result<(), String> {
    let url = format!("https://github.com/{slug}.git");
    match git_ref {
        None => {
            // Default path: one-shot shallow clone of the default branch.
            let status = Command::new("git")
                .args(["clone", "--depth", "1", "--quiet", &url])
                .arg(dest)
                .status()
                .map_err(|e| format!("spawn git: {e}"))?;
            if !status.success() {
                return Err(format!("git clone {url} exited {status}"));
            }
        }
        Some(r) => {
            // Pinned ref: init + fetch the specific ref + checkout
            // FETCH_HEAD.  Works for tags, branches, and commit SHAs —
            // GitHub allows fetching arbitrary reachable SHAs over the
            // smart HTTP protocol.
            std::fs::create_dir_all(dest).map_err(|e| format!("mkdir {}: {e}", dest.display()))?;
            run_git_in(&["init", "--quiet"], dest)?;
            run_git_in(&["remote", "add", "origin", &url], dest)?;
            run_git_in(&["fetch", "--depth", "1", "--quiet", "origin", r], dest)?;
            run_git_in(&["checkout", "--quiet", "FETCH_HEAD"], dest)?;
        }
    }
    Ok(())
}

fn run_git_in(args: &[&str], cwd: &Path) -> Result<(), String> {
    let status = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .status()
        .map_err(|e| format!("spawn git {args:?}: {e}"))?;
    if !status.success() {
        return Err(format!("git {args:?} in {} exited {status}", cwd.display()));
    }
    Ok(())
}

/// Replace path-unfriendly characters in a git ref so it can safely be
/// a directory-name component.  Slashes become underscores (e.g.
/// `release/v15` → `release_v15`).  Tags and SHAs pass through unchanged.
fn sanitize_ref_for_path(r: &str) -> String {
    r.chars()
        .map(|c| {
            if matches!(c, '/' | '\\' | ':') {
                '_'
            } else {
                c
            }
        })
        .collect()
}
