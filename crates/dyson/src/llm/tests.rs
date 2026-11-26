use super::*;

// -----------------------------------------------------------------------
// format_prompt tests
// -----------------------------------------------------------------------

#[test]
fn single_message_is_just_text() {
    let messages = vec![Message::user("hello")];
    let prompt = format_prompt(&messages, &[]);
    assert_eq!(prompt, "hello");
}

#[test]
fn multi_turn_includes_history() {
    let messages = vec![
        Message::user("what files?"),
        Message::assistant(vec![ContentBlock::Text {
            text: "Let me check.".into(),
        }]),
        Message::user("thanks"),
    ];
    let prompt = format_prompt(&messages, &[]);
    assert!(prompt.contains("[Previous conversation:]"));
    assert!(prompt.contains("User: what files?"));
    assert!(prompt.contains("Assistant: Let me check."));
    assert!(prompt.contains("[Current message:]"));
    assert!(prompt.contains("thanks"));
}

#[test]
fn tool_results_in_history() {
    let messages = vec![
        Message::user("list files"),
        Message::assistant(vec![ContentBlock::ToolUse {
            id: "call_1".into(),
            name: "bash".into(),
            input: serde_json::json!({"command": "ls"}),
        }]),
        Message::tool_result("call_1", "Cargo.toml\nsrc/", false),
        Message::user("nice"),
    ];
    let prompt = format_prompt(&messages, &[]);
    assert!(prompt.contains("[Used tool: bash"));
    assert!(prompt.contains("Tool result: Cargo.toml"));
}

#[test]
fn tools_appended_to_prompt() {
    let messages = vec![Message::user("help")];
    let tool = ToolDefinition {
        name: "bash".into(),
        description: "Run commands".into(),
        input_schema: serde_json::json!({"type": "object"}),
        agent_only: false,
    };
    let prompt = format_prompt(&messages, &[&tool]);
    assert!(prompt.contains("[Available tools:]"));
    assert!(prompt.contains("**bash**"));
}

// -----------------------------------------------------------------------
// SseLineBuffer tests
// -----------------------------------------------------------------------

#[test]
fn sse_line_buffer_extracts_data_payloads() {
    let mut buf = SseLineBuffer::new();
    let payloads = buf.feed(b"data: {\"type\":\"text\"}\n\n").unwrap();
    assert_eq!(payloads.len(), 1);
    assert_eq!(payloads[0], "{\"type\":\"text\"}");
}

#[test]
fn sse_line_buffer_skips_comments_and_events() {
    let mut buf = SseLineBuffer::new();
    let payloads = buf
        .feed(b": this is a comment\nevent: message_start\ndata: {\"ok\":true}\n\n")
        .unwrap();
    assert_eq!(payloads.len(), 1);
    assert_eq!(payloads[0], "{\"ok\":true}");
}

#[test]
fn sse_line_buffer_handles_partial_lines() {
    let mut buf = SseLineBuffer::new();

    // First chunk: incomplete line.
    let p1 = buf.feed(b"data: {\"part").unwrap();
    assert!(p1.is_empty());

    // Second chunk: completes the line.
    let p2 = buf.feed(b"ial\":true}\n\n").unwrap();
    assert_eq!(p2.len(), 1);
    assert_eq!(p2[0], "{\"partial\":true}");
}

#[test]
fn sse_line_buffer_returns_done_sentinel() {
    let mut buf = SseLineBuffer::new();
    let payloads = buf.feed(b"data: [DONE]\n\n").unwrap();
    assert_eq!(payloads.len(), 1);
    assert_eq!(payloads[0], "[DONE]");
}

