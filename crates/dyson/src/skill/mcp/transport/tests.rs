use super::http::{extract_all_sse_frames, extract_last_sse_data};
use super::*;

/// Spawn a mock MCP server that echoes JSON-RPC responses.
///
/// The script reads a JSON line from stdin, extracts the id, and
/// writes back a JSON-RPC response with `{"ok": true}` as the result.
async fn spawn_echo_server() -> StdioTransport {
    StdioTransport::spawn(
        "sh",
        &[
            "-c".to_string(),
            // Read lines, parse id with sed, echo a response.
            r#"while IFS= read -r line; do
                id=$(echo "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
                echo "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"ok\":true}}"
            done"#
                .to_string(),
        ],
        &HashMap::new(),
        false,
        false,
    )
    .await
    .expect("failed to spawn echo server")
}

#[test]
fn classify_inbound_distinguishes_message_shapes() {
    // Response: no method, id present.
    let resp = serde_json::json!({"jsonrpc":"2.0","id":1,"result":{"ok":true}});
    assert!(matches!(classify_inbound(&resp), Inbound::Response));

    // Request: method + non-null id (string id is legal JSON-RPC).
    let req = serde_json::json!({"jsonrpc":"2.0","id":"abc","method":"roots/list"});
    match classify_inbound(&req) {
        Inbound::Request { id, method, .. } => {
            assert_eq!(id, serde_json::json!("abc"));
            assert_eq!(method, "roots/list");
        }
        _ => panic!("expected request"),
    }

    // Notification: method, no id.
    let note = serde_json::json!({"jsonrpc":"2.0","method":"notifications/message","params":{"level":"info"}});
    match classify_inbound(&note) {
        Inbound::Notification { method, params } => {
            assert_eq!(method, "notifications/message");
            assert_eq!(params.unwrap()["level"], "info");
        }
        _ => panic!("expected notification"),
    }

    // Null id + method is a notification, not a request (JSON-RPC
    // forbids null request ids).
    let null_id = serde_json::json!({"jsonrpc":"2.0","id":null,"method":"x"});
    assert!(matches!(
        classify_inbound(&null_id),
        Inbound::Notification { .. }
    ));
}

#[test]
fn build_response_line_echoes_id_and_shapes_result_or_error() {
    let ok = build_response_line(
        &serde_json::json!("r1"),
        Ok(serde_json::json!({"roots": []})),
    );
    let v: serde_json::Value = serde_json::from_str(&ok).unwrap();
    assert_eq!(v["id"], "r1");
    assert_eq!(v["result"]["roots"], serde_json::json!([]));
    assert!(v.get("error").is_none());

    let err = build_response_line(
        &serde_json::json!(7),
        Err(JsonRpcError {
            code: -32601,
            message: "nope".into(),
            data: None,
        }),
    );
    let v: serde_json::Value = serde_json::from_str(&err).unwrap();
    assert_eq!(v["id"], 7);
    assert_eq!(v["error"]["code"], -32601);
    assert_eq!(v["error"]["message"], "nope");
}

/// Records inbound dispatch for assertions and answers `roots/list`
/// with a sentinel so the full request round-trip is observable.
struct RecordingHandler {
    notes: tokio::sync::mpsc::UnboundedSender<(String, Option<Value>)>,
}

#[async_trait]
impl InboundHandler for RecordingHandler {
    async fn handle_request(
        &self,
        method: &str,
        _params: Option<Value>,
    ) -> std::result::Result<Value, JsonRpcError> {
        assert_eq!(method, "roots/list");
        Ok(serde_json::json!({ "roots": ["sentinel"] }))
    }

    async fn handle_notification(&self, method: &str, params: Option<Value>) {
        let _ = self.notes.send((method.to_string(), params));
    }
}

#[tokio::test]
async fn inbound_request_is_dispatched_and_response_written_back() {
    // Server waits for a kick, emits a server-originated `roots/list`
    // request, reads our response, then echoes it back wrapped in a
    // notification so the test can confirm the full round-trip:
    // dispatch -> handler -> response written to stdin -> server saw it.
    let transport = StdioTransport::spawn(
        "sh",
        &[
            "-c".to_string(),
            r#"IFS= read -r _go
               echo '{"jsonrpc":"2.0","id":"r1","method":"roots/list"}'
               IFS= read -r resp
               printf '{"jsonrpc":"2.0","method":"notifications/echo","params":%s}\n' "$resp"
               cat >/dev/null"#
                .to_string(),
        ],
        &HashMap::new(),
        false,
        false,
    )
    .await
    .expect("spawn inbound test server");

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    transport.set_inbound_handler(Arc::new(RecordingHandler { notes: tx }));

    // Kick the server (installs the handler first to avoid the race
    // where the default handler answers before ours is in place).
    transport.send_notification("go", None).await.unwrap();

    let (method, params) = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
        .await
        .expect("echo notification within timeout")
        .expect("channel open");

    assert_eq!(method, "notifications/echo");
    // params is the response object the server received from us.
    let echoed = params.expect("echo params");
    assert_eq!(echoed["id"], "r1");
    assert_eq!(echoed["result"]["roots"][0], "sentinel");
}

#[tokio::test]
async fn send_request_receives_matching_response() {
    let transport = spawn_echo_server().await;
    let result = transport.send_request("test/method", None).await.unwrap();
    assert_eq!(result, serde_json::json!({"ok": true}));
}

#[tokio::test]
async fn stdio_server_gets_allowlisted_env_only() {
    // SEC-12: an MCP server must not inherit Dyson's whole
    // environment (provider keys, swarm tokens).  Cargo sets
    // CARGO_MANIFEST_DIR for the test process — not on the
    // allowlist, so it stands in for a secret here.
    assert!(std::env::var_os("CARGO_MANIFEST_DIR").is_some());
    let mut env = HashMap::new();
    env.insert("MY_CFG".to_string(), "configured".to_string());
    let transport = StdioTransport::spawn(
        "sh",
        &[
            "-c".to_string(),
            r#"while IFS= read -r line; do
                id=$(echo "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
                echo "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"leak\":\"${CARGO_MANIFEST_DIR:+leaked}\",\"cfg\":\"${MY_CFG:-}\",\"path\":\"${PATH:+set}\"}}"
            done"#
                .to_string(),
        ],
        &env,
        false,
        false,
    )
    .await
    .unwrap();
    let result = transport.send_request("env", None).await.unwrap();
    assert_eq!(result["leak"], "", "parent env leaked: {result}");
    assert_eq!(result["cfg"], "configured");
    assert_eq!(result["path"], "set");
}

#[tokio::test]
async fn stdio_failed_send_leaves_no_pending_entry() {
    // MEM-09: an oversized request bailed out via `?` after the
    // pending entry was registered, leaking it forever.
    let transport = spawn_echo_server().await;
    let huge = "x".repeat(MAX_MCP_REQUEST_BYTES + 1);
    let err = transport
        .send_request("tools/call", Some(serde_json::json!({ "blob": huge })))
        .await
        .expect_err("oversized request must fail");
    assert!(err.to_string().contains("too large"), "{err}");
    assert!(
        transport.pending.lock().await.is_empty(),
        "pending entry leaked"
    );
}

#[tokio::test]
async fn stdio_server_exit_fails_pending_requests_promptly() {
    // MEM-09: when the server exits without answering, waiting
    // callers used to sit on their pending entry for the full 120 s
    // response timeout.
    let transport = StdioTransport::spawn(
        "sh",
        &["-c".to_string(), "read line; exit 0".to_string()],
        &HashMap::new(),
        false,
        false,
    )
    .await
    .unwrap();
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        transport.send_request("tools/list", None),
    )
    .await
    .expect("send_request should fail promptly once the server exits");
    assert!(result.is_err());
    assert!(transport.pending.lock().await.is_empty());
}

#[tokio::test]
async fn stdio_stdout_frame_above_result_cap_fails_explicitly() {
    let mut reader = BufReader::new(tokio::io::repeat(b'a'));
    let err = read_line_capped(&mut reader, MAX_MCP_RESULT_BYTES)
        .await
        .expect_err("line above cap should fail");

    assert_eq!(err.to_string(), MCP_RESULT_TOO_LARGE_MESSAGE);
}

#[tokio::test]
async fn http_response_above_result_cap_fails_explicitly() {
    use std::convert::Infallible;

    use http_body_util::StreamBody;
    use hyper::body::{Bytes, Frame, Incoming};
    use hyper::service::service_fn;
    use hyper::{Request, Response};
    use hyper_util::rt::TokioIo;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind test server");
    let addr = listener.local_addr().expect("local addr");
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept connection");
        let io = TokioIo::new(stream);
        let service = service_fn(|_req: Request<Incoming>| async move {
            let body_stream = async_stream::stream! {
                for _ in 0..=MAX_MCP_RESULT_BYTES / MIB {
                    yield Ok::<_, Infallible>(Frame::data(Bytes::from(vec![b'a'; MIB])));
                }
            };
            Ok::<_, Infallible>(
                Response::builder()
                    .header("content-type", "application/json")
                    .body(StreamBody::new(body_stream))
                    .expect("response"),
            )
        });

        let _ = hyper::server::conn::http1::Builder::new()
            .serve_connection(io, service)
            .await;
    });

    let transport = HttpTransport::new(
        &format!("http://{addr}/mcp"),
        Box::new(crate::auth::DangerousNoAuth),
    );
    let err = transport
        .send_request("tools/call", Some(serde_json::json!({})))
        .await
        .expect_err("oversized HTTP MCP response should fail");
    server.abort();

    assert!(
        err.to_string().contains(MCP_RESULT_TOO_LARGE_MESSAGE),
        "unexpected error: {err}"
    );
}

