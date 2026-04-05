# Prompt Caching

Prompt caching avoids redundant computation by reusing KV cache entries across
requests that share a common prefix.  When every turn of a conversation starts
with the same system prompt and tool definitions, the inference engine computes
the KV for that prefix once and reuses it on subsequent turns.

Dyson's prompt structure is designed around this: the `LlmClient` trait splits
the system prompt into a **stable prefix** (cached) and an **ephemeral suffix**
(not cached), and the Anthropic client places cache breakpoints at four
strategic locations.

**Key files:**
- `src/llm/anthropic.rs` — cache breakpoint placement (the 4-breakpoint strategy)
- `src/llm/mod.rs` — `LlmClient::stream()` trait (`system` + `system_suffix` split)
- `src/agent/mod.rs` — stable prompt composition and ephemeral context collection

---

## How It Works Inside the Inference Engine

This section describes what happens inside the inference server when prompt
caching is enabled.  The mechanism is documented in detail in
[rLLM](https://github.com/JonCooperWorks/rLLM), a Rust inference engine whose
[KV cache](https://github.com/JonCooperWorks/rLLM/blob/master/docs/kv-cache.md)
and prompt caching implementations are the reference for this explanation.

### KV cache basics

During **prefill** the model processes the prompt's token sequence through all
transformer layers, producing a set of K (key) and V (value) vectors at each
layer.  These vectors are stored in a paged KV cache — fixed-size blocks of 16
tokens each, mapped through a block table (like virtual memory page tables).

The KV for a given token sequence is **deterministic**: the same tokens at the
same positions always produce the same K/V vectors.  This is what makes caching
possible.

### Prefix caching mechanism

Caching works at the **block boundary**.  A 100-token system prompt occupies
`ceil(100/16) = 7` blocks, but only the first 6 full blocks (96 tokens) are
cacheable.  The remaining 4 tokens sit in a partial block that can't be shared —
the next request might have different tokens in those positions.

When a new request arrives, the engine checks the prompt against a prefix cache
before running prefill:

1. Hash progressive block-aligned prefixes of the prompt (longest first)
2. On match: verify full token equality (hash collision safety)
3. Link the cached blocks into the new sequence's block table
4. Set `seq_len = prefix_token_count` (skip those positions in prefill)
5. Only prefill the remaining suffix tokens

The sequence's block table ends up with shared blocks from the cache followed by
freshly allocated blocks for the suffix:

```
block_table: [shared_0, shared_1, ..., shared_n, own_0, own_1, ...]
              <--- from cache --->  <--- newly allocated --->
```

The attention kernel sees a contiguous logical sequence — the block table
indirection makes the sharing transparent.

### Reference counting and eviction

Each cache entry tracks how many active sequences use its blocks (`ref_count`).
Blocks can't be freed while `ref_count > 0`.  When a sequence finishes, it only
returns blocks it **owns** (allocated after the shared prefix); the shared
prefix blocks stay allocated until the cache evicts the entry via LRU.

The first request with a new prefix pays full prefill cost.  Every subsequent
request with the same prefix pays only the suffix cost.

---

## Why Dyson's Prompt Is Structured This Way

The core insight: **anything that changes between turns breaks the cache from
that point forward.**  A timestamp injected into the middle of the system prompt
would invalidate the KV cache for everything after it, forcing a full re-prefill
every turn.

This is why the `LlmClient` trait takes two separate system prompt parameters:

```rust
async fn stream(
    &self,
    messages: &[Message],
    system: &str,           // Stable prefix (cacheable)
    system_suffix: &str,    // Ephemeral per-turn context (NOT cacheable)
    tools: &[ToolDefinition],
    config: &CompletionConfig,
) -> Result<StreamResponse>;
```

The agent composes these separately:

- **`system`** — built once at agent construction: the base system prompt +
  model/provider info + each skill's `system_prompt()` fragment.  Immutable
  across the session.

- **`system_suffix`** — collected fresh every turn via `collect_skill_context()`,
  which calls each skill's `before_turn()` hook.  This is where ephemeral data
  like timestamps and per-turn state goes.

By keeping them separate at the trait level, every provider can make the right
caching decision.  The Anthropic client sends them as two distinct content
blocks.  Providers without cache control (OpenAI, CLI wrappers) just concatenate
them.

---

## The 4-Breakpoint Strategy (Anthropic)

The Anthropic Messages API supports up to 4 `cache_control` breakpoints.  Dyson
uses all of them (`src/llm/anthropic.rs`):

```
Request structure:

  system: [
    { text: <stable system prompt>,  cache_control: "ephemeral" },  // breakpoint 1
    { text: <ephemeral suffix> }                                     // NO cache_control
  ]

  tools: [
    { name: "bash", ... },
    { name: "read_file", ... },
    ...
    { name: "last_tool", ..., cache_control: "ephemeral" }          // breakpoint 2
  ]

  messages: [
    { role: "user",      content: [...] },
    { role: "assistant", content: [...] },
    ...
    { role: "...",       content: [..., cache_control: "ephemeral"] }, // breakpoint 3
    { role: "user",      content: [...] }                              // latest turn
  ]
```

### Breakpoint 1: Stable system prompt

The large system prompt (identity, capabilities, tool descriptions from skills)
is marked with `cache_control`.  This is the highest-value breakpoint — the
system prompt is identical across every turn in the session.

### Breakpoint 2: Last tool definition

Tool definitions are stable within a session (they don't change between turns).
Marking the last tool with `cache_control` caches the entire tool array as part
of the prefix.

### Breakpoint 3: Penultimate user message

Conversation history grows monotonically — messages are appended, never removed
(outside of compaction).  Placing a breakpoint on the second-to-last message
means the cache covers the stable conversation prefix.  Only the latest turn
(the most recent message) needs re-processing.

### The ephemeral suffix (no breakpoint)

The system suffix (timestamps, per-turn skill context) is sent as a separate
content block **without** `cache_control`.  This is the critical design choice:
because it's a separate block that comes after the cached system prompt, it
doesn't invalidate the cached prefix.

If the suffix were concatenated into the system prompt, the entire system prompt
would change every turn, and breakpoint 1 would never hit.

---

## What Gets Cached in Practice

| Component | Changes? | Cached? | Typical size |
|-----------|----------|---------|-------------|
| System prompt | Never (within session) | Yes (breakpoint 1) | 500-2000 tokens |
| Skill prompt fragments | Never (within session) | Yes (part of system prompt) | 200-1000 tokens |
| Ephemeral suffix | Every turn | No | 20-50 tokens |
| Tool definitions | Never (within session) | Yes (breakpoint 2) | 200-1000 tokens |
| Conversation history (prefix) | Grows monotonically | Yes (breakpoint 3) | Varies |
| Latest turn | Every turn | No | Varies |

For a session with a 1500-token system prompt, 800 tokens of tool definitions,
and a 50-token ephemeral suffix, the cache turns what would be a ~2350-token
prefill into a ~50-token prefill on every turn after the first.  This is a
significant reduction in prefill compute and directly lowers time to first
token (TTFT).

Decode throughput (tokens/second) is unaffected — prompt caching only eliminates
redundant prefill work.  Once generation begins, each new token requires the
same forward pass through all layers regardless of whether the KV entries were
computed fresh or restored from cache.

---

## Provider Differences

| Provider | Caching support | How Dyson handles it |
|----------|----------------|---------------------|
| **Anthropic** | Explicit `cache_control` breakpoints (up to 4) | 4-breakpoint strategy described above |
| **OpenAI** | Automatic prefix caching (no API flags needed) | Concatenates `system` + `system_suffix` into one block |
| **Claude Code / Codex** | Handled internally by the CLI subprocess | Single concatenated system prompt via `--append-system-prompt` |
| **OpenRouter** | Depends on upstream provider | Same as OpenAI (OpenAI-compatible API) |

The `system` / `system_suffix` split in the `LlmClient` trait means the caching
strategy is a provider concern — the agent loop doesn't need to know how (or
whether) each provider implements caching.

---

## Correctness

Prompt caching is correct because:

1. **KV is deterministic** — the same tokens always produce the same K/V vectors
   at the same positions (given the same model weights)
2. **Blocks are read-only after prefill** — no sequence ever writes to a shared
   block; new tokens go into newly-allocated blocks
3. **Dyson's prompt ordering is stable** — the system prompt and tool definitions
   don't change within a session, so the token prefix is identical across turns
4. **The ephemeral suffix is isolated** — it's a separate content block after the
   cached prefix, so it never corrupts the cached KV entries

---

See also: [LLM Clients](llm-clients.md) · [Agent Loop](agent-loop.md) ·
[Tools & Skills](tools-and-skills.md) ·
[rLLM KV Cache](https://github.com/JonCooperWorks/rLLM/blob/master/docs/kv-cache.md)