#[test]
fn sse_line_buffer_preserves_multibyte_utf8_across_chunks() {
    let mut buf = SseLineBuffer::new();

    // "├──" is three box-drawing characters, each 3 bytes in UTF-8.
    // U+251C (├) = E2 94 9C, U+2500 (─) = E2 94 80
    // Split the data line so the chunk boundary falls mid-character.
    let full = "data: {\"text\":\"├──\"}\n";
    let bytes = full.as_bytes();

    // Split at byte 17: bytes 15-17 are E2 94 9C (├).
    // split_at(17) puts [..17] (ending with 0x94) in chunk1, and
    // [17..] (starting with 0x9C) in chunk2, splitting mid-character.
    let (chunk1, chunk2) = bytes.split_at(17);
    assert!(
        chunk1.last().copied() == Some(0x94),
        "sanity: split should land mid-character"
    );

    let p1 = buf.feed(chunk1).unwrap();
    assert!(p1.is_empty(), "first chunk has no complete line");

    let p2 = buf.feed(chunk2).unwrap();
    assert_eq!(p2.len(), 1);
    assert_eq!(p2[0], "{\"text\":\"├──\"}");
}

// Regression: a line containing invalid UTF-8 used to be replaced
// with "" — the whole SSE payload silently vanished.  Decode must
// be lossy (replacement char) so the payload survives.
#[test]
fn sse_line_buffer_keeps_invalid_utf8_payloads_lossily() {
    let mut buf = SseLineBuffer::new();
    let mut bytes = b"data: {\"text\":\"a".to_vec();
    bytes.push(0xFF); // invalid UTF-8 byte mid-payload
    bytes.extend_from_slice(b"b\"}\n");

    let payloads = buf.feed(&bytes).unwrap();
    assert_eq!(
        payloads.len(),
        1,
        "an invalid byte must not drop the whole payload"
    );
    assert!(
        payloads[0].starts_with("{\"text\":\"a"),
        "payload prefix must survive: {:?}",
        payloads[0]
    );
    assert!(
        payloads[0].contains('\u{FFFD}'),
        "invalid byte should decode to the replacement char: {:?}",
        payloads[0]
    );
    assert!(payloads[0].ends_with("b\"}"), "payload tail must survive");
}

#[test]
fn sse_line_buffer_rejects_oversized_input() {
    let mut buf = SseLineBuffer::new();
    let chunk = vec![b'x'; 10 * 1024 * 1024 + 1];
    let result = buf.feed(&chunk);
    assert!(result.is_err());
    let err_msg = format!("{}", result.unwrap_err());
    assert!(err_msg.contains("10 MB"));
}

// -----------------------------------------------------------------------
// finalize_tool_call tests
// -----------------------------------------------------------------------

#[test]
fn finalize_valid_json() {
    let buf = ToolCallBuffer {
        id: "call_1".into(),
        name: "bash".into(),
        json: r#"{"command":"ls"}"#.into(),
    };
    let event = finalize_tool_call(buf).unwrap();
    match event {
        stream::StreamEvent::ToolUseComplete { id, name, input } => {
            assert_eq!(id, "call_1");
            assert_eq!(name, "bash");
            assert_eq!(input["command"], "ls");
        }
        other => panic!("expected ToolUseComplete, got: {other:?}"),
    }
}

#[test]
fn finalize_malformed_json_includes_parse_error() {
    let buf = ToolCallBuffer {
        id: "call_2".into(),
        name: "bash".into(),
        json: "{broken json".into(),
    };
    let event = finalize_tool_call(buf).unwrap();
    match event {
        stream::StreamEvent::ToolUseComplete { input, .. } => {
            // Should contain the parse error info, not an empty object.
            assert!(
                input["_parse_error"]
                    .as_str()
                    .unwrap()
                    .contains("malformed")
            );
            assert_eq!(input["_raw_json"], "{broken json");
        }
        other => panic!("expected ToolUseComplete, got: {other:?}"),
    }
}