#[tokio::test]
async fn http_transport_does_not_follow_redirects() {
    // SEC-22: auth headers (custom API-key headers included) must not
    // be replayed to wherever an MCP server redirects us.  Locally the
    // shared redirect policy already refuses `localhost`, so this pins
    // the behaviour rather than reproducing a public-host redirect.
    use std::convert::Infallible;
    use std::sync::atomic::AtomicUsize;

    use http_body_util::Full;
    use hyper::body::{Bytes, Incoming};
    use hyper::service::service_fn;
    use hyper::{Request, Response};
    use hyper_util::rt::TokioIo;

    let target_hits = Arc::new(AtomicUsize::new(0));
    let target = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let target_port = target.local_addr().unwrap().port();
    let hits = Arc::clone(&target_hits);
    let target_task = tokio::spawn(async move {
        while let Ok((stream, _)) = target.accept().await {
            let hits = Arc::clone(&hits);
            let service = service_fn(move |_req: Request<Incoming>| {
                hits.fetch_add(1, Ordering::SeqCst);
                async move {
                    Ok::<_, Infallible>(Response::new(Full::new(Bytes::from_static(
                        br#"{"jsonrpc":"2.0","id":1,"result":{}}"#,
                    ))))
                }
            });
            let _ = hyper::server::conn::http1::Builder::new()
                .serve_connection(TokioIo::new(stream), service)
                .await;
        }
    });

    let origin = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin_addr = origin.local_addr().unwrap();
    let origin_task = tokio::spawn(async move {
        while let Ok((stream, _)) = origin.accept().await {
            let service = service_fn(move |_req: Request<Incoming>| async move {
                Ok::<_, Infallible>(
                    Response::builder()
                        .status(307)
                        // A hostname, so the private-IP redirect filter
                        // does not catch it on its own.
                        .header("location", format!("http://localhost:{target_port}/mcp"))
                        .body(Full::new(Bytes::new()))
                        .unwrap(),
                )
            });
            let _ = hyper::server::conn::http1::Builder::new()
                .serve_connection(TokioIo::new(stream), service)
                .await;
        }
    });

    let transport = HttpTransport::new(
        &format!("http://{origin_addr}/mcp"),
        Box::new(crate::auth::DangerousNoAuth),
    );
    let result = transport.send_request("tools/list", None).await;
    origin_task.abort();
    target_task.abort();
    assert!(result.is_err(), "redirect response should not succeed");
    assert_eq!(
        target_hits.load(Ordering::SeqCst),
        0,
        "redirect was followed"
    );
}

