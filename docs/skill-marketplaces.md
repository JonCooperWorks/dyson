# Skill Marketplaces And Dream-Learned Skills

Status: draft spec.

This spec defines a Swarm-hosted skill marketplace system and the mechanism
that lets agents learn durable skills through dreaming. Swarm owns the shared
marketplace catalog and fleet inventory; Dyson owns installed workspace skills
and runtime loading. The design keeps one source of truth for active skills:
installed skills are still ordinary workspace files under
`skills/<name>/SKILL.md`, and the existing `load_skill` path remains the runtime
loading mechanism.

## Goals

- Let Swarm ingest one or more skill marketplaces and expose a single catalog
  to managed Dyson instances.
- Let Dyson discover marketplace skills through Swarm instead of each instance
  carrying its own marketplace configuration.
- Let users and agents install skills from inside Dyson itself, with Swarm only
  serving the marketplace catalog and package content.
- Let an agent list, inspect, install, update, and remove marketplace skills.
- Keep installed skills plain, portable workspace files.
- Let dreaming create or improve local skills using the same installed-skill
  layout.
- Sweep marketplace-installed and dream-learned skills back to Swarm through
  the existing state-file mirror.
- Let Swarm show the skills available on one instance and across the fleet.
- Avoid prompt bloat: only skill names and short descriptions are injected;
  full skill bodies are loaded on demand.
- Keep trust explicit. A marketplace can advertise skills, but it cannot make
  Dyson execute code or install anything without an explicit tool call or
  operator action.

## Non-Goals

- Running arbitrary skill code from marketplaces in v1.
- Auto-installing marketplace skills during startup.
- Publishing dream-learned skills to a shared marketplace without user or
  admin review.
- Making Swarm the authoritative skill store in v1. Dyson workspace files
  remain the source of truth; Swarm indexes the mirrored view.
- Requiring every marketplace source to be an MCP server. Swarm should support
  simple HTTP/file indexes first and may expose an MCP facade for agents.
- Replacing MCP servers or built-in tools. Marketplace skills are prompt and
  procedure packages; they can teach the agent how to use tools, but they do
  not grant new tools by themselves.

## Current Baseline

Dyson already has the core primitives:

- `LocalSkill` discovers workspace directories shaped like
  `skills/<name>/SKILL.md`.
- `SkillListSkill` injects a compact `<available_skills>` list into the system
  prompt.
- `load_skill` reads `skills/<name>/SKILL.md` on demand and returns the body.
- `skill_create` writes or improves `SKILL.md` files in the workspace.
- `SelfImprovementDream` runs in the background and can call `skill_create`.
- `HotReloader` watches the workspace `skills/` directory and each `SKILL.md`.
- Dyson-to-Swarm state sync already includes workspace skill files.
- Swarm already stores mirrored workspace files in `instance_state_files` and
  replays `skills/<name>/SKILL.md` during instance restoration.

The marketplace feature should extend these primitives instead of creating a
parallel loader.

## Concepts

### Skill

An installed skill is a workspace directory:

```text
skills/
  code-review/
    SKILL.md
    dyson-skill.json
```

`SKILL.md` is the only file the agent loads as instructions. Extra files may be
present for future references or examples, but v1 does not execute them.

`dyson-skill.json` is optional metadata. Locally hand-written skills without it
remain valid.

### Swarm Skill Mirror

The Swarm skill mirror is not a second loader. It is a derived inventory built
from mirrored workspace files:

```text
workspace/skills/<name>/SKILL.md
workspace/skills/<name>/dyson-skill.json
```

Dyson owns writes. Swarm stores the sealed file mirror, parses enough metadata
to show operators what exists, and uses the same mirrored files during instance
restore. If the mirror is stale, the UI should say so instead of pretending the
inventory is authoritative.

### Swarm Marketplace Service

The Swarm marketplace service is the shared catalog for the fleet. It ingests
upstream marketplace sources, caches and validates indexes, and exposes a
normalized read API to Dyson instances and the Swarm UI.

