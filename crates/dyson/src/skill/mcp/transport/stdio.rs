//! Stdio MCP transport: a child process spoken to over JSON lines.

use super::*;

// ---------------------------------------------------------------------------
// StdioTransport
// ---------------------------------------------------------------------------

/// Manages a stdio connection to an MCP server process.
///
/// Spawns the process, sends requests via stdin, reads responses from
/// stdout.  A background task matches responses to waiting callers by ID.
pub struct StdioTransport {
    stdin: Arc<Mutex<tokio::process::ChildStdin>>,
    next_id: AtomicU64,
    pub(super) pending: Arc<Mutex<HashMap<u64, oneshot::Sender<PendingResponse>>>>,
    /// Handler for server-originated requests/notifications.  Shared with
    /// the reader task; swapped in by `set_inbound_handler`.  A `std`
    /// mutex is fine: it is only ever held to clone the `Arc` out, never
    /// across an `.await`.
    inbound: Arc<std::sync::Mutex<Arc<dyn InboundHandler>>>,
    _child: Arc<Mutex<Child>>,
    /// Bounds requests in flight to one server (each holds a pending
    /// entry and possibly a large serialized frame).
    inflight: Arc<tokio::sync::Semaphore>,
    /// Background task handles — aborted on drop to prevent orphaned tasks.
    _reader_handle: tokio::task::JoinHandle<()>,
}

/// Concurrent requests allowed per stdio MCP server.
const MAX_STDIO_INFLIGHT: usize = 64;

/// Removes a request's pending entry however `send_request` exits —
/// error return, timeout, or the caller dropping the future.
struct PendingGuard {
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<PendingResponse>>>>,
    id: u64,
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        let id = self.id;
        if let Ok(mut pending) = self.pending.try_lock() {
            pending.remove(&id);
            return;
        }
        let pending = Arc::clone(&self.pending);
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                pending.lock().await.remove(&id);
            });
        }
    }
}

