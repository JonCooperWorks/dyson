# crates

## Overview

The six Cargo workspace members of the Dyson runtime [../Cargo.toml:workspace.members].
`dyson` is the composition root; the others are focused libraries it depends on.

## Subdirectories & Boundaries

- dyson: [./dyson/EGM.md](./dyson/EGM.md) - composition root: agent loop, providers, tools, skills, sandbox, controllers, config, CLI, embedded web UI
- dyson-core: [./dyson-core/EGM.md](./dyson-core/EGM.md) - provider-neutral `DysonError` and `Message`/`ContentBlock` conversation model
- dyson-harness: [./dyson-harness/EGM.md](./dyson-harness/EGM.md) - tool execution plans, dependency scheduler, `RunCursor`, versioned `RunEvent` protocol and run grading
- dyson-persistence: [./dyson-persistence/EGM.md](./dyson-persistence/EGM.md) - `ChatHistory`, `DiskChatHistory`, fsynced run journal, `CoalescingPersister`
- dyson-ast: [./dyson-ast/EGM.md](./dyson-ast/EGM.md) - tree-sitter grammar registry, parsing/walking limits, cross-file taint tracing
- dyson-dependency-analysis: [./dyson-dependency-analysis/EGM.md](./dyson-dependency-analysis/EGM.md) - manifest/lockfile parsers and OSV vulnerability lookup