#[test]
fn extract_all_sse_frames_returns_every_frame_in_order() {
    let body = "event: message\ndata: {\"a\":1}\n\nevent: message\ndata: {\"b\":2}\n\n";
    let frames = extract_all_sse_frames(body);
    assert_eq!(
        frames,
        vec!["{\"a\":1}".to_string(), "{\"b\":2}".to_string()]
    );
}

#[tokio::test]
async fn http_sse_streams_inbound_then_unblocks_on_answer_post() {
    // End-to-end streaming regression: the server emits an elicit
    // frame, then *waits* for the dyson client to POST the answer
    // on a separate request, then emits the tools/call response
    // frame.  If `parse_rpc_response` ever buffers the SSE body
    // before dispatching, this test deadlocks — the server is
    // waiting on an answer that can't arrive until dispatch fires.
    //
    // Captures the wire-shape contract we depend on for the
    // swarm-proxied stdio elicit flow.
    use std::convert::Infallible;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use http_body_util::StreamBody;
    use http_body_util::combinators::BoxBody;
    use hyper::body::{Bytes, Frame, Incoming};
    use hyper::service::service_fn;
    use hyper::{Request, Response};
    use hyper_util::rt::TokioIo;

    static ANSWER_SEEN: AtomicBool = AtomicBool::new(false);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind test server");
    let addr = listener.local_addr().expect("local addr");
    let server = tokio::spawn(async move {
        // Two connections expected: the streaming tools/call POST,
        // and the inbound answer POST.  Both are loopback so they
        // complete fast; the order is determined by the client's
        // dispatch behaviour, which is what we're asserting.
        for _ in 0..2 {
            let (stream, _) = listener.accept().await.expect("accept");
            let io = TokioIo::new(stream);
            tokio::spawn(async move {
                let svc = service_fn(|req: Request<Incoming>| async move {
                    // POST body tells us which call this is.
                    let body_bytes = http_body_util::BodyExt::collect(req.into_body())
                        .await
                        .unwrap()
                        .to_bytes();
                    let req_value: serde_json::Value =
                        serde_json::from_slice(&body_bytes).unwrap_or(serde_json::Value::Null);
                    // The inbound answer is a JSON-RPC response —
                    // id present, no method.
                    let is_inbound_answer =
                        req_value.get("method").is_none() && req_value.get("id").is_some();
                    // Single stream type for both branches so
                    // their `Response<...>` types unify.
                    let body_stream = async_stream::stream! {
                        if is_inbound_answer {
                            ANSWER_SEEN.store(true, Ordering::SeqCst);
                        } else {
                            yield Ok::<_, Infallible>(Frame::data(Bytes::from(
                                "event: message\n\
                                 data: {\"jsonrpc\":\"2.0\",\"id\":\"e1\",\"method\":\"elicitation/create\",\"params\":{\"message\":\"go?\",\"requestedSchema\":{}}}\n\n",
                            )));
                            // Wait for the answer.  Polling is fine
                            // here — the dispatch is on a different
                            // connection so we won't deadlock.
                            for _ in 0..200 {
                                if ANSWER_SEEN.load(Ordering::SeqCst) {
                                    break;
                                }
                                tokio::time::sleep(Duration::from_millis(25)).await;
                            }
                            yield Ok(Frame::data(Bytes::from(
                                "event: message\n\
                                 data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":true}}\n\n",
                            )));
                        }
                    };
                    let body = BoxBody::new(StreamBody::new(body_stream));
                    let status = if is_inbound_answer { 202 } else { 200 };
                    let mut builder = Response::builder().status(status);
                    if !is_inbound_answer {
                        builder = builder.header("content-type", "text/event-stream");
                    }
                    Ok::<_, Infallible>(builder.body(body).expect("response"))
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(io, svc)
                    .await;
            });
        }
    });

    struct ElicitHandler;
    #[async_trait]
    impl InboundHandler for ElicitHandler {
        async fn handle_request(
            &self,
            method: &str,
            _params: Option<Value>,
        ) -> std::result::Result<Value, JsonRpcError> {
            assert_eq!(method, "elicitation/create");
            Ok(serde_json::json!({ "action": "accept", "content": {} }))
        }
        async fn handle_notification(&self, _method: &str, _params: Option<Value>) {}
    }

    let transport = HttpTransport::new(
        &format!("http://{addr}/mcp"),
        Box::new(crate::auth::DangerousNoAuth),
    );
    transport.set_inbound_handler(Arc::new(ElicitHandler));

    let result = transport
        .send_request("tools/call", Some(serde_json::json!({})))
        .await
        .expect("streaming request resolves once response frame arrives");
    server.abort();

    assert_eq!(result, serde_json::json!({ "ok": true }));
    assert!(
        ANSWER_SEEN.load(Ordering::SeqCst),
        "inbound answer must have been POSTed before the body closed"
    );
}