impl StdioTransport {
    /// Spawn an MCP server process and establish communication.
    ///
    /// When `sandbox` is true on Linux and `bwrap` is on PATH, the
    /// subprocess is wrapped with `bwrap` using a read-only root,
    /// tmpfs `/tmp`, PID-namespace isolation, and `--die-with-parent`.
    /// If `deny_network` is also true, the network namespace is
    /// unshared so the server cannot reach the network.
    ///
    /// On non-Linux or when bwrap is missing, `sandbox = true` falls
    /// back to the unsandboxed path with a warning.
    pub async fn spawn(
        command: &str,
        args: &[String],
        env: &HashMap<String, String>,
        sandbox: bool,
        deny_network: bool,
    ) -> Result<Self> {
        let (exec, exec_args) = Self::resolve_exec(command, args, sandbox, deny_network)?;
        let mut cmd = Command::new(&exec);
        // Start from an empty environment: the server gets the same
        // allowlisted parent vars as `bash` (PATH, HOME, locale, proxy…)
        // plus whatever its config names explicitly — never Dyson's
        // provider keys or swarm tokens.  bwrap passes its own env
        // through, so this also covers the sandboxed path.
        cmd.args(&exec_args)
            .env_clear()
            .envs(std::env::vars().filter(|(k, _)| crate::tool::bash::is_safe_env_var(k)))
            .envs(env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());

        let mut child = cmd.spawn().map_err(|e| DysonError::Mcp {
            server: command.to_string(),
            message: format!("failed to spawn: {e}"),
        })?;

        let stdin = child.stdin.take().ok_or_else(|| DysonError::Mcp {
            server: command.to_string(),
            message: "failed to open stdin".into(),
        })?;

        let stdout = child.stdout.take().ok_or_else(|| DysonError::Mcp {
            server: command.to_string(),
            message: "failed to open stdout".into(),
        })?;

        let stdin = Arc::new(Mutex::new(stdin));
        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<PendingResponse>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let inbound: Arc<std::sync::Mutex<Arc<dyn InboundHandler>>> =
            Arc::new(std::sync::Mutex::new(Arc::new(UnsupportedInboundHandler)));

        // Background reader task.
        let pending_clone = Arc::clone(&pending);
        let inbound_clone = Arc::clone(&inbound);
        let stdin_clone = Arc::clone(&stdin);
        let command_name = command.to_string();
        let reader_handle = tokio::spawn(async move {
            let mut reader = BufReader::new(stdout);

            loop {
                let line = match read_line_capped(&mut reader, MAX_MCP_RESULT_BYTES).await {
                    Ok(Some(line)) => line,
                    Ok(None) => {
                        // The server closed stdout: nothing will answer the
                        // callers still waiting, so fail them now instead of
                        // leaving them on the response timeout.
                        let mut pending = pending_clone.lock().await;
                        for (_, tx) in pending.drain() {
                            let _ = tx.send(Err("MCP server closed its output".into()));
                        }
                        break;
                    }
                    Err(e) => {
                        let message = e.to_string();
                        tracing::warn!(
                            server = command_name,
                            error = %message,
                            "failed to read MCP server output"
                        );
                        let mut pending = pending_clone.lock().await;
                        for (_, tx) in pending.drain() {
                            let _ = tx.send(Err(message.clone()));
                        }
                        break;
                    }
                };
                if line.is_empty() {
                    continue;
                }

                let value: Value = match serde_json::from_str(&line) {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::debug!(
                            server = command_name,
                            error = %e,
                            "failed to parse MCP server output"
                        );
                        continue;
                    }
                };

                match classify_inbound(&value) {
                    Inbound::Response => {
                        let response: JsonRpcResponse = match serde_json::from_value(value) {
                            Ok(r) => r,
                            Err(e) => {
                                tracing::debug!(
                                    server = command_name,
                                    error = %e,
                                    "failed to parse MCP response frame"
                                );
                                continue;
                            }
                        };
                        if let Some(id) = response.id {
                            let mut pending = pending_clone.lock().await;
                            if let Some(tx) = pending.remove(&id) {
                                let _ = tx.send(Ok(response));
                            }
                        }
                    }
                    // Server-originated request: dispatch to the handler on
                    // its own task (the handler may block — e.g. an
                    // elicitation awaiting user input — and must not stall
                    // the reader) and write the response back over stdin.
                    Inbound::Request { id, method, params } => {
                        let handler = Arc::clone(&inbound_clone.lock().unwrap());
                        let stdin = Arc::clone(&stdin_clone);
                        let server = command_name.clone();
                        tokio::spawn(async move {
                            let result = handler.handle_request(&method, params).await;
                            let line = build_response_line(&id, result);
                            if let Err(e) = write_line(&stdin, &server, &line).await {
                                tracing::warn!(
                                    server = server,
                                    error = %e,
                                    "failed to write inbound MCP response"
                                );
                            }
                        });
                    }
                    Inbound::Notification { method, params } => {
                        let handler = Arc::clone(&inbound_clone.lock().unwrap());
                        tokio::spawn(async move {
                            handler.handle_notification(&method, params).await;
                        });
                    }
                }
            }

            tracing::debug!(server = command_name, "MCP server stdout closed");
        });

        Ok(Self {
            stdin,
            next_id: AtomicU64::new(1),
            pending,
            inbound,
            _child: Arc::new(Mutex::new(child)),
            inflight: Arc::new(tokio::sync::Semaphore::new(MAX_STDIO_INFLIGHT)),
            _reader_handle: reader_handle,
        })
    }
}

/// Write a single newline-terminated JSON line to an MCP server's stdin.
/// Shared by outbound requests/notifications and inbound responses.
async fn write_line(
    stdin: &Arc<Mutex<tokio::process::ChildStdin>>,
    server: &str,
    json: &str,
) -> Result<()> {
    let mut stdin = stdin.lock().await;
    stdin
        .write_all(json.as_bytes())
        .await
        .map_err(|e| DysonError::Mcp {
            server: server.to_string(),
            message: format!("stdin write failed: {e}"),
        })?;
    stdin.write_all(b"\n").await.map_err(|e| DysonError::Mcp {
        server: server.to_string(),
        message: format!("stdin write failed: {e}"),
    })?;
    stdin.flush().await.map_err(|e| DysonError::Mcp {
        server: server.to_string(),
        message: format!("stdin flush failed: {e}"),
    })?;
    Ok(())
}

