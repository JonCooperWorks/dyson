# Sandbox

The sandbox is the security gate between the LLM and tool execution.  Every
tool call passes through the sandbox before running.  The sandbox can allow,
deny, or redirect calls — giving you a hook to enforce policies, route tools
to containers, or audit everything the agent does.

**Key files:**
- `src/sandbox/mod.rs` — `Sandbox` trait, `SandboxDecision`
- `src/sandbox/no_sandbox.rs` — `DangerousNoSandbox` (passthrough)

---

## Sandbox Trait

```rust
#[async_trait]
pub trait Sandbox: Send + Sync {
    async fn check(
        &self,
        tool_name: &str,
        input: &Value,
        ctx: &ToolContext,
    ) -> Result<SandboxDecision>;

    async fn after(
        &self,
        tool_name: &str,
        input: &Value,
        output: &mut ToolOutput,
    ) -> Result<()> {
        Ok(())
    }
}
```

| Method | When called | Purpose |
|--------|-------------|---------|
| `check()` | Before every tool call | Decide: Allow, Deny, or Redirect |
| `after()` | After tool executes (success only) | Post-process output: redact, audit, truncate |

### SandboxDecision

```rust
pub enum SandboxDecision {
    Allow { input: Value },
    Deny { reason: String },
    Redirect { tool_name: String, input: Value },
}
```

| Variant | What happens | LLM sees |
|---------|-------------|----------|
| `Allow { input }` | Tool runs with (possibly rewritten) input | Normal tool result |
| `Deny { reason }` | Tool does NOT run | Error tool_result with deny reason |
| `Redirect { tool_name, input }` | A *different* tool runs | Normal tool result (LLM doesn't know) |

---

## The Three Decisions

### Allow

The most common decision.  The tool call proceeds.  The input in `Allow` may
be the original input unchanged, or a rewritten version:

```rust
// Pass through unchanged
SandboxDecision::Allow { input: input.clone() }

// Rewrite: add safety flags
let mut safe_input = input.clone();
safe_input["command"] = json!(format!("{} --dry-run", input["command"].as_str().unwrap()));
SandboxDecision::Allow { input: safe_input }
```

### Deny

The tool call is blocked entirely.  The deny reason is sent back to the LLM
as an error `tool_result` so it can understand why and try something else:

```rust
SandboxDecision::Deny {
    reason: "bash command 'rm -rf /' denied by sandbox policy".into()
}
```

The LLM sees: `{"type":"tool_result","content":"Denied by sandbox: bash command 'rm -rf /' denied by sandbox policy","is_error":true}`

### Redirect

The call is transparently rerouted to a different tool.  The LLM doesn't
know the redirect happened — it gets back a normal tool_result for its
original tool_use:

```rust
// Route file reads to S3 instead of the local filesystem
SandboxDecision::Redirect {
    tool_name: "s3_read_file".into(),
    input: json!({ "bucket": "my-bucket", "key": path }),
}
```

This is the key innovation.  It doesn't just block things — it can
transparently swap tool implementations.

---

## DangerousNoSandbox

The "I know what I'm doing" passthrough:

```rust
pub struct DangerousNoSandbox;

impl Sandbox for DangerousNoSandbox {
    async fn check(&self, _: &str, input: &Value, _: &ToolContext) -> Result<SandboxDecision> {
        Ok(SandboxDecision::Allow { input: input.clone() })
    }
    // after() uses the default no-op
}
```

Selected via `--dangerous-no-sandbox` CLI flag.  Required in Phase 1 because
no other sandbox exists yet.  The name is intentionally alarming.

### Why not `Option<Box<dyn Sandbox>>`?

Making the sandbox mandatory (not optional) means the agent loop always has
the same code path: `sandbox.check()` → `tool.run()` → `sandbox.after()`.
No `if let Some(sandbox)` branching.  When you add a real sandbox, you just
swap the impl — zero changes to the agent loop.

---

## Future Sandbox Implementations

| Sandbox | `check()` behavior | `after()` behavior |
|---------|-------------------|-------------------|
| `ContainerSandbox` | Redirect bash/python to Docker exec | None |
| `BlacklistSandbox` | Deny specific tools or command patterns | None |
| `S3Sandbox` | Redirect read_file/write_file to S3 ops | None |
| `AuditSandbox` | Allow everything | Log all calls to a file |
| `CompositeSandbox` | Chain multiple sandboxes (first Deny wins) | Chain all afters |

### ContainerSandbox (example design)

```rust
struct ContainerSandbox {
    container_id: String,
    mount_map: HashMap<PathBuf, PathBuf>,  // host → container
}

impl Sandbox for ContainerSandbox {
    async fn check(&self, tool: &str, input: &Value, _: &ToolContext) -> Result<SandboxDecision> {
        match tool {
            "bash" => {
                // Rewrite: run inside the container
                let cmd = input["command"].as_str().unwrap();
                Ok(SandboxDecision::Allow {
                    input: json!({
                        "command": format!("docker exec {} bash -c '{}'", self.container_id, cmd)
                    })
                })
            }
            "read_file" => {
                // Rewrite path to container mount
                // ...
            }
            _ => Ok(SandboxDecision::Allow { input: input.clone() })
        }
    }
}
```

### CompositeSandbox (example design)

```rust
struct CompositeSandbox {
    sandboxes: Vec<Box<dyn Sandbox>>,
}

impl Sandbox for CompositeSandbox {
    async fn check(&self, tool: &str, input: &Value, ctx: &ToolContext) -> Result<SandboxDecision> {
        let mut current_input = input.clone();

        for sandbox in &self.sandboxes {
            match sandbox.check(tool, &current_input, ctx).await? {
                SandboxDecision::Deny { reason } => return Ok(SandboxDecision::Deny { reason }),
                SandboxDecision::Redirect { .. } => return Ok(/* redirect */),
                SandboxDecision::Allow { input } => current_input = input,
            }
        }

        Ok(SandboxDecision::Allow { input: current_input })
    }
}
```

---

## Integration with the Agent Loop

The sandbox sits in the hot path of every tool call:

```
Agent.execute_tool_call(call)
  │
  ├── sandbox.check(name, input, ctx)
  │     │
  │     ├── Allow { input }
  │     │     tool.run(input, ctx) → ToolOutput
  │     │     sandbox.after(name, input, &mut output)
  │     │
  │     ├── Deny { reason }
  │     │     ToolOutput::error("Denied by sandbox: {reason}")
  │     │
  │     └── Redirect { tool_name, input }
  │           tools[tool_name].run(input, ctx) → ToolOutput
  │           sandbox.after(tool_name, input, &mut output)
  │
  └── output.tool_result(&tool_output)
      Message::tool_result(call.id, content, is_error)
```

The sandbox is `Send + Sync` because the agent may process tool calls from
multiple sessions in the future.

---

See also: [Architecture Overview](architecture-overview.md) ·
[Agent Loop](agent-loop.md) · [Tools & Skills](tools-and-skills.md)
