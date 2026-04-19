# Sample security_engineer reports

Real outputs from the `expensive_live_security_review` harness driven by the production `security_engineer` prompt + language cheatsheets.  Each `.md` here is an unmodified copy of what the agent produced in `test-output/iterN/`.

**Every report was produced with `--hints off`** — the agent was NOT told which CVE lives in the target.  The orchestrator received only a neutral one-line `summary` ("Apache Log4j — Java logging library") and the scoped `path`.  Every finding is an independent rediscovery.

Current stack behind these runs:

- 150-turn iteration budget on `security_engineer` (raised from 40 after iter5/6 budget-outs).
- Class-level cheatsheet rules for Java / JS / Python deser, property-path reflection walk, trust-boundary headers, scope-delegation dismissal.  De-fit pass removed Jackson- and React-specific terminology that was letting the agent pattern-match.
- `SubagentTool` scoped-path propagation fix (previously `dependency_review` fell back to the process cwd on inner-subagent dispatch).

Full case study in [../security-engineer-subagent.md → Case study: CVE-repro sweep and the scope-delegation rule](../security-engineer-subagent.md#case-study-cve-repro-sweep-and-the-scope-delegation-rule).

## At a glance

| Verdict | Count | Targets |
|---|---|---|
| **Hit** (CVE pinned OR multiple real findings) | 10 | log4j, jackson, nextjs, pyyaml, lodash, commons-text, webgoat, ghost, strapi, react-server |
| **Partial hit** (adjacent real finding, not the exact CVE) | 2 | spring-beans (ClassEditor vs Spring4Shell), mastodon (LinkDetails iframe vs formatter) |
| **Near-miss** (right code examined, safe-looking reasoning, no finding) | 1 | django (Trunc/Extract regex rationale) |
| **Miss** | 2 | react-server-dom-webpack (dismissed as delegator), gitea (filed HTML-attr MEDIUM, missed SVG SSRF) |

Stacks exercised: **Java** (Log4j, Spring, jackson-databind, Commons Text, WebGoat), **JavaScript/TypeScript** (lodash, Next.js, React server + adapter, Ghost, Strapi), **Python** (PyYAML, Django), **Ruby** (Mastodon), **Go** (Gitea).

## Detailed index

### Hits — clean rediscoveries

| File | Target | Stack | What it shows |
|---|---|---|---|
| [`iter7-log4j-2.14.1-hit.md`](iter7-log4j-2.14.1-hit.md) | Apache Log4j 2.14.1 `net/` | Java | CVE-2021-44228 Log4Shell at `JndiManager.java:172`.  Full attack tree, exploit payload, remediation.  Reference example of a clean CVE rediscovery. |
| [`iter7-jackson-databind-hit.md`](iter7-jackson-databind-hit.md) | jackson-databind 2.12.6 `deser/` | Java | Polymorphic deserialization RCE class via `SubTypeValidator` incomplete deny-list; plus stack-overflow DoS, hash-collision DoS, `SettableAnyProperty` arbitrary setters.  Six total findings.  Preamble leak (`Now I have enough evidence…`) but findings are real. |
| [`iter7-nextjs-14.0.0-hit.md`](iter7-nextjs-14.0.0-hit.md) | Next.js 14.0.0 `server/web/` | Node/TS | CVE-2025-29927 middleware authorization bypass at `sandbox/sandbox.ts:83`.  Trust-boundary-header cheatsheet rule steered the search; `x-middleware-subrequest` is listed there as an example pattern, so this rediscovery has partial cheatsheet assist. |
| [`iter7-pyyaml-5.3-hit.md`](iter7-pyyaml-5.3-hit.md) | PyYAML 5.3 | Python | Five findings covering the full FullLoader / UnsafeLoader RCE surface: `!!python/object/apply:`, `!!python/object/new:` (CVE-2020-1747), `!!python/object:` state injection, `!!python/name:` function refs, `unsafe_load` alias.  Comprehensive library-level review, not just single-CVE pinning. |
| [`iter7-lodash-4.17.11-hit.md`](iter7-lodash-4.17.11-hit.md) | lodash 4.17.11 | Node | CVE-2019-10744 prototype pollution via `_.set` / `_.update` / `_.zipObjectDeep` at `lodash.js:3987`, plus a second CRITICAL on `_.defaultsDeep` where `constructor` key bypasses the `safeGet` guard.  3 real taint_trace calls, 1 inlined verbatim. |
| [`iter8-commons-text-1.9-hit.md`](iter8-commons-text-1.9-hit.md) | Apache Commons Text 1.9 | Java | CVE-2022-42889 Text4Shell via `StringSubstitutor` default active lookups (`script:`, `dns:`, `url:`).  **Fresh target** — cheatsheets never mentioned Commons Text, `StringSubstitutor`, or Text4Shell when this ran.  Clean class-level rediscovery with the de-fit Java cheatsheet. |
| [`iter8-webgoat-sqli-hit.md`](iter8-webgoat-sqli-hit.md) | OWASP WebGoat SQLi lessons | Java / Spring | SQL injection via ORDER BY clause in server listing endpoint.  Exercises the SQL section of the Java cheatsheet — different context from the serialization-heavy targets above. |
| [`iter8-ghost-5.59.0-hit.md`](iter8-ghost-5.59.0-hit.md) | Ghost 5.59.0 admin API | Node/Express | Did not pin CVE-2023-40028 (symlink file-read) exactly — instead filed four other real bugs: SQL injection via filter param concatenation, no-auth file upload endpoint, no-auth media upload endpoint, open redirect in mail event processing.  Shows the agent finding meaningful issues in a full-stack CMS when scoped to routes. |
| [`iter8-strapi-4.4.5-hit.md`](iter8-strapi-4.4.5-hit.md) | Strapi 4.4.5 admin server | Node/Koa | Five findings including **CVE-2023-22894 SSRF via admin webhook** (CRITICAL), JWT signature algorithm bypass (CRITICAL), password reset token in URL, admin static-files auth gap.  5 real taint_trace calls, 1 inlined.  First Koa target in the suite. |
| [`iter8-react-server-19.2.0-hit.md`](iter8-react-server-19.2.0-hit.md) | React 19.2.0 `react-server/src` | Node/TS | **The React2Shell prototype-walk primitive**, found cleanly when the scope points at the package that actually contains the sink (`react-server`), not the wrapper package.  Lists prototype-walk in `getOutlinedModel`, same primitive in `createModelResolver`, `JSON.parse` on unvalidated `FormData`, `bindArgs` on attacker-controlled args.  Paired with the still-miss below to illustrate the scope-delegation failure mode. |

### Partial hits — adjacent findings, class rule worked

| File | Target | Stack | What it shows |
|---|---|---|---|
| [`iter7-spring-beans-partial-hit.md`](iter7-spring-beans-partial-hit.md) | Spring Framework 5.3.17 `spring-beans/` | Java | Did NOT pin CVE-2022-22965 (Spring4Shell) at `CachedIntrospectionResults.java:289` exactly.  Instead filed HIGH at `propertyeditors/ClassEditor.java:65` — `ClassUtils.resolveClassName` on user-bound string.  Adjacent Spring property-binding reflection sink in the same attack-tree family.  The Java property-path reflection-walk rule is steering toward the right vulnerability class even when it doesn't land the exact CVE. |
| [`iter8-mastodon-4.0.2-partial-hit.md`](iter8-mastodon-4.0.2-partial-hit.md) | Mastodon 4.0.2 `app/lib` | Ruby/Rails | Did NOT pin CVE-2023-36462 (HTML injection in toot formatter) exactly.  Filed five adjacent findings: stored HTML injection via `LinkDetailsExtractor` iframe (same sanitiser-gap class, different subsystem), WebFinger host-meta SSRF, admin-only SQL interpolation, unscoped `send(key)` via ActiveModel, Nokogiri with libxml2 CVEs.  2 real taint_trace calls, 1 inlined.  First Ruby target in the sample set. |
| [`iter9-keycloak-22.0.0-partial-hit.md`](iter9-keycloak-22.0.0-partial-hit.md) | Keycloak 22.0.0 `services/resources/admin` | Java/Quarkus | Did NOT pin CVE-2023-6134 (SAML URL reflected XSS) exactly.  Filed two real admin-auth issues in the scoped subsystem: HIGH missing authorization check on `testSMTPConnection` endpoint (`RealmAdminResource.java:989`), LOW stack trace leak in admin event endpoint.  First Quarkus target in the suite — shows the concern-scoped approach (scope = admin resources directory) surfacing real auth bugs even when it doesn't land the specific CVE. |

### Near-miss — right code examined, no finding

| File | Target | Stack | What it shows |
|---|---|---|---|
| [`iter7-django-near-miss.md`](iter7-django-near-miss.md) | Django 3.2.14 `db/models/functions/` | Python/Django | CVE-2022-34265 (Trunc/Extract SQL injection).  Agent correctly navigated to `datetime.py:44-50` — the exact CVE location — examined the `extract_trunc_lookup_pattern` regex, and concluded the input is validated against an allowlist.  Plausible reasoning, no finding.  Instructional example of the difference between "looks at the right code" and "files the bug". |
| [`iter9-spring-security-5.6.2-near-miss.md`](iter9-spring-security-5.6.2-near-miss.md) | Spring Security 5.6.2 `web/util/matcher` | Java/Spring | CVE-2022-22978 (regex auth bypass via `RegexRequestMatcher`).  Agent navigated to `RegexRequestMatcher.java:101` (the exact sink) and dismissed it: *"If the pattern is developer-controlled (standard usage), this is fine."*  The bug is that anchor-absence lets attacker URLs fake-match a developer pattern — the exact thing that "developer-controlled" doesn't mitigate.  Filed two adjacent real MEDIUM findings instead (SpEL evaluation in `ELRequestMatcher`, log injection).  2 real taint_trace calls, 1 inlined.  Preamble leak. |

### Misses — class rule didn't fire or wrong conclusion

| File | Target | Stack | What it shows |
|---|---|---|---|
| [`iter7-react-server-dom-webpack-still-miss.md`](iter7-react-server-dom-webpack-still-miss.md) | React 19.2.0 `react-server-dom-webpack/src` | Node/TS | CVE-2025-55182 React2Shell.  After 7 attempts across iter1 → iter7, the agent continues to dismiss `decodeReply` as "thin delegation to an external package" and moves it to `Checked and Cleared`.  Scope-delegation rule in the JS cheatsheet either insufficient for this pattern or the model's framework fights it.  Paired with `iter8-react-server-19.2.0-hit.md` — same agent finds the same bug class when scope points at the package that actually contains the sink. |
| [`iter8-gitea-1.17.3-miss.md`](iter8-gitea-1.17.3-miss.md) | Gitea 1.17.3 `modules/markup` | Go | CVE-2022-42968 (SSRF via SVG image proxy).  Filed only MEDIUM about Orgmode HTML-attribute escaping, missed the SSRF.  First Go target in the suite — suggests the Go cheatsheet may need the same trust-boundary / SSRF class pass the JS cheatsheet got. |

## How to read a report

The schema is fixed — every report has the same seven sections in the same order:

```
## CRITICAL       — findings at the highest severity
## HIGH           — same schema, one level down
## MEDIUM         — same
## LOW / INFORMATIONAL
## Checked and Cleared   — one line per sink examined and ruled out
## Dependencies          — integrated dependency_review output
## Remediation Summary   — per-severity fix list referencing findings above
```

A section with no items prints its header followed by `No findings.` on its own line — silent omission is a schema violation.

Every finding has the same seven fields:

```
- **File:** `path:LINE`
- **Evidence:** <the exact source at that line>
- **Attack Tree:** <entry → hop → sink, grounded in real code locations>
- **Taint Trace:** <verbatim tool output, or "not run within budget" disclaimer>
- **Impact:** <concrete outcome, no "may" / "could" / "might">
- **Exploit:** <one payload, required for eval/deser/SSTI/redirect classes>
- **Remediation:** <specific fix with corrected snippet>
```

## Cross-report signals

Two things to grep across this directory when calibrating the cheatsheet / prompt:

1. **`taint_trace` verbatim markers** — real tool output contains `index: language=… files=N defs=N calls=N unresolved_callees=N`, `Found N candidate path(s) from X to Y:`, `[byte A-B]` ranges, and `[SINK REACHED] — tainted at sink:`.  Missing any of those = fabricated (cap at MEDIUM) or "not run within budget" disclaimer (keep severity).
2. **Preamble shape** — no report's first non-whitespace character should be anything but `#`.  Strings like `Now I have enough evidence…`, `Let me compile…`, `Based on my analysis…` at the top are the forbidden preamble class.  Several reports here still open with one (notably iter7 jackson, iter8 commons-text, iter8 webgoat); the findings were real so they shipped, but the preambles are a persistent weaker-model tell worth keeping an eye on.

## Not here

- **Flakes.**  Several iter7 / iter8 runs hit OpenRouter `HTTP error: error decoding response body` mid-stream (the provider returning malformed responses — not a dyson issue): pyyaml (two flakes before the iter7 success), ejs (three consecutive flakes, no successful run yet), jackson iter8 de-fit run (125-byte truncation), urllib3 iter8 (353-byte truncation mid-analysis after correctly identifying the CVE-2023-43804 cookie-on-redirect chain), minimist iter8 (multiple flakes).  Needs rerun when the provider stabilises.
- **Logs.**  Full tool-call transcripts live alongside each report in `test-output/iterN/dyson-live-<target>.log` — gitignored, regenerable from a billable run.
- **Prior iterations.**  iter1 through iter6 samples are superseded by this set and only live in git history.  The current set reflects the state of prompt + cheatsheets + 150-turn budget + dep-review fix + `--hints off` default + de-fit Java cheatsheet + new library / web-app CVE-repro targets.