Swarm is a better home than Dyson for this catalog because:

- Operators configure marketplaces once for the fleet.
- Admins can review, approve, disable, or pin skills centrally.
- Fleet-wide visibility can answer "which instances can learn or install this?"
- Dyson images stay smaller and do not need marketplace-specific network/cache
  code beyond the Swarm client.

Swarm can expose the catalog in two forms:

- HTTP control-plane API for Dyson and the Swarm UI.
- Optional read-only MCP server so agents can browse marketplace skills through
  normal tool discovery.

Install/remove/update still happen inside Dyson because those operations mutate
the workspace skill files used by `load_skill`.

Dyson should expose that install path in both places users naturally look:

- Conversation/tool surface: an agent can call the local `skill_marketplace`
  tool.
- Dyson web UI: a Skills or Mind marketplace panel can browse the Swarm catalog
  and call the same backend install/update/remove logic.

### Marketplace

A marketplace is a read-only index of available skill packages. Swarm supports
two source types in v1:

- `file`: a local JSON index on disk.
- `http`: an HTTPS JSON index fetched with a timeout and cached locally.

Both source types produce the same `MarketplaceIndex` structure.

A later `mcp` source type can let Swarm connect to a marketplace MCP server and
map its list/search/show/read tools into the same normalized
`MarketplaceIndex`. That should be an adapter at the edge, not the internal
catalog model, because marketplace MCP servers do not all expose the same tool
schema.

### Skill Package

A marketplace skill package is a record containing metadata and a way to obtain
the `SKILL.md` body. v1 supports inline bodies and relative URLs:

```json
{
  "schema_version": 1,
  "marketplace": {
    "id": "official",
    "name": "Dyson Official Skills",
    "homepage": "https://example.test/dyson-skills"
  },
  "skills": [
    {
      "name": "code-review",
      "version": "1.2.0",
      "description": "Review code for correctness, security, and tests.",
      "tags": ["coding", "review"],
      "license": "MIT",
      "min_dyson_version": "0.1.0",
      "sha256": "hex-encoded-sha256-of-skill-md",
      "content": {
        "type": "inline",
        "skill_md": "---\nname: code-review\ndescription: Review code...\n---\n\n..."
      }
    }
  ]
}
```

For URL-backed content:

```json
{
  "content": {
    "type": "url",
    "url": "skills/code-review/SKILL.md"
  }
}
```

Relative content URLs resolve against the marketplace index URL or file
directory. Absolute URLs must use `https`.

## Configuration

Add marketplace sources to Swarm config:

```json
{
  "skill_marketplace": {
    "marketplaces": [
      {
        "id": "official",
        "type": "http",
        "url": "https://example.test/dyson-skills/marketplace.json"
      },
      {
        "id": "local",
        "type": "file",
        "path": "~/.dyson/skill-marketplaces/local/marketplace.json"
      }
    ]
  }
}
```

If no marketplaces are configured, Swarm should still support a local default
index path if it exists:

```text
<swarm-data>/skill-marketplaces/marketplace.json
```

This gives operators and tests an offline marketplace without network setup.
Individual Dyson instances should not need marketplace source configuration in
normal Swarm-managed deployments. They need only their existing Swarm control
plane connection. A standalone Dyson developer mode may support a local file
marketplace later, but that is not the primary architecture.

## Installed Metadata

Marketplace installs write `dyson-skill.json` beside `SKILL.md`:

```json
{
  "schema_version": 1,
  "name": "code-review",
  "version": "1.2.0",
  "description": "Review code for correctness, security, and tests.",
  "origin": {
    "kind": "marketplace",
    "marketplace_id": "official",
    "sha256": "hex-encoded-sha256-of-skill-md"
  },
  "installed_at": "2026-05-07T09:00:00Z"
}
```

Dream-learned skills use the same metadata file with `origin.kind = "learned"`:

```json
{
  "schema_version": 1,
  "name": "debug-mcp-runtime",
  "version": "0.0.0-learned",
  "description": "Debug Docker-backed MCP runtime failures.",
  "origin": {
    "kind": "learned",
    "dream": "self-improvement",
    "turn_count": 12
  },
  "installed_at": "2026-05-07T09:00:00Z"
}
```

The loader must not require this file. It is only provenance.

## Tools

Add one Dyson built-in tool: `skill_marketplace`.

The tool is local because install/update/remove mutate the Dyson workspace. It
uses Swarm as the marketplace source in managed deployments.

The tool exposes operation-based input:

```json
{
  "op": "list_sources | list | show | install | update | remove",
  "marketplace": "official",
  "skill": "code-review",
  "version": "1.2.0",
  "force": false
}
```

### Operations

- `list_sources`: show configured marketplaces and cache status.
- `list`: list skills from one marketplace or all marketplaces.
- `show`: return metadata and the first part of `SKILL.md` for review.
- `install`: validate and copy the skill into `skills/<name>/`.
- `update`: replace an installed marketplace skill if the source has a newer
  version or if `version` is explicitly requested.
- `remove`: remove the installed skill directory only when the skill has
  marketplace or learned metadata. Hand-written skills without metadata are not
  removed unless `force = true`.

The tool should return structured text useful to the LLM and metadata useful to
controllers later.

### Swarm Catalog APIs

Swarm should expose catalog endpoints for the Dyson tool and the Swarm UI:

- `GET /v1/skill-marketplaces`: configured sources and health.
- `GET /v1/skill-marketplaces/skills`: normalized catalog across sources.
- `GET /v1/skill-marketplaces/:marketplace/skills/:skill`: package metadata
  and review preview.
- `GET /v1/skill-marketplaces/:marketplace/skills/:skill/content`: validated
  `SKILL.md` body for installation.

The content endpoint should require authentication and should return the
expected hash alongside the body. Dyson verifies the hash again before writing
workspace files.

### Dyson Install Surface

Dyson should let the user install from the Swarm marketplace without switching
to Swarm.

The install flow is:

1. Dyson lists or searches Swarm catalog entries.
2. User or agent chooses a marketplace skill inside Dyson.
3. Dyson fetches validated package content from Swarm.
4. Dyson writes `skills/<name>/SKILL.md` and `dyson-skill.json` atomically.
5. Dyson hot reloads the skill list and can `load_skill` immediately.
6. State sync sweeps the installed files back to Swarm inventory.

The Dyson UI and the Dyson `skill_marketplace` tool must call the same backend
installer so permissions, validation, hashing, metadata, and audit logs do not
fork.

### Optional Swarm MCP Server

Swarm may also expose a read-only MCP server for marketplace browsing:

- `list_skill_marketplaces`
- `search_marketplace_skills`
- `show_marketplace_skill`

Do not put write operations in this MCP server in v1. The local Dyson
`skill_marketplace` tool remains the only path that installs, updates, or
removes active workspace skills.

### Future MCP Source Adapter

Some external skill registries are already exposed as MCP servers. Swarm can
support those later as upstream sources:

1. Connect to the configured MCP server with explicit admin credentials.
2. Discover a known capability set such as list/search/show/read.
3. Normalize remote entries into `MarketplaceIndex`.
4. Cache normalized package metadata and fetched bodies.
5. Require the same validation and hash checks as HTTP/file marketplaces before
   Dyson can install anything.

This makes MCP marketplaces useful without binding Dyson's installer to every
third-party MCP tool shape.

## Loading Semantics

Marketplace install does not create a new runtime loading path.

1. `skill_marketplace install` writes `skills/<name>/SKILL.md` atomically.
2. `load_skill` can read the installed skill immediately by name.
3. The compact `<available_skills>` prompt list updates on the next hot reload
   or agent rebuild.
