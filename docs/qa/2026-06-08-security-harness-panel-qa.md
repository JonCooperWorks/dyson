# Security Harness Panel — Live QA Report

**Date:** 2026-06-08
**QA target:** `security_engineer` tool panel rendering in the Dyson agent chat UI
**Test agent:** `sparky` (`dynamic-eel-946-4b26de`) at `https://dynamic-eel-946-4b26de.swarm.myprivate.network/`
**Conversation:** `c-0016` "Security Harness Panel QA"
**Tool call:** `call_a9bc829488fe4d2ebdfd665f` (`sec-1780939724-2`)
**Payload:** single-file intentionally-vulnerable Flask app (`/tmp/vuln_app.py`, 1465 bytes) — planted SQLi, command injection, SSTI, pickle deserialization, path traversal, MD5 weak hash, hardcoded secret, IDOR cookie, `debug=True` Werkzeug RCE.

## Run timing

| Stage      | Duration (approx) |
|------------|-------------------|
| Recon      | ~2 min            |
| Hunt       | ~26 min           |
| Validate → Report | <30 s combined (visible together) |
| **Total**  | **~29 min**       |

Hunt dominated wall-clock — `exploit_builder`, `ast_describe`, `attack_surface_analyzer`, repeated `read_file`/`search_files` iterations. 350+ tool blocks streamed under the panel.

## PASS / FAIL

| Check | Result | Evidence |
|---|---|---|
| Panel appears inline under the assistant turn | ✅ PASS | `security_engineer` block rendered as soon as the tool was called |
| "Harness initializing…" shimmer strip | ✅ PASS | Caught cleanly on cold reload — *"harness initializing — loading checkpoint, choosing first stage…"* with *"(no run id yet)"* and *"Subagent starting…"* |
| StageBar advances live with ▸/✓ glyphs and dashed pending | ✅ PASS | Live transitions ▸ Recon → ✓ Recon + ▸ Hunt → ✓ all 8. JS confirmed `▸ Recon` blue bg, pending stages transparent + grey. |
| Findings counter / severity row populates | ✅ PASS | **55 findings · 18 CRITICAL · 19 HIGH · 15 MEDIUM · 3 LOW** rendered after Report |
| CLASS COVERAGE grid populates | ✅ PASS | **24/24 REPORTED** with per-class counts (`injection_unsafe_exec 5`, `secrets_credentials 5`, `crypto_randomness 2`, `auth_authorization 3`, etc.) |
| Planted vulns flagged | ✅ PASS | All 7 surfaced: SQLi, command injection, SSTI, pickle, path traversal, MD5, `debug=True`/Werkzeug |
| Nested subagent/tool activity visible under panel | ✅ PASS | 350+ tool blocks streamed inline |
| Browser console errors | ✅ PASS | None observed (caveat: console capture starts on first call) |
| **Rehydration after hard refresh** | ⚠️ **PARTIAL FAIL** | See "Rehydration regression" below |

## ⚠️ Rehydration regression

After `Ctrl+Shift+R` on the completed panel:

- ✓ Stage glyphs partially survive: `✓ Recon ✓ Hunt ✓ Validate ✓ Gapfill ✓ Dedupe ✓ Trace ✓ Feedback` — but the last stage comes back as **`▸ Report`** (running), not `✓ Report`.
- ❌ Run-id lost: shows `(no run id yet)` instead of `sec-1780939724-2 [completed]`.
- ❌ Findings counter gone: no "55 findings · 18 CRITICAL …" row.
- ❌ CLASS COVERAGE grid gone: 24-class grid empty.
- ❌ Tool re-enters a "running" state post-refresh (timer counting up from 0:04 → 2:19+), even though the original 28:55 run had completed.

This is a regression against the intent of commit `e17d626` ("bake CheckpointEvent stream into tool content so panel rehydrates after refresh"). The stage transitions appear baked; the terminal `report-done`/`completed` checkpoint (with findings rollup, run-id, class coverage grid) is either not persisted or not deserialized on rehydration. Likely root cause to investigate: the rehydrator replays stage events but the "completed" snapshot containing findings/grid/run-id isn't part of the replay stream — or the stream lacks the final terminal event that flips `▸ Report` → `✓ Report`.

## Side observation (not a panel bug)

First attempt on a small `login + os.system` snippet: sparky chose to answer inline and skipped the harness ("the snippet is right there in the query. Let me just give the findings directly."). Required an explicit re-prompt naming the `security_engineer` tool. On the larger Flask sample, sparky invoked the harness on the first try after writing the file to `/tmp/vuln_app.py`. Worth noting that on trivial inputs the model treats the harness as not worth the latency.

## Reproduction

1. Open sparky's chat at `https://dynamic-eel-946-4b26de.swarm.myprivate.network/`.
2. Start a new conversation.
3. Paste the Flask app at `/tmp/qa_msg.txt` (the QA prompt asks for the full pipeline through Report).
4. Wait for sparky to write the file to disk and invoke `security_engineer`.
5. Watch StageBar advance — confirm ✓/▸ glyphs and dashed pending stages.
6. After completion, confirm 55 findings + CLASS COVERAGE grid render.
7. Hard-refresh (`Ctrl+Shift+R`). Observe partial rehydration.

## Screenshots produced

1. Init / "Subagent starting…" frame
2. Mid-run with ▸ Hunt running, ✓ Recon, dashed pending stages
3. Completed panel — all 8 ✓, `completed` badge, 55 findings + severity row
4. Completed panel scrolled — full CLASS COVERAGE 24/24 grid
5. Post hard-reload — initializing strip caught ("harness initializing — loading checkpoint…")
6. Post hard-reload — partial rehydration (stages back, findings/grid/run-id gone, Report ▸)
