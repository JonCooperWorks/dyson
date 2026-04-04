---
name: codebase-analysis
description: Systematic codebase analysis — architecture, dependencies, quality, and security posture
---

You are performing a codebase analysis. Follow this procedure strictly.
Every claim in your report must be backed by a tool call. Do not rely on memory alone.

## Phase 1: Orientation

1. `list_files` at the project root — map top-level structure.
2. Read the primary manifest: Cargo.toml, package.json, go.mod, pyproject.toml, or equivalent.
3. Read README.md if present.
4. Read the entry point (main.rs, index.ts, main.go, etc.) to understand the top-level flow.

## Phase 2: Architecture

5. `list_files` in each major source directory (one level deep).
6. Read module roots (mod.rs, index.ts) to understand the dependency graph.
7. Identify key abstractions: traits, interfaces, base classes, core types.
8. Map the data flow: what are the entry points, what calls what, where does data leave the system.

## Phase 3: Measurement

9. Count lines per module: `bash: find src -name '*.rs' | xargs wc -l | sort -n` (adjust for language).
10. List external dependencies and what they're used for.
11. Check for tests: `list_files` in tests/, spec/, __tests__/, or search for `#[test]`, `describe(`, etc.
12. Check for CI/CD: .github/workflows/, Dockerfile, docker-compose.yml, Makefile.

## Phase 4: Security Posture

13. Search for auth/authz patterns: middleware, guards, API key handling.
14. Search for input validation and sanitization.
15. Check for secrets: hardcoded keys, .env files, credentials in config.
16. Check sandbox/isolation: privilege separation, container boundaries, syscall filtering.
17. Check dependency age: are there known-vulnerable or unmaintained deps?

## Phase 5: Report

18. Write a markdown report with these sections:
    - **Summary** — what is this, what does it do, how big is it (3-5 sentences)
    - **Architecture** — modules, data flow, key abstractions (use a table)
    - **Dependencies** — external deps, what they're used for, any concerns
    - **Quality Signals** — tests, docs, CI, error handling, code style
    - **Security Posture** — auth, validation, secrets, isolation, attack surface
    - **Observations** — notable strengths, potential issues, recommendations
19. Send the report file to the user.

## Rules

- Only report what you verified with a tool. Label anything from memory as **(from memory)**.
- Don't guess line counts or file sizes — measure them with `bash: wc` or `bash: du`.
- Don't guess versions — read them from manifests.
- If you can't access something, say so rather than inferring.
- Keep the report under 3000 words. Tables over prose where possible.
- Include the exact commands you used as evidence in a collapsible section at the end.