#[tokio::test]
async fn http_sse_demux_dispatches_inbound_and_returns_response() {
    use std::convert::Infallible;

    use http_body_util::Full;
    use hyper::body::{Bytes, Incoming};
    use hyper::service::service_fn;
    use hyper::{Request, Response};
    use hyper_util::rt::TokioIo;

    // SSE body: a server-originated notification, then the response to
    // our outbound request (id 1).  The demux must surface the
    // notification to the handler and return {"ok":true} as the result.
    let sse = "event: message\n\
               data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/message\",\"params\":{\"level\":\"info\",\"data\":\"hi\"}}\n\n\
               event: message\n\
               data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":true}}\n\n";

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind test server");
    let addr = listener.local_addr().expect("local addr");
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept connection");
        let io = TokioIo::new(stream);
        let service = service_fn(move |_req: Request<Incoming>| async move {
            Ok::<_, Infallible>(
                Response::builder()
                    .header("content-type", "text/event-stream")
                    .body(Full::new(Bytes::from(sse)))
                    .expect("response"),
            )
        });
        let _ = hyper::server::conn::http1::Builder::new()
            .serve_connection(io, service)
            .await;
    });

    let transport = HttpTransport::new(
        &format!("http://{addr}/mcp"),
        Box::new(crate::auth::DangerousNoAuth),
    );
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    transport.set_inbound_handler(Arc::new(RecordingHandler { notes: tx }));

    let result = transport
        .send_request("tools/call", Some(serde_json::json!({})))
        .await
        .expect("request should resolve to the response frame");
    server.abort();

    assert_eq!(result, serde_json::json!({ "ok": true }));
    let (method, params) = rx.try_recv().expect("inbound notification dispatched");
    assert_eq!(method, "notifications/message");
    assert_eq!(params.unwrap()["data"], "hi");
}

