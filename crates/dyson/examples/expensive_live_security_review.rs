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
            return Err(
                "--target and --expensive-scan-all-targets are mutually exclusive".into(),
            );
        }
        (Some(name), false) => {
            let matched: Vec<&Target> = TARGETS.iter().filter(|t| t.name == name).collect();
            if matched.is_empty() {
                let known: Vec<&str> = TARGETS.iter().map(|t| t.name).collect();
                return Err(format!(
                    "unknown target {name:?}; known: {known:?}"
                )
                .into());
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
        run_target(t, &cache, &sec_eng, suffix, ref_override, &output_dir, hints_on).await?;
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
            std::fs::create_dir_all(dest)
                .map_err(|e| format!("mkdir {}: {e}", dest.display()))?;
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
        return Err(format!(
            "git {args:?} in {} exited {status}",
            cwd.display()
        ));
    }
    Ok(())
}

/// Replace path-unfriendly characters in a git ref so it can safely be
/// a directory-name component.  Slashes become underscores (e.g.
/// `release/v15` → `release_v15`).  Tags and SHAs pass through unchanged.
fn sanitize_ref_for_path(r: &str) -> String {
    r.chars()
        .map(|c| if matches!(c, '/' | '\\' | ':') { '_' } else { c })
        .collect()
}
