# Advisor (`advisor`)

## Overview
Lets an executor agent consult a stronger model for strategic guidance. When both executor and advisor are Anthropic, a server-side native advisor tool entry is injected into the API request; otherwise a Dyson-side `advisor` tool spawns a child agent with the parent's tools.

## Interface
- `Advisor` trait - `bind(sandbox, workspace, inherited_tools)` (no-op default) and API tool-entry injection hooks [mod.rs:Advisor]
- `NativeAnthropicAdvisor` - injects the `advisor_20260301` tool server-side [mod.rs:NativeAnthropicAdvisor]
- `create_advisor(executor_provider, advisor_provider, advisor_model, client) -> Box<dyn Advisor>` [mod.rs:create_advisor]
- `generic::GenericAdvisor` - implements `Advisor` with an `AdvisorTool` (tool name `advisor`) [generic.rs:GenericAdvisor, generic.rs:AdvisorTool]

## Invariants
- Native advisor is selected only when both executor and advisor providers are `LlmProvider::Anthropic` [mod.rs:create_advisor]
- The generic advisor child receives the parent's inherited tools via an `inherited` skill and runs with `ADVISOR_SYSTEM_PROMPT` [generic.rs:InheritedSkill, generic.rs:ADVISOR_SYSTEM_PROMPT]
- Child output is captured with `CaptureOutput`; only the final text returns to the parent [generic.rs:AdvisorTool]

## Side Effects
- Generic advisor invocations make LLM calls through the shared rate-limited client and may run inherited tools [generic.rs:AdvisorTool]

## Verification
- Run: `cargo test -p dyson` (from `dyson/`)