#[test]
fn extract_last_sse_data_handles_single_frame() {
    // The shape Context7 (and most streamable HTTP MCP servers)
    // emit for initialize: one event with one data line, blank
    // line terminator.
    let body = "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":true}}\n\n";
    let extracted = extract_last_sse_data(body).unwrap();
    let v: serde_json::Value = serde_json::from_str(&extracted).unwrap();
    assert_eq!(v["result"]["ok"], true);
}

#[test]
fn extract_last_sse_data_takes_final_event() {
    // Defense for servers that ship a status frame before the
    // response — we want the LAST `data:`, not the first.
    let body = "event: message\ndata: {\"status\":\"in_progress\"}\n\nevent: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":42}\n\n";
    let extracted = extract_last_sse_data(body).unwrap();
    let v: serde_json::Value = serde_json::from_str(&extracted).unwrap();
    assert_eq!(v["result"], 42);
}

#[test]
fn extract_last_sse_data_concatenates_multiline_data_per_spec() {
    // SSE spec: multiple `data:` lines in one event get joined
    // with `\n`.  Real-world: rare for JSON payloads but valid
    // and we should not silently drop the second half.
    let body = "data: line one\ndata: line two\n\n";
    assert_eq!(extract_last_sse_data(body).unwrap(), "line one\nline two");
}

#[test]
fn extract_last_sse_data_tolerates_missing_terminator() {
    // Some servers omit the trailing blank line on the final
    // event — we still need to surface that data.
    let body = "data: {\"a\":1}";
    assert_eq!(extract_last_sse_data(body).unwrap(), "{\"a\":1}");
}