#[test]
fn finalize_empty_json_includes_parse_error() {
    let buf = ToolCallBuffer {
        id: "call_3".into(),
        name: "bash".into(),
        json: String::new(),
    };
    let event = finalize_tool_call(buf).unwrap();
    match event {
        stream::StreamEvent::ToolUseComplete { input, .. } => {
            assert!(input.get("_parse_error").is_some());
        }
        other => panic!("expected ToolUseComplete, got: {other:?}"),
    }
}

// -----------------------------------------------------------------------
// RetryingLlmClient tests
// -----------------------------------------------------------------------

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Test-only client that returns a prepared sequence of `Result`s on each
/// `stream()` call.  Counts attempts so tests can assert retry behavior.
/// `attempts` is an `Arc<AtomicUsize>` so the test can observe the count
/// after the client is moved into `RetryingLlmClient`.
struct ScriptedClient {
    results: std::sync::Mutex<Vec<Result<()>>>,
    attempts: Arc<AtomicUsize>,
}

impl ScriptedClient {
    fn new(results: Vec<Result<()>>) -> (Self, Arc<AtomicUsize>) {
        let attempts = Arc::new(AtomicUsize::new(0));
        let client = Self {
            results: std::sync::Mutex::new(results),
            attempts: Arc::clone(&attempts),
        };
        (client, attempts)
    }
}

#[async_trait]
impl LlmClient for ScriptedClient {
    async fn stream(
        &self,
        _messages: &[Message],
        _system: &str,
        _system_suffix: &str,
        _tools: &[ToolDefinition],
        _tool_instances: &std::collections::HashMap<String, std::sync::Arc<dyn Tool>>,
        _config: &CompletionConfig,
    ) -> Result<StreamResponse> {
        self.attempts.fetch_add(1, Ordering::SeqCst);
        let next = self.results.lock().unwrap().remove(0);
        next.map(|()| StreamResponse {
            stream: Box::pin(tokio_stream::iter(std::iter::empty())),
            tool_mode: ToolMode::Execute,
            input_tokens: None,
            swarm_llm_audit_id: None,
            provider: None,
            model: None,
        })
    }
}

fn empty_config() -> CompletionConfig {
    CompletionConfig {
        model: "test".into(),
        max_tokens: 1,
        temperature: None,
        api_tool_injections: vec![],
    }
}

fn assert_err(r: Result<StreamResponse>) -> DysonError {
    match r {
        Ok(_) => panic!("expected Err, got Ok"),
        Err(e) => e,
    }
}

fn rate_limit_err(retry_after: Option<std::time::Duration>) -> DysonError {
    DysonError::LlmRateLimit {
        message: "429".into(),
        retry_after,
    }
}

fn overloaded_err(retry_after: Option<std::time::Duration>) -> DysonError {
    DysonError::LlmOverloaded {
        message: "529".into(),
        retry_after,
    }
}