impl StdioTransport {
    /// Decide whether to invoke the MCP command directly or wrap it in
    /// `bwrap`.  Returns `(exec, argv)` suitable for `Command::new` +
    /// `.args()`.
    ///
    /// When `sandbox == true` and no wrapper is available (bwrap
    /// missing on Linux, no wrapper on non-Linux), returns an error
    /// rather than falling back to a direct unsandboxed spawn — that
    /// fallback was a silent sandbox bypass.
    fn resolve_exec(
        command: &str,
        args: &[String],
        sandbox: bool,
        deny_network: bool,
    ) -> Result<(String, Vec<String>)> {
        #[cfg(target_os = "linux")]
        let has_bwrap = std::process::Command::new("sh")
            .arg("-c")
            .arg("command -v bwrap")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        #[cfg(not(target_os = "linux"))]
        let has_bwrap = false;

        Self::resolve_exec_with(command, args, sandbox, has_bwrap, deny_network)
    }

    /// Pure decision function — extracted so the bwrap-missing path
    /// is testable without depending on the host's PATH.
    pub(super) fn resolve_exec_with(
        command: &str,
        args: &[String],
        sandbox: bool,
        has_bwrap: bool,
        deny_network: bool,
    ) -> Result<(String, Vec<String>)> {
        if !sandbox {
            return Ok((command.to_string(), args.to_vec()));
        }
        if has_bwrap {
            let argv =
                crate::sandbox::os::build_bwrap_argv_for_mcp_stdio(command, args, deny_network);
            return Ok(("bwrap".to_string(), argv));
        }
        Err(DysonError::Mcp {
            server: command.to_string(),
            message: "sandbox requested but bwrap is unavailable on this host; \
                      install bubblewrap or run with --dangerous-no-sandbox to \
                      accept an unsandboxed MCP stdio process"
                .into(),
        })
    }
}

impl Drop for StdioTransport {
    fn drop(&mut self) {
        self._reader_handle.abort();
    }
}

#[async_trait]
impl McpTransport for StdioTransport {
    async fn send_request(
        &self,
        method: &str,
        params: Option<serde_json::Value>,
    ) -> Result<serde_json::Value> {
        let _permit = self.inflight.acquire().await.map_err(|_| DysonError::Mcp {
            server: method.to_string(),
            message: "transport closed".into(),
        })?;
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let request = JsonRpcRequest::new(id, method, params);
        let json = serde_json::to_string(&request)?;
        enforce_mcp_request_cap(method, &json)?;

        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        // From here on every exit path (including the caller dropping
        // this future) removes the entry.
        let _guard = PendingGuard {
            pending: Arc::clone(&self.pending),
            id,
        };
        write_line(&self.stdin, method, &json).await?;

        /// Maximum time to wait for an MCP server to respond to a request.
        const MCP_RESPONSE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

        let response = match tokio::time::timeout(MCP_RESPONSE_TIMEOUT, rx).await {
            Ok(Ok(Ok(resp))) => resp,
            Ok(Ok(Err(message))) => {
                self.pending.lock().await.remove(&id);
                return Err(DysonError::Mcp {
                    server: method.to_string(),
                    message,
                });
            }
            Ok(Err(_)) => {
                // Channel closed — server died. Clean up the pending entry.
                self.pending.lock().await.remove(&id);
                return Err(DysonError::Mcp {
                    server: method.to_string(),
                    message: "response channel closed (server died?)".into(),
                });
            }
            Err(_) => {
                // Timeout — clean up the orphaned pending entry.
                self.pending.lock().await.remove(&id);
                return Err(DysonError::Mcp {
                    server: method.to_string(),
                    message: format!(
                        "response timed out after {}s",
                        MCP_RESPONSE_TIMEOUT.as_secs()
                    ),
                });
            }
        };

        if let Some(err) = response.error {
            return Err(DysonError::Mcp {
                server: method.to_string(),
                message: format!("RPC error {}: {}", err.code, err.message),
            });
        }

        response.result.ok_or_else(|| DysonError::Mcp {
            server: method.to_string(),
            message: "response has neither result nor error".into(),
        })
    }

    async fn send_notification(
        &self,
        method: &str,
        params: Option<serde_json::Value>,
    ) -> Result<()> {
        let notification = JsonRpcNotification::new(method, params);
        let json = serde_json::to_string(&notification)?;
        enforce_mcp_request_cap(method, &json)?;
        write_line(&self.stdin, method, &json).await
    }

    fn set_inbound_handler(&self, handler: Arc<dyn InboundHandler>) {
        *self.inbound.lock().unwrap() = handler;
    }
}