#[test]
fn extract_last_sse_data_returns_none_on_no_data_line() {
    // If the body is somehow empty or contains only `event:`
    // lines, we should return None so the caller surfaces a
    // useful error rather than panicking on parse.
    assert!(extract_last_sse_data("").is_none());
    assert!(extract_last_sse_data(":heartbeat\n\n").is_none());
}

#[tokio::test]
async fn sequential_requests_match_by_id() {
    let transport = spawn_echo_server().await;

    let r1 = transport.send_request("method/one", None).await.unwrap();
    let r2 = transport.send_request("method/two", None).await.unwrap();

    assert_eq!(r1, serde_json::json!({"ok": true}));
    assert_eq!(r2, serde_json::json!({"ok": true}));
}

#[tokio::test]
async fn request_timeout_on_dead_process() {
    // Spawn a process that immediately exits without responding.
    let transport = StdioTransport::spawn(
        "sh",
        &["-c".to_string(), "exit 0".to_string()],
        &HashMap::new(),
        false,
        false,
    )
    .await
    .expect("failed to spawn");

    // Give the process a moment to exit.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let err = transport.send_request("test", None).await;
    assert!(err.is_err(), "should fail when process dies");
    let msg = err.unwrap_err().to_string();
    assert!(
        msg.contains("closed") || msg.contains("timed out") || msg.contains("failed"),
        "error should indicate connection loss: {msg}"
    );
}

#[tokio::test]
async fn rpc_error_is_propagated() {
    // Server returns a JSON-RPC error.
    let transport = StdioTransport::spawn(
        "sh",
        &[
            "-c".to_string(),
            r#"while IFS= read -r line; do
                id=$(echo "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
                echo "{\"jsonrpc\":\"2.0\",\"id\":$id,\"error\":{\"code\":-32601,\"message\":\"method not found\"}}"
            done"#
                .to_string(),
        ],
        &HashMap::new(),
        false,
        false,
    )
    .await
    .expect("failed to spawn error server");

    let err = transport.send_request("nonexistent", None).await;
    assert!(err.is_err());
    let msg = err.unwrap_err().to_string();
    assert!(msg.contains("method not found"));
}

#[tokio::test]
async fn drop_aborts_reader_task() {
    let transport = spawn_echo_server().await;
    // Just verify that dropping doesn't panic.
    drop(transport);
    // Give tokio a moment to clean up.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
}

#[tokio::test]
async fn spawn_nonexistent_command_fails() {
    let err = StdioTransport::spawn(
        "nonexistent-command-xyz",
        &[],
        &HashMap::new(),
        false,
        false,
    )
    .await;
    assert!(err.is_err());
}

// M7: when sandbox is required but the platform/PATH cannot
// provide a wrapper (bwrap missing on Linux, no wrapper on
// non-Linux), resolve_exec must refuse rather than silently
// falling back to a direct unsandboxed spawn.
#[test]
fn resolve_exec_refuses_silent_fallback_when_bwrap_missing() {
    let err =
        StdioTransport::resolve_exec_with("uvx", &["mcp-test".to_string()], true, false, false);
    assert!(
        err.is_err(),
        "must refuse when sandbox requested and no bwrap"
    );
    let msg = format!("{}", err.unwrap_err());
    assert!(
        msg.to_lowercase().contains("sandbox") || msg.to_lowercase().contains("bwrap"),
        "error must explain why: {msg}"
    );
}

#[test]
fn resolve_exec_wraps_with_bwrap_when_available() {
    let (exec, argv) =
        StdioTransport::resolve_exec_with("uvx", &["mcp-test".to_string()], true, true, false)
            .expect("bwrap-available path");
    assert_eq!(exec, "bwrap");
    assert!(argv.iter().any(|a| a == "uvx"), "uvx must appear in argv");
}

#[test]
fn resolve_exec_passthrough_when_sandbox_disabled() {
    let (exec, argv) =
        StdioTransport::resolve_exec_with("uvx", &["mcp-test".to_string()], false, false, false)
            .expect("passthrough path");
    assert_eq!(exec, "uvx");
    assert_eq!(argv, vec!["mcp-test".to_string()]);
}
