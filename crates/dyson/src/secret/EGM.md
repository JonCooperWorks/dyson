# Secrets (`secret`)

## Overview
Pluggable secret resolution for configuration values. A config value is either a literal or a `{resolver, name}` reference; `SecretRegistry` dispatches references to registered `SecretResolver` backends. The default backend reads environment variables and is intended for development.

## Interface
- `SecretResolver` trait - `resolve(&self, key) -> Result<String>` [mod.rs:SecretResolver]
- `SecretValue { Literal(String) | Reference { resolver, name } }` [mod.rs:SecretValue]
- `SecretRegistry` - `new`, `register(name, resolver)`, `resolve(&SecretValue) -> Result<Credential>`, `resolve_or_env_fallback(...)` [mod.rs:SecretRegistry]
- `InsecureEnvironmentVariable` - resolver backed by `std::env::var(key)` [insecure_env.rs:InsecureEnvironmentVariable]

## Invariants
- The resolver key is passed through verbatim (no prefix stripping or transformation) [mod.rs:SecretResolver]
- An unknown resolver name is a config error [mod.rs:SecretRegistry::resolve]
- `SecretRegistry::default()` registers only `insecure_env` [mod.rs:SecretRegistry]

## Side Effects
- Reads process environment variables [insecure_env.rs:InsecureEnvironmentVariable]

## Verification
- Run: `cargo test -p dyson` (from `dyson/`)
