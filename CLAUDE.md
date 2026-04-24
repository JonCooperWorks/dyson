# CLAUDE.md

Guidance for Claude Code (and any other Claude instance) working on this repo.

Dyson is an AI agent framework in Rust.  Everything interesting happens through traits — `LlmClient`, `Tool`, `Skill`, `Sandbox`, `Controller`, `SecretResolver` — and the agent loop streams tool calls through a sandbox.  Start with [lib.rs](crates/dyson/src/lib.rs) for the module map, [README.md](README.md) for the feature surface, and [docs/architecture-overview.md](docs/architecture-overview.md) for data flow.

## Project priorities (observed, not aspirational)

- **RSS and binary size matter.**  Minimize allocations, avoid shelling out to large dependencies, prefer statically-linked grammars over dynamic loading.  `jemalloc` is the global allocator; the `MALLOC_CONF` note in the README is load-bearing for container deployments.
- **Additions should pay their keep in capabilities AND tokens.**  Tools like `image/pdf-extract` are worth it because they both add a capability *and* reduce what the agent has to read.  Adding a dep that only does one should prompt a conversation first.
- **The sandbox is the security boundary.**  Every tool call passes through `Sandbox::check()` → `Sandbox::after()`.  Never add a bypass.  `--dangerous-no-sandbox` is an explicit opt-in, not a fallback.
- **MCP is just another `Skill`.**  Don't special-case it in the agent loop.
- **Read the git log before suggesting prompt or agent changes.**  Recent commits on `prompts/` and `subagent/` files often rule out what looks like an obvious improvement.

## Testing

Four layers, covered in detail in [docs/testing.md](docs/testing.md):

| Layer | Command | Cost |
|---|---|---|
| Unit tests | `cargo test` | Free |
| Smoke tests (tools vs. real repos) | `cargo run -p dyson --example smoke_*` | Free |
| Integration / regression | `cargo test` (root `tests/`) | Free |
| Frontend (vitest) | `npm test` in `crates/dyson/src/controller/http/web/` | Free |
| Live subagent review (real LLM) | `cargo run -p dyson --example expensive_live_security_review` | Billable |

The web frontend is a Vite + React project under `crates/dyson/src/controller/http/web/`.  `crates/dyson/build.rs` runs `npm run build` as part of `cargo build` (mtime-gated — nothing fires when the frontend is untouched) and `include_bytes!`s the resulting `dist/` into the binary.  Frontend regressions live next to the code in `web/src/__tests__/`; `npm run build` runs vitest first, so a failing JS test fails `cargo build` too.  For active UI work, `npm run dev` from `web/` gives HMR with `/api` proxied to a running dyson on :7878.  Node 20+ is required to build — there is no feature flag to skip the frontend.

Two loops connect them:
- Smoke failures get **minimised to a fixture and promoted** into `tests/ast_taint_patterns.rs` or similar.  Don't patch silently.
- Live-review failures produce either a **code fix + unit test** (e.g. `OrchestratorTool.path`) OR a **prompt tune + rerun** — prompt tunes don't get regression tests because LLM outputs are non-deterministic.

### Tuning subagent prompts against production models

When adapting a prompt like [security_engineer.md](crates/dyson/src/skill/subagent/prompts/security_engineer.md) for a non-Claude model, use the `expensive_live_security_review` example in a **run → grade → tune → run** loop:

1. Run with `--report-suffix iterN` (outputs don't clobber each other).
2. Grade against [Subagents → Evaluating report quality](docs/subagents.md#evaluating-report-quality) AND a tool-call histogram from the log.  Look for: tool-mix regressions, fabricated tool-output blocks in the report, missing severity sections, preamble leaks, missed ground-truth findings.
3. Tune the prompt — small targeted changes; never rewrite wholesale.
4. Rerun with a new suffix and diff the behavior.

Empirically: **concrete negative examples beat abstract rules**, **per-finding penalties beat whole-report ones** (the latter causes premature termination), and **"paste required" / "invent forbidden"** must be stated as two separate rules because weaker models collapse them into "don't mention tool output".  Full trajectory and case study in [docs/testing.md → Case study: tuning security_engineer.md for a smaller model](docs/testing.md#case-study-tuning-security_engineermd-for-a-smaller-model).

## Code conventions

- Comments explain **why**, not **what**.  Read existing files before adding your own.
- No emojis anywhere unless a user explicitly asks for them.
- Don't add backwards-compatibility shims when you can just change the code.
- Don't add error handling for scenarios that can't happen.  Trust internal code; validate at boundaries.
- Prefer editing existing files to creating new ones.
- Tests are next to the code they test for units (`mod tests` in the same file) and in `crates/dyson/tests/` for cross-module behavior.

## Platform

- macOS-only for the OS sandbox (requires Apple Containers).  Linux uses bubblewrap.  Windows is intentionally unsupported.
- `--dangerous-no-sandbox` is the escape hatch for environments without a sandbox binary.