4. Existing `LocalSkill` parsing, size limits, and description extraction apply
   to marketplace-installed and dream-learned skills.
5. The existing state sync sweeps `skills/<name>/SKILL.md` and
   `skills/<name>/dyson-skill.json` to Swarm, where they become visible in the
   derived skill inventory.

This avoids duplicate state. If the skill file exists, the skill exists.

## Dream Learning

Dream learning remains local-first.

`SelfImprovementDream` already reviews conversation summaries and can call
`skill_create`. Extend that path as follows:

- The self-improvement prompt should mention installed marketplace skills and
  learned skills separately.
- The dream should prefer improving an existing relevant skill over creating a
  new near-duplicate.
- `skill_create` should write or update `dyson-skill.json` with
  `origin.kind = "learned"` when called from a dream context.
- The dream must not install marketplace skills automatically. It may recommend
  a marketplace skill in a log or memory entry, but installation requires an
  explicit `skill_marketplace install`.
- Learned skills are private to the workspace unless exported by a user or
  admin-reviewed process.
- Dream-created and dream-improved skills are swept to Swarm the same way as
  marketplace installs because they are ordinary workspace skill files.

The dream trigger remains controlled by workspace memory config:

- Memory maintenance fires every `nudge_interval` turns.
- Self-improvement fires every `2 * nudge_interval` turns.
- Session end fires all dreams.

## Duplicate Avoidance

Before creating a skill, `SelfImprovementDream` should compare against:

- Current `skills/<name>/SKILL.md` entries.
- Installed metadata descriptions.
- Swarm marketplace package names and descriptions when the Swarm catalog is
  reachable or cache is warm.

The first implementation can keep this simple: include the existing skill list
and metadata in the dream prompt. A later implementation can add embedding or
FTS search if needed.

## Swarm Sweep And Inventory

Swarm should make skills visible without taking ownership of them.

Swarm has two related skill views:

- Marketplace catalog: skills available to install.
- Mirrored inventory: skills actually present on each Dyson instance.

These should share presentation and provenance concepts, but they are different
data sets. Catalog entries are candidates; mirrored inventory entries are active
workspace files.

### Sweep

Dyson state sync should include:

- `workspace/skills/<name>/SKILL.md`
- `workspace/skills/<name>/dyson-skill.json`
- Optional future skill assets under `workspace/skills/<name>/...` once the
  package model explicitly allows them.

The allowlist should cover both `SKILL.md` and metadata files. The sweep remains
file-level and eventually consistent, matching the existing state-file mirror.

### Inventory Model

Swarm derives skill inventory rows from `StateFileService::list_for_instance`
instead of maintaining a separate source of truth in v1:

```json
{
  "instance_id": "inst_123",
  "skill": "code-review",
  "description": "Review code for correctness, security, and tests.",
  "origin_kind": "marketplace",
  "marketplace_id": "official",
  "version": "1.2.0",
  "installed_at": "2026-05-07T09:00:00Z",
  "updated_at": "2026-05-07T09:00:00Z",
  "has_body": true,
  "source_path": "workspace/skills/code-review/SKILL.md"
}
```

Description precedence:

1. `dyson-skill.json.description`
2. `SKILL.md` frontmatter description
3. First heading or first non-empty paragraph from `SKILL.md`
4. Empty description

Origin precedence:

1. `dyson-skill.json.origin.kind`
2. `local` when a `SKILL.md` exists without metadata
3. `unknown` when metadata exists without a body

The derived service can start as an on-demand parser. Add a materialized table
only if fleet-wide queries become expensive.

### Swarm APIs

Add read-only endpoints:

- `GET /v1/instances/:id/skills`: skills visible for one instance.
- `GET /v1/skills`: fleet-wide skill inventory, owner-scoped by default.
- `GET /v1/instances/:id/skills/:skill`: skill detail and metadata.

List endpoints should return metadata only, not the full `SKILL.md` body.
Detail may return the body for authorized users, with clear labeling that the
body is agent instruction content.

