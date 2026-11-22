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
| `dyson-persistence` | Chat-history trait, disk store, run journal, coalesced checkpoints | Configuration schema, controllers, providers |
| `dyson` | Configuration, provider and tool implementations, agent orchestration, controllers, CLI | Nothing below it may depend back on this package |

Code imports each type from the crate that owns it (`dyson_core::message`,
`dyson_harness::protocol`, `dyson_persistence::ChatHistory`, ...); the `dyson`
package does not re-export the focused crates under its own module paths.

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