#[tokio::test]
async fn retries_rate_limit_then_succeeds() {
    let (scripted, attempts) = ScriptedClient::new(vec![
        Err(rate_limit_err(None)),
        Err(rate_limit_err(None)),
        Ok(()),
    ]);
    let client = RetryingLlmClient::with_base_delay(Box::new(scripted), 3, 1);

    let result = client
        .stream(
            &[],
            "",
            "",
            &[],
            &std::collections::HashMap::new(),
            &empty_config(),
        )
        .await;
    assert!(result.is_ok());
    assert_eq!(attempts.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn gives_up_after_max_retries() {
    let (scripted, attempts) = ScriptedClient::new(vec![
        Err(rate_limit_err(None)),
        Err(rate_limit_err(None)),
        Err(rate_limit_err(None)),
    ]);
    let client = RetryingLlmClient::with_base_delay(Box::new(scripted), 2, 1);

    let err = assert_err(
        client
            .stream(
                &[],
                "",
                "",
                &[],
                &std::collections::HashMap::new(),
                &empty_config(),
            )
            .await,
    );
    assert!(matches!(err, DysonError::LlmRateLimit { .. }));
    // initial + 2 retries = 3 attempts.
    assert_eq!(attempts.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn non_retryable_error_is_not_retried() {
    let (scripted, attempts) =
        ScriptedClient::new(vec![Err(DysonError::Llm("auth failed".into()))]);
    let client = RetryingLlmClient::with_base_delay(Box::new(scripted), 5, 1);

    let err = assert_err(
        client
            .stream(
                &[],
                "",
                "",
                &[],
                &std::collections::HashMap::new(),
                &empty_config(),
            )
            .await,
    );
    assert!(matches!(err, DysonError::Llm(_)));
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn zero_max_retries_disables_retry() {
    let (scripted, attempts) = ScriptedClient::new(vec![Err(overloaded_err(None))]);
    let client = RetryingLlmClient::with_base_delay(Box::new(scripted), 0, 1);

    let err = assert_err(
        client
            .stream(
                &[],
                "",
                "",
                &[],
                &std::collections::HashMap::new(),
                &empty_config(),
            )
            .await,
    );
    assert!(matches!(err, DysonError::LlmOverloaded { .. }));
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
}

#[test]
fn parse_retry_hint_reads_retry_after_seconds() {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(reqwest::header::RETRY_AFTER, "30".parse().unwrap());
    let hint = parse_retry_hint(&headers).expect("expected a hint");
    assert_eq!(hint, std::time::Duration::from_secs(30));
}

#[test]
fn parse_retry_hint_reads_x_ratelimit_reset_seconds() {
    use std::time::{SystemTime, UNIX_EPOCH};
    let target = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 45;
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert("x-ratelimit-reset", target.to_string().parse().unwrap());
    let hint = parse_retry_hint(&headers).expect("expected a hint");
    // Allow 2s slack for clock drift between insertion and the reread.
    assert!(
        hint.as_secs() >= 43 && hint.as_secs() <= 45,
        "hint was {hint:?}"
    );
}

#[test]
fn parse_retry_hint_handles_x_ratelimit_reset_milliseconds() {
    use std::time::{SystemTime, UNIX_EPOCH};
    let target_ms = (SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 60)
        * 1000;
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert("x-ratelimit-reset", target_ms.to_string().parse().unwrap());
    let hint = parse_retry_hint(&headers).expect("expected a hint");
    assert!(
        hint.as_secs() >= 58 && hint.as_secs() <= 60,
        "hint was {hint:?}"
    );
}

#[test]
fn parse_retry_hint_returns_none_when_absent() {
    let headers = reqwest::header::HeaderMap::new();
    assert!(parse_retry_hint(&headers).is_none());
}

#[test]
fn provider_http_error_routes_429_to_rate_limit() {
    let err = provider_http_error(
        "OpenAI",
        reqwest::StatusCode::TOO_MANY_REQUESTS,
        Some(std::time::Duration::from_secs(7)),
        "you are being rate limited",
    );
    match err {
        DysonError::LlmRateLimit {
            message,
            retry_after,
        } => {
            assert_eq!(
                message,
                "OpenAI (429 Too Many Requests): you are being rate limited"
            );
            assert_eq!(retry_after, Some(std::time::Duration::from_secs(7)));
        }
        other => panic!("expected LlmRateLimit, got {other:?}"),
    }
}

#[test]
fn provider_http_error_routes_5xx_to_overloaded() {
    for code in [502u16, 503, 529] {
        let status = reqwest::StatusCode::from_u16(code).unwrap();
        let err = provider_http_error("Anthropic", status, None, "service unavailable");
        assert!(
            matches!(err, DysonError::LlmOverloaded { .. }),
            "status {code} should be LlmOverloaded, got {err:?}"
        );
    }
}

#[test]
fn provider_http_error_routes_402_to_llm() {
    let err = provider_http_error(
        "OpenRouter",
        reqwest::StatusCode::PAYMENT_REQUIRED,
        None,
        "Prompt tokens limit exceeded — add credits",
    );
    match err {
        DysonError::Llm(message) => {
            assert_eq!(
                message,
                "OpenRouter (402 Payment Required): Prompt tokens limit exceeded — add credits"
            );
        }
        other => panic!("expected Llm, got {other:?}"),
    }
}

#[test]
fn parse_provider_error_body_strips_openrouter_402_envelope() {
    // OpenRouter credit-exhaustion body — sibling fields like `metadata`
    // and `user_id` must not leak.
    let body = r#"{"error":{"message":"Prompt tokens limit exceeded: 163586 > 132288. To increase, visit https://openrouter.ai/settings/credits and add more credits","code":402,"metadata":{"provider_name":null}},"user_id":"user_3BGgMlV4QdIpRqJBHJukfpbymrP"}"#;
    let msg = parse_provider_error_body(body);
    assert_eq!(
        msg,
        "Prompt tokens limit exceeded: 163586 > 132288. To increase, visit https://openrouter.ai/settings/credits and add more credits"
    );
    assert!(!msg.contains("user_id"));
    assert!(!msg.contains('{'));
}

#[test]
fn parse_provider_error_body_strips_openai_insufficient_quota_envelope() {
    let body = r#"{"error":{"message":"You exceeded your current quota, please check your plan and billing details.","type":"insufficient_quota","param":null,"code":"insufficient_quota"}}"#;
    assert_eq!(
        parse_provider_error_body(body),
        "You exceeded your current quota, please check your plan and billing details."
    );
}

#[test]
fn parse_provider_error_body_strips_anthropic_overloaded_envelope() {
    let body = r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#;
    let msg = parse_provider_error_body(body);
    assert_eq!(msg, "Overloaded");
    assert!(!msg.contains("overloaded_error"));
    assert!(!msg.contains('{'));
}

#[test]
fn parse_provider_error_body_strips_gemini_resource_exhausted_envelope() {
    let body = r#"{"error":{"code":429,"message":"Resource has been exhausted (e.g. check quota).","status":"RESOURCE_EXHAUSTED","details":[{"@type":"type.googleapis.com/google.rpc.QuotaFailure","violations":[{"quotaMetric":"generativelanguage.googleapis.com/generate_content_free_tier_requests"}]}]}}"#;
    let msg = parse_provider_error_body(body);
    assert_eq!(msg, "Resource has been exhausted (e.g. check quota).");
    assert!(!msg.contains("RESOURCE_EXHAUSTED"));
    assert!(!msg.contains("QuotaFailure"));
}

#[test]
fn parse_provider_error_body_falls_back_to_raw_for_non_json() {
    assert_eq!(
        parse_provider_error_body("upstream connect timeout"),
        "upstream connect timeout"
    );
}

#[test]
fn parse_provider_error_body_falls_back_when_message_missing() {
    let body = r#"{"error":{"type":"unknown"}}"#;
    assert_eq!(parse_provider_error_body(body), body);
}

#[test]
fn parse_retry_hint_prefers_retry_after_over_reset() {
    // When both headers are present, Retry-After wins.
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(reqwest::header::RETRY_AFTER, "5".parse().unwrap());
    headers.insert("x-ratelimit-reset", "9999999999".parse().unwrap());
    let hint = parse_retry_hint(&headers).expect("expected a hint");
    assert_eq!(hint, std::time::Duration::from_secs(5));
}

#[tokio::test(start_paused = true)]
async fn retry_honors_server_hint_over_exponential() {
    // base_delay_ms=1 → exponential floor on first retry is ~1-2ms.
    // Hint of 5s must dominate.  Paused time auto-advances through
    // sleeps, so the assertion is on virtual elapsed time.
    let (scripted, _attempts) = ScriptedClient::new(vec![
        Err(rate_limit_err(Some(std::time::Duration::from_secs(5)))),
        Ok(()),
    ]);
    let client = RetryingLlmClient::with_base_delay(Box::new(scripted), 3, 1);
    let start = tokio::time::Instant::now();
    client
        .stream(
            &[],
            "",
            "",
            &[],
            &std::collections::HashMap::new(),
            &empty_config(),
        )
        .await
        .expect("retry should succeed");
    let elapsed = start.elapsed();
    assert!(
        elapsed >= std::time::Duration::from_secs(5) && elapsed < std::time::Duration::from_secs(6),
        "expected ~5s wait honoring hint, got {elapsed:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn retry_caps_excessive_server_hint() {
    // A 10-minute server hint must be capped to MAX_HINT_MS (90s) so a
    // misbehaving header can't wedge the loop.  Tokio's paused-time mode
    // lets us assert the actual elapsed virtual time without sleeping.
    let (scripted, attempts) = ScriptedClient::new(vec![
        Err(rate_limit_err(Some(std::time::Duration::from_secs(600)))),
        Ok(()),
    ]);
    let client = RetryingLlmClient::with_base_delay(Box::new(scripted), 1, 1);
    let start = tokio::time::Instant::now();
    client
        .stream(
            &[],
            "",
            "",
            &[],
            &std::collections::HashMap::new(),
            &empty_config(),
        )
        .await
        .expect("retry should succeed");
    let elapsed = start.elapsed();
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    assert!(
        elapsed <= std::time::Duration::from_millis(90_001),
        "expected wait capped at 90s, got {elapsed:?}"
    );
    assert!(
        elapsed >= std::time::Duration::from_millis(89_000),
        "expected wait near the 90s cap, got {elapsed:?}"
    );
}

// -----------------------------------------------------------------------
// ConcurrencyLimitedLlmClient tests
// -----------------------------------------------------------------------

/// Test client that records how many `stream()` calls overlap.  Each
/// call increments an in-flight counter, awaits a notification gating
/// release, then decrements.  The peak counter shows whether the
/// semaphore actually serialised callers.
struct OverlapTrackingClient {
    in_flight: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
    gate: Arc<tokio::sync::Notify>,
}

impl OverlapTrackingClient {
    fn new() -> (Self, Arc<AtomicUsize>, Arc<tokio::sync::Notify>) {
        let in_flight = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let gate = Arc::new(tokio::sync::Notify::new());
        (
            Self {
                in_flight: Arc::clone(&in_flight),
                peak: Arc::clone(&peak),
                gate: Arc::clone(&gate),
            },
            peak,
            gate,
        )
    }
}

#[async_trait]
impl LlmClient for OverlapTrackingClient {
    async fn stream(
        &self,
        _messages: &[Message],
        _system: &str,
        _system_suffix: &str,
        _tools: &[ToolDefinition],
        _tool_instances: &std::collections::HashMap<String, std::sync::Arc<dyn Tool>>,
        _config: &CompletionConfig,
    ) -> Result<StreamResponse> {
        let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(now, Ordering::SeqCst);
        self.gate.notified().await;
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
        Ok(StreamResponse {
            stream: Box::pin(tokio_stream::iter(std::iter::empty())),
            tool_mode: ToolMode::Execute,
            input_tokens: None,
            swarm_llm_audit_id: None,
            provider: None,
            model: None,
        })
    }
}

#[tokio::test]
async fn concurrency_limit_caps_in_flight_calls() {
    let (inner, peak, gate) = OverlapTrackingClient::new();
    let client = Arc::new(ConcurrencyLimitedLlmClient::new(Box::new(inner), 2));

    // Spawn 6 callers; only 2 may be in flight at once.
    let mut handles = Vec::new();
    for _ in 0..6 {
        let c = Arc::clone(&client);
        handles.push(tokio::spawn(async move {
            c.stream(
                &[],
                "",
                "",
                &[],
                &std::collections::HashMap::new(),
                &empty_config(),
            )
            .await
            .unwrap();
        }));
    }

    // Let calls progress in waves of 2 by releasing the gate repeatedly.
    // notify_one wakes one waiter; loop until all 6 have completed.
    for _ in 0..6 {
        // Give spawned tasks a tick to enter the inner client.
        tokio::task::yield_now().await;
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        gate.notify_one();
    }
    for h in handles {
        h.await.unwrap();
    }

    let observed_peak = peak.load(Ordering::SeqCst);
    assert!(
        observed_peak <= 2,
        "expected ≤2 concurrent calls, observed peak {observed_peak}"
    );
    assert!(observed_peak >= 1, "expected at least one call to land");
}

#[tokio::test]
async fn concurrency_limit_zero_is_disabled_path_in_create_client() {
    // Direct test of the wrapper itself: with cap=1, 3 spawned callers
    // serialise (peak == 1).  Validates the semaphore is actually
    // applied — `create_client` skips wrapping when the cap is 0, so
    // that path stays unchanged.
    let (inner, peak, gate) = OverlapTrackingClient::new();
    let client = Arc::new(ConcurrencyLimitedLlmClient::new(Box::new(inner), 1));

    let mut handles = Vec::new();
    for _ in 0..3 {
        let c = Arc::clone(&client);
        handles.push(tokio::spawn(async move {
            c.stream(
                &[],
                "",
                "",
                &[],
                &std::collections::HashMap::new(),
                &empty_config(),
            )
            .await
            .unwrap();
        }));
    }
    for _ in 0..3 {
        tokio::task::yield_now().await;
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        gate.notify_one();
    }
    for h in handles {
        h.await.unwrap();
    }
    assert_eq!(peak.load(Ordering::SeqCst), 1);
}

struct ActiveResponseGuard {
    active: Arc<AtomicUsize>,
}

impl Drop for ActiveResponseGuard {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::SeqCst);
    }
}

struct StreamingOverlapClient {
    active: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
}

#[async_trait]
impl LlmClient for StreamingOverlapClient {
    async fn stream(
        &self,
        _messages: &[Message],
        _system: &str,
        _system_suffix: &str,
        _tools: &[ToolDefinition],
        _tool_instances: &std::collections::HashMap<String, std::sync::Arc<dyn Tool>>,
        _config: &CompletionConfig,
    ) -> Result<StreamResponse> {
        let now = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(now, Ordering::SeqCst);
        let guard = ActiveResponseGuard {
            active: Arc::clone(&self.active),
        };
        let stream = async_stream::stream! {
            let _guard = guard;
            std::future::pending::<()>().await;
            #[allow(unreachable_code)]
            yield Ok(StreamEvent::TextDelta(String::new()));
        };
        Ok(StreamResponse {
            stream: Box::pin(stream),
            tool_mode: ToolMode::Execute,
            input_tokens: None,
            swarm_llm_audit_id: None,
            provider: None,
            model: None,
        })
    }
}

#[tokio::test]
async fn concurrency_permit_is_held_until_response_stream_drops() {
    use futures_util::StreamExt as _;

    let active = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let client = Arc::new(ConcurrencyLimitedLlmClient::new(
        Box::new(StreamingOverlapClient {
            active: Arc::clone(&active),
            peak: Arc::clone(&peak),
        }),
        1,
    ));

    let mut handles = Vec::new();
    for _ in 0..2 {
        let client = Arc::clone(&client);
        handles.push(tokio::spawn(async move {
            let mut response = client
                .stream(
                    &[],
                    "",
                    "",
                    &[],
                    &std::collections::HashMap::new(),
                    &empty_config(),
                )
                .await
                .unwrap();
            let _ = response.stream.next().await;
        }));
    }

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(
        peak.load(Ordering::SeqCst),
        1,
        "the configured cap must cover active response streams"
    );
    for handle in handles {
        handle.abort();
        let _ = handle.await;
    }
}