### Swarm UI

Add visibility in two places:

- Instance detail: a `Skills` panel or tab next to runtime/MCP/artifacts.
- Fleet/admin view: a `Skills` page showing skill name, instance count, origin,
  version, and last swept time.

The UI should use provenance badges:

- `local`: workspace file without marketplace or learned metadata.
- `marketplace`: installed from a configured marketplace.
- `learned`: created or improved by dreaming.
- `unknown`: metadata/body mismatch or parse failure.

Swarm should also show stale/missing states:

- Skill body mirrored, metadata missing.
- Metadata mirrored, skill body missing.
- Last sweep older than the instance heartbeat or runtime update.

Swarm v1 does not edit, install, or remove Dyson skills. Those actions stay in
Dyson and can be added later as explicit RPCs.

## Security And Trust

Marketplace content is untrusted input.

Validation rules:

- Skill names must be lowercase ASCII letters, digits, and hyphens.
- Names cannot start or end with a hyphen.
- Install paths must stay under `skills/<name>/`.
- `SKILL.md` must be non-empty and at most the existing local skill size limit.
- If `sha256` is present, the fetched `SKILL.md` must match it.
- HTTP marketplace indexes and content URLs must use `https`.
- HTTP fetches must have strict timeouts and max response sizes.
- Relative URLs may not escape the marketplace root.
- Installs must write to a temporary file and rename atomically.
- v1 never executes files from a skill package.

Prompt injection posture:

- `show` and `install` should label marketplace content as untrusted until the
  user or agent chooses to install it.
- Installing a skill means its instructions may influence future turns.
  Therefore install should be explicit and observable in the transcript/logs.

## Cache

HTTP marketplaces should cache indexes under Swarm's data directory:

```text
<swarm-data>/cache/skill-marketplaces/<marketplace-id>/marketplace.json
```

Cache entries should record:

- fetched_at
- source URL
- response etag or last-modified when available
- parse errors, if any

`list` may use cache on network failure and should report that it is stale.
`install` may use cache only when the package has inline content with a matching
hash, or when the cached content file is present with a matching hash.

## Dyson UI

The first marketplace implementation can be tool-first inside Dyson. The Dyson
web UI can follow once the model and tests are stable. Swarm visibility is
covered separately above because operators need an inventory even before Dyson
gets a full marketplace browser.

Future Dyson UI surface:

- A Skills tab or Mind subpanel showing installed skills.
- Marketplace browser with search/filter, backed by the Swarm catalog.
- Install/update/remove buttons that call Dyson's local installer.
- Provenance badges: local, learned, marketplace.
- Dream-learned skill review queue.

## Implementation Phases

### Phase 1: Spec And Core Model

- Add Swarm marketplace config structs.
- Add marketplace index parser and validators.
- Add installed metadata helpers.
- Unit test valid/invalid names, URL/path handling, size limits, and hashes.

### Phase 2: Swarm Catalog And Dyson Tool

- Add Swarm catalog APIs.
- Add the Dyson `skill_marketplace` built-in tool backed by Swarm APIs.
- Support Swarm `file` sources and the Swarm data-dir default index.
- Implement `list_sources`, `list`, `show`, `install`, and `remove`.
- Test install into an in-memory or temporary filesystem workspace.

### Phase 3: HTTP Marketplace

- Add Swarm HTTPS fetch with timeout and size cap.
- Add Swarm cache and stale-cache behavior.
- Test with a mock HTTP server.

### Phase 4: Dream Metadata

- Mark `ToolContext` calls that originate from dreams, or pass a dream-specific
  field into `SkillCreateTool`.
- Make `skill_create` write learned provenance metadata for dream-created or
  dream-improved skills.
- Expand the self-improvement prompt to avoid duplicates and prefer improving.
- Test that a dream-created skill gets `origin.kind = "learned"`.

### Phase 5: Swarm Skill Mirror

