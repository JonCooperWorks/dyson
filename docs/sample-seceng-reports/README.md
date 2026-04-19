# Sample security_engineer reports

Real outputs from the `expensive_live_security_review` harness driven by the production `security_engineer` prompt + language cheatsheets.  These are not curated — they are the unmodified `.md` files that landed in `test-output/iterN/` during the CVE-repro tuning loop documented in [../security-engineer-subagent.md → Case study: CVE-repro sweep and the scope-delegation rule](../security-engineer-subagent.md#case-study-cve-repro-sweep-and-the-scope-delegation-rule).

Use them to calibrate what "shipping-quality" output looks like, what a near-miss looks like, and where the prompt still has work to do.

## What's here

| File | Target | Verdict | What to notice |
|---|---|---|---|
| [`iter1-log4j-2.14.1-hit.md`](iter1-log4j-2.14.1-hit.md) | Apache Log4j 2.14.1 `net/` | **Hit** | Full Log4Shell chain at `JndiManager.java:172`; inline verbatim `taint_trace` output; exploit payload; remediation snippet.  Reference example of what a complete CRITICAL looks like. |
| [`iter1-pyyaml-5.3-hit.md`](iter1-pyyaml-5.3-hit.md) | PyYAML 5.3 | **Hit** | CVE-2020-1747 `python/object/new:` → FullLoader RCE.  Depth-4 taint trace.  Constructor-registration line cited correctly. |
| [`iter1-nextjs-14.0.0-hit.md`](iter1-nextjs-14.0.0-hit.md) | Next.js 14.0.0 `server/web/` | **Hit** (different CVE) | Found CVE-2025-29927 (middleware subrequest bypass) in the scoped subpath.  Note: this wasn't the CVE the target was originally staged for, but it's a legitimate pre-auth bypass in scope — a useful reminder that a "correct" CVE-repro run is a finding anywhere in scope, not just the specific CVE you had in mind. |
| [`iter1-react-server-dom-webpack-near-miss.md`](iter1-react-server-dom-webpack-near-miss.md) | React 19.2.0 `react-server-dom-webpack/src` | **Near-miss + preamble + tool-mix regression** | The React2Shell (CVE-2025-55182) surface.  Opens with the banned "I have a complete picture now" preamble; correctly identifies `decodeReply` as the wrapper but dismisses it to Checked and Cleared with "outside this review scope"; 0 `taint_trace` calls on a deep-chain deserialization target.  This is the report that motivated the scope-delegation dismissal rule. |
| [`iter2-jackson-databind-hit.md`](iter2-jackson-databind-hit.md) | jackson-databind 2.12.6 | **Hit** (with preamble) | Correct CRITICAL on `StdTypeResolverBuilder.java:141` with full `Class.forName` → gadget-chain attack tree + JSON exploit payload.  The iter2 Java scope-delegation rule activated — filed at the in-scope wrapper, cited `TypeFactory.findClass` as the sink hop.  Opens with "Now I have a comprehensive understanding…" (preamble regressed even though the analytic result was correct). |
| [`iter2-react-server-dom-webpack-still-miss.md`](iter2-react-server-dom-webpack-still-miss.md) | React 19.2.0 `react-server-dom-webpack/src` (retry) | **Still near-miss** | Preamble leak fixed.  Scope-delegation rule didn't fire because it was a sub-bullet inside the wire-format section — the model concluded early that the package is a thin re-exporter and never entered wire-format mode.  Diagnosis drove the iter3 change (promote the rule to a top-level section). |

## How to read a report

The schema is fixed — all reports have the same seven sections in the same order:

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

The two most informative cross-report signals:

1. **Does `Taint Trace:` contain the four structural markers** (`index: language=… files=… defs=… unresolved_callees=…`, `Found N candidate path(s) from X to Y:`, `[byte N-N]` ranges, `[SINK REACHED] — tainted at sink:`)?  Missing any = fabricated (cap at MEDIUM) or not run within budget (disclaimer, keep severity).
2. **Does every sink `attack_surface_analyzer` named appear somewhere** — either as a finding or in Checked and Cleared?  Silent drops are the #1 missed-finding bug.

## What's NOT here

- iter3 reports (post-scope-delegation-promotion) — running while this was written; will land in `test-output/iter3/` and may be promoted into this directory once graded.
- Logs.  The full tool-call transcripts that back each report live alongside them in `test-output/iterN/dyson-live-<target>.log` and are not checked in (gitignored) — they are large, timestamped, and easy to regenerate from a billable run.  Grep them with the rubric one-liner in the main case study.
