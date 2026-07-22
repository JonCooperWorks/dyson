# Crate architecture

Dyson is a Cargo workspace with dependency direction flowing from stable,
provider-neutral contracts toward the application composition root.

```text
dyson-core
   ├── dyson-harness
   ├── dyson-ast
   └── dyson-persistence ── dyson-harness

dyson-core ── dyson-dependency-analysis

all focused crates ── dyson (providers, tools, controllers, CLI composition)
```

## Package responsibilities

| Package | Owns | Must not depend on |
|---|---|---|
| `dyson-core` | Messages, artefacts, token estimates, public errors | Agent runtime, tools, controllers, providers |
| `dyson-harness` | Tool-call and execution contracts, scheduler, durable run protocol, replay, deterministic grading | Controllers, providers, persistence implementations |
| `dyson-ast` | Grammar registry, parsing, structural navigation, taint primitives | Tools, controllers, provider APIs |
| `dyson-dependency-analysis` | Manifest parsers, dependency discovery, OSV client | Tool presentation, controllers, agent loop |
| `dyson-persistence` | Chat-history trait, disk store, migrations, run journal, coalesced checkpoints | Configuration schema, controllers, providers |
| `dyson` | Configuration, provider and tool implementations, agent orchestration, controllers, CLI | Nothing below it may depend back on this package |

The `dyson` package keeps compatibility façades at the old module paths, such
as `dyson::message`, `dyson::agent::protocol`, and `dyson::chat_history`.
Consumers can migrate to focused crates without a flag day, while existing
integrations keep compiling.

## Boundary rules

1. Domain and wire types have exactly one owner. Re-export them; never copy
   equivalent structs into another package.
2. Focused crates accept dependencies through values or traits. They do not
   reach into the application composition root for global clients or config.
3. Factories that interpret `dyson.json` stay in `dyson`; implementation
   crates receive already-resolved paths, clients, and settings.
4. Durable protocol changes are versioned and must retain replay tests.
5. Every package must pass formatting, all-target tests, and Clippy with
   warnings denied as part of the workspace CI gate.