- Ensure Dyson state sync includes `dyson-skill.json` next to `SKILL.md`.
- Add a Swarm skill inventory service derived from `instance_state_files`.
- Add per-instance and fleet-wide read-only APIs.
- Preserve skills and metadata during instance restore and replay.
- Test owner scoping, malformed metadata handling, stale mirrors, and restore.

### Phase 6: Optional Swarm Marketplace MCP

- Add a read-only Swarm MCP server exposing marketplace list/search/show.
- Keep install/update/remove out of the MCP server until there is a separate
  approval story.
- Test that MCP output matches HTTP catalog output.

### Phase 7: Optional MCP Source Adapter

- Add an `mcp` marketplace source type in Swarm.
- Normalize external MCP marketplace entries into the same catalog model used
  by file and HTTP sources.
- Cache and validate fetched package bodies before Dyson installs them.
- Test with a mock MCP marketplace server and malformed tool responses.

### Phase 8: UI

- Add installed skills and marketplace browsing to the Dyson web UI, backed by
  Swarm catalog APIs.
- Add marketplace catalog, per-instance skill inventory, and fleet-wide skill
  inventory to the Swarm UI.
- Keep all write operations routed through the same backend/tool logic.

## Test Plan

Core Rust tests:

- Parse marketplace indexes.
- Swarm catalog APIs normalize file and HTTP marketplace sources.
- Reject unknown schema versions.
- Reject invalid skill names and traversal paths.
- Verify SHA-256 before install.
- Install writes `SKILL.md` and `dyson-skill.json`.
- `load_skill` loads an installed marketplace skill.
- Removing a marketplace skill does not remove hand-written local skills by
  default.
- Hot reload sees newly installed `SKILL.md`.
- Dream-created skill metadata records `origin.kind = "learned"`.
- State sync allowlist accepts `skills/<name>/dyson-skill.json`.
- Swarm skill inventory derives local, marketplace, learned, and unknown
  origins from mirrored files.

Integration tests:

- Swarm file marketplace end-to-end: list, show, install through Dyson, load.
- Swarm HTTP marketplace end-to-end with mock server and stale cache fallback.
- Optional MCP source end-to-end with a mock marketplace MCP server.
- Self-improvement dream creates or improves a skill without blocking the main
  agent loop.
- Dyson-to-Swarm sweep makes installed and learned skills visible in
  `GET /v1/instances/:id/skills`.
- Swarm restore replays both `SKILL.md` and `dyson-skill.json`.
- Fleet-wide skill inventory is scoped to instances the caller can access.

UI tests:

- Dyson shows installed marketplace and learned skills from the Swarm-backed
  catalog/inventory.
- Swarm shows marketplace catalog entries before install.
- Swarm instance detail shows the mirrored skill list.
- Swarm fleet skills page groups skills by name and instance count.

Smoke:

- Install a skill from a Swarm-hosted local marketplace into a real Dyson
  workspace.
- Install the same kind of skill through the Dyson UI or Dyson
  `skill_marketplace` tool, not only through Swarm admin views.
- Confirm it appears in `<available_skills>` after reload.
- Call `load_skill` and verify only the skill body is returned.
- Confirm the skill appears in Swarm for that instance after state sync.
- Trigger a dream-learned skill and confirm Swarm shows it as `learned`.
- Browse the Swarm marketplace through HTTP and, if enabled, the read-only MCP
  server.

## Open Questions

- Should marketplace indexes support multi-file packages in v1, or should v1
  stay `SKILL.md` only?
- Should learned skills appear in a review queue before becoming active, or is
  workspace-local immediate activation acceptable?
- Should marketplace installs be allowed from public agents, or only private
  agents/admin contexts?
- Should Swarm detail views show full `SKILL.md` bodies by default, or require
  an explicit reveal because skill bodies are prompt instructions?
- Do we want signed marketplace indexes in v1, or is HTTPS plus SHA-256 enough
  for the first pass?
