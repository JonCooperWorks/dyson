# Sample security_engineer reports

Real outputs from the `expensive_live_security_review` harness driven by the production `security_engineer` prompt + language cheatsheets.  These are unmodified `.md` files copied from `test-output/iterN/` during the CVE-repro tuning loop documented in [../security-engineer-subagent.md → Case study: CVE-repro sweep and the scope-delegation rule](../security-engineer-subagent.md#case-study-cve-repro-sweep-and-the-scope-delegation-rule) (and the follow-on iter5 → iter8 rounds for hints-off baseline, 150-turn budget, and web-application CVE-repro).

**Important:** every report here was produced with `--hints off` — the agent was NOT told which CVE lives in the target.  The orchestrator received only a neutral one-line `summary` ("Apache Log4j — Java logging library") and the scoped `path`.  Every finding below is an independent rediscovery.

## What's in each

| File | Target | Verdict | Why it's here |
|---|---|---|---|
| [`iter7-log4j-2.14.1-hit.md`](iter7-log4j-2.14.1-hit.md) | Apache Log4j 2.14.1 `net/` | **Hit** | CVE-2021-44228 (Log4Shell) rediscovered independently.  `JndiManager.java:172` `context.lookup(name)` sink, full attack tree through `JndiLookup` / `StrSubstitutor`, exploit payload, remediation.  The reference example of a clean CVE rediscovery. |
| [`iter7-jackson-databind-hit.md`](iter7-jackson-databind-hit.md) | jackson-databind 2.12.6 `deser/` | **Hit** | Polymorphic deserialization RCE class found via the Java cheatsheet's class-resolution-is-the-sink rule.  Six findings including `SubTypeValidator` incomplete deny-list, stack-overflow DoS, hash-collision DoS, `SettableAnyProperty` accepting arbitrary property names.  Has a preamble leak (`Now I have enough evidence…`) but the findings are real. |
| [`iter7-nextjs-14.0.0-hit.md`](iter7-nextjs-14.0.0-hit.md) | Next.js 14.0.0 `server/web/` | **Hit** | CVE-2025-29927 middleware authorization bypass at `sandbox/sandbox.ts:83`.  Trust-boundary-header class tweak in `javascript.md` steered the search; `x-middleware-subrequest` is listed there as an example header pattern, so this rediscovery has partial cheatsheet assist.  Still a real find on non-trivial request-handling code. |
| [`iter7-spring-beans-partial-hit.md`](iter7-spring-beans-partial-hit.md) | Spring Framework 5.3.17 `spring-beans/` | **Partial hit** | Did NOT pin CVE-2022-22965 (Spring4Shell) at `CachedIntrospectionResults.java:289` exactly.  Instead filed HIGH at `propertyeditors/ClassEditor.java:65` — `ClassUtils.resolveClassName` called with user-bound string.  That IS an adjacent Spring property-binding reflection sink in the same attack-tree family.  Shows the Java property-path reflection-walk rule steering toward the right class of vulnerability, even when it doesn't land the exact CVE. |
| [`iter7-django-near-miss.md`](iter7-django-near-miss.md) | Django 3.2.14 `db/models/functions/` | **Near-miss** | CVE-2022-34265 (Trunc/Extract SQL injection).  Agent correctly navigated to `datetime.py:44-50` — the exact CVE location — examined the `extract_trunc_lookup_pattern` regex, and concluded the input is validated against an allowlist.  The class-level rules put it in the right spot; the analysis was plausible even if the specific CVE was present.  Instructional example of the difference between "looks at the right code" and "files the bug". |
| [`iter7-react-server-dom-webpack-still-miss.md`](iter7-react-server-dom-webpack-still-miss.md) | React 19.2.0 `react-server-dom-webpack/src` | **Still miss** | CVE-2025-55182 React2Shell.  After 7 attempts across iter1 → iter7, the agent continues to dismiss `decodeReply` as "thin delegation to an external package" and moves it to `Checked and Cleared`.  The scope-delegation rule in the JS cheatsheet is either insufficient for this pattern or the model's framework fights it.  Paired with `iter8-react-server-19.2.0-hit.md` below which shows the same agent finds the same bug class when the scope points at the package that actually contains the sink, not the wrapper package. |
| [`iter5-pyyaml-5.3-hit.md`](iter5-pyyaml-5.3-hit.md) | PyYAML 5.3 | **Hit** | CVE-2020-1747 `FullLoader` RCE.  `constructor.py:575` `cls(*args, **kwds)` sink.  Notable for the tool mix: 5 real `taint_trace` calls, 2 inlined verbatim into the report (most of the other reports are 0 taint_trace).  Reference for what good taint output looks like. |
| [`iter7-lodash-4.17.11-hit.md`](iter7-lodash-4.17.11-hit.md) | lodash 4.17.11 | **Hit** | CVE-2019-10744 family — prototype pollution via `_.set` / `_.update` / `_.zipObjectDeep` at `lodash.js:3987`, plus a second CRITICAL on `_.defaultsDeep` where `constructor` key bypasses the `safeGet` guard.  3 real taint_trace calls, 1 inlined verbatim.  Shows the JS prototype-walk rule landing on the canonical CVE without hints. |
| [`iter8-commons-text-1.9-hit.md`](iter8-commons-text-1.9-hit.md) | Apache Commons Text 1.9 | **Hit** | CVE-2022-42889 (Text4Shell) RCE via `StringSubstitutor` default active lookups (`script:`, `dns:`, `url:`).  A **fresh** target — the cheatsheets never mentioned Commons Text, `StringSubstitutor`, or Text4Shell when this ran.  Clean class-level rediscovery with the de-fit Java cheatsheet. |
| [`iter8-webgoat-sqli-hit.md`](iter8-webgoat-sqli-hit.md) | OWASP WebGoat SQLi lessons | **Hit** | SQL injection via ORDER BY clause in server listing endpoint.  Different Java context from the serialization-heavy targets above; exercises the SQL section of the Java cheatsheet. |
| [`iter8-ghost-5.59.0-hit.md`](iter8-ghost-5.59.0-hit.md) | Ghost 5.59.0 admin API | **Hit on multiple real bugs** | Did NOT pin CVE-2023-40028 (symlink file-read) exactly — instead found four other real bugs: SQL injection via filter param concatenation, no-auth file upload endpoint, no-auth media upload endpoint, open redirect in mail event processing.  Shows the agent finding meaningful issues in a full-stack Node/Express CMS when scoped to a sub-directory of routes, not just in library internals. |
| [`iter8-react-server-19.2.0-hit.md`](iter8-react-server-19.2.0-hit.md) | React 19.2.0 `react-server/src` | **Hit** | **The React2Shell prototype-walk primitive**, found cleanly when the scope points at the package that actually contains the sink (`react-server`), not the wrapper package (`react-server-dom-webpack` — see the still-miss report above).  Lists multiple findings: prototype-walk in `getOutlinedModel`, same primitive in `createModelResolver`, `JSON.parse` on unvalidated `FormData`, `bindArgs` on attacker-controlled args.  Paired with the still-miss to illustrate the scope-delegation failure mode: the rule works *if* scope points at the right package. |
| [`iter8-gitea-1.17.3-miss.md`](iter8-gitea-1.17.3-miss.md) | Gitea 1.17.3 `modules/markup` | **Miss** | CVE-2022-42968 (SSRF via SVG image proxy).  Agent filed only MEDIUM about Orgmode HTML-attribute escaping, didn't find the SSRF.  First Go target in the suite — suggests the Go cheatsheet may need the same trust-boundary / SSRF class pass the JS cheatsheet got. |

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

- **iter7 pyyaml + ejs**, **iter8 jackson + urllib3 truncations** — OpenRouter returned malformed responses mid-stream on these runs (5+ flakes this session).  Not a dyson issue; the error is visible in the logs as `HTTP error: error decoding response body`.  Runs need to be rerun when provider stabilises.
- **Logs.**  The full tool-call transcripts that back each report live alongside them in `test-output/iterN/dyson-live-<target>.log` and are not checked in (gitignored) — they are large, timestamped, and easy to regenerate from a billable run.  Grep them with the rubric one-liner in the main case study.
- **Iterations 1-6.**  Prior sample sets covered iter1/iter2 and are documented in git history; the current set replaces them with the iter5/iter7/iter8 results which reflect the current state of the prompt, cheatsheets, 150-turn budget, dep-review fix, `--hints off` default, and de-fit Java cheatsheet.
