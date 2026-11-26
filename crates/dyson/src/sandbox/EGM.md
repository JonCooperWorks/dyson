# Sandbox (`sandbox`)

## Overview
The security gate between the LLM and tool execution. Every tool call goes through `Sandbox::check`, which returns Allow (possibly with rewritten input), Deny, or Redirect; `Sandbox::after` may inspect and mutate output. `PolicySandbox` enforces per-tool capability policies at the application level for Rust-native tools and at the OS level for `bash` (bubblewrap on Linux, Apple Containers on macOS). `CedarSandbox` layers Cedar policies in front of another sandbox. `DangerousNoSandbox` is a passthrough enabled only by the CLI flag.

## Interface
- `Sandbox` trait: `check(tool_name, input, ctx)`, `after(...)`, `sandbox_bypass()` [mod.rs:Sandbox]
- `SandboxDecision { Allow { input } | Deny { reason } | Redirect { tool_name, input } }` [mod.rs:SandboxDecision]
- `create_sandbox(config, sandbox_bypass) -> Arc<dyn Sandbox>` [mod.rs:create_sandbox]
- `SandboxBackendStatus { Ready | DangerousDisabled | MissingBackend(&str) | UnsupportedPlatform }`, `current_sandbox_backend_status`, `sandbox_backend_status_for_target` [mod.rs:SandboxBackendStatus]
- `SandboxBypassGuard`, `SandboxBypassPurpose`, `sandbox_bypass_from_cli_flag(flag)` [mod.rs:SandboxBypassGuard, mod.rs:sandbox_bypass_from_cli_flag]
- `PolicySandbox`, `redact_secrets`, `resolve_canonical` [policy_sandbox.rs:PolicySandbox]
- `SandboxPolicy`, `Access`, `PathAccess`, `default_policy(tool_name, working_dir)`, `ToolPolicyConfig`, `ToolPolicyPathConfig`, `resolve_policy`, `PolicyTable` [policy.rs:SandboxPolicy, policy.rs:default_policy]
- `CedarSandbox`, `Verdict` [cedar_sandbox.rs:CedarSandbox]
- `build_bwrap_command_from_policy`, `build_bwrap_argv_for_mcp_stdio`, `build_container_command_from_policy` [os.rs:build_bwrap_command_from_policy]
- `DangerousNoSandbox` [no_sandbox.rs:DangerousNoSandbox]
- `cedar/` holds `default.cedar` and `tools.cedarschema`, embedded with `include_str!` [cedar_sandbox.rs:DEFAULT_POLICIES, cedar_sandbox.rs:SCHEMA]

## Invariants
- The sandbox can be disabled only via the `--dangerous-no-sandbox` CLI flag; `sandbox.disabled` and `sandbox.os_profile` in config are ignored with a warning [mod.rs:create_sandbox]
- If the OS backend (`bwrap` on Linux, `container` on macOS) is missing or the platform is unsupported, the process exits with status 1 rather than running unsandboxed [mod.rs:create_sandbox]
- Cedar only narrows what the inner sandbox allows; `audit` mode logs would-deny decisions, `enforce` mode turns a Cedar deny into `SandboxDecision::Deny`. Cedar stays active even when the OS sandbox is disabled [cedar_sandbox.rs:CedarSandbox, mod.rs:create_sandbox]
- Sandboxed output is truncated to `MAX_OUTPUT_CHARS = 100_000` [mod.rs:MAX_OUTPUT_CHARS, policy_sandbox.rs:PolicySandbox]
- `PolicySandbox` checks the paths of `bulk_edit` (read+write), `ast_describe` / `ast_query` / `attack_surface_analyzer` / `dependency_scan` (`path`, read) and `taint_trace` (`source.file` / `sink.file`, read) against the file policies; `dependency_scan` and `image_generate` also need network; `skill_create` is internal like `workspace` [policy_sandbox.rs:check_named_paths, policy.rs:default_policy]
- `resolve_canonical` canonicalises each existing prefix before applying a following `..` (as the kernel does); only the nonexistent tail is normalised lexically [policy_sandbox.rs:resolve_canonical]
- Cedar judges every path in `file_path`, `path`, `source.file` and `sink.file`, and derives `host` the way the HTTP client parses the URL [cedar_sandbox.rs:path_candidates, cedar_sandbox.rs:url_host]
- Tool instances handed to CLI providers (called through the in-process MCP server, outside the agent loop) are wrapped by `gate_tools` so they still pass `check` / `after` [mod.rs:gate_tools]
- bwrap mounts include essential system dirs `/usr`, `/bin`, `/sbin`, `/lib`, `/lib64` [os.rs:ESSENTIAL_SYSTEM_DIRS]

## Side Effects
- May terminate the process (`std::process::exit(1)`) at startup when no backend is available [mod.rs:create_sandbox]
- Rewrites `bash` invocations into `bwrap` / `container` commands [os.rs:build_bwrap_command_from_policy, os.rs:build_container_command_from_policy]
- Reads `PATH` to locate backends [mod.rs:current_sandbox_backend_status], and `SWARM_INSTANCE_ID` / `HOME` for Cedar context [cedar_sandbox.rs:CedarSandbox]
- Logs Cedar decisions on the `sandbox.cedar` tracing target [cedar_sandbox.rs:CedarSandbox]

## Verification
- Run: `cargo test -p dyson` (from `dyson/`)
