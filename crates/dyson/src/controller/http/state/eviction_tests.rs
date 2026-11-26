use super::*;
use crate::agent::Agent;

struct NoopLlm;

#[async_trait::async_trait]
impl crate::llm::LlmClient for NoopLlm {
    async fn stream(
        &self,
        _messages: &[Message],
        _system: &str,
        _system_suffix: &str,
        _tools: &[crate::llm::ToolDefinition],
        _tool_instances: &std::collections::HashMap<String, std::sync::Arc<dyn crate::tool::Tool>>,
        _config: &crate::llm::CompletionConfig,
    ) -> dyson_core::error::Result<crate::llm::StreamResponse> {
        Err(dyson_core::error::DysonError::Llm("noop".into()))
    }
}

fn make_agent() -> Agent {
    let settings = crate::config::AgentSettings {
        api_key: "test".into(),
        ..Default::default()
    };
    let sandbox: Arc<dyn crate::sandbox::Sandbox> =
        Arc::new(crate::sandbox::no_sandbox::DangerousNoSandbox::new(
            crate::sandbox::SandboxBypassGuard::for_test(),
        ));
    Agent::new(
        crate::agent::rate_limiter::RateLimitedHandle::unlimited(
            Box::new(NoopLlm) as Box<dyn crate::llm::LlmClient>
        ),
        sandbox,
        Vec::new(),
        &settings,
        None,
        0,
        None,
        None,
    )
    .unwrap()
}

fn build_test_state() -> Arc<HttpState> {
    let settings = Settings::default();
    let registry = Arc::new(ClientRegistry::new(&settings, None));
    super::super::test_helpers::build_state(
        settings,
        registry,
        None,
        None,
        Arc::new(crate::auth::DangerousNoAuth),
    )
}

#[tokio::test]
async fn sse_tickets_are_capped() {
    // MEM-06: unexpired tickets were only pruned by age, so a client
    // minting faster than the 30 s TTL grew the map without bound.
    let state = build_test_state();
    let first = state.mint_sse_ticket("bearer");
    for _ in 0..(MAX_SSE_TICKETS + 100) {
        state.mint_sse_ticket("bearer");
    }
    let len = state.sse_tickets.lock().unwrap().len();
    assert!(len <= MAX_SSE_TICKETS, "{len} live tickets");
    // The oldest ticket was the one evicted.
    assert!(state.consume_sse_ticket(&first).is_none());
    let fresh = state.mint_sse_ticket("bearer");
    assert_eq!(state.consume_sse_ticket(&fresh).as_deref(), Some("bearer"));
}

// Regression: the per-chat Agent cache never evicted — every chat
// ever opened kept its full in-memory history (including restored
// base64 images) until deletion or process restart.
#[tokio::test]
async fn idle_agents_are_evicted_but_busy_and_fresh_ones_are_kept() {
    let state = build_test_state();

    let idle = Arc::new(ChatHandle::new("c-idle".into(), "t".into(), None));
    *idle.agent.lock().await = Some(make_agent());
    let busy = Arc::new(ChatHandle::new("c-busy".into(), "t".into(), None));
    *busy.agent.lock().await = Some(make_agent());
    busy.busy.store(true, std::sync::atomic::Ordering::SeqCst);
    let fresh = Arc::new(ChatHandle::new("c-fresh".into(), "t".into(), None));
    *fresh.agent.lock().await = Some(make_agent());

    {
        let mut chats = state.chats.lock().await;
        chats.insert("c-idle".into(), Arc::clone(&idle));
        chats.insert("c-busy".into(), Arc::clone(&busy));
        chats.insert("c-fresh".into(), Arc::clone(&fresh));
    }

    // Age the idle and busy chats past the threshold; keep `fresh`
    // recent by touching it.
    let past = std::time::Instant::now() - std::time::Duration::from_secs(3600);
    *idle.last_used.lock().unwrap() = past;
    *busy.last_used.lock().unwrap() = past;
    fresh.touch();

    let evicted = state
        .evict_idle_agents(std::time::Duration::from_secs(30 * 60))
        .await;

    assert_eq!(evicted, 1, "exactly the idle, non-busy agent is evicted");
    assert!(
        idle.agent.lock().await.is_none(),
        "idle agent must be dropped — the next turn rebuilds it from disk \
         (the guard.is_none() branch of the turn dispatcher)"
    );
    assert!(
        busy.agent.lock().await.is_some(),
        "a chat with an in-flight turn must never lose its agent"
    );
    assert!(
        fresh.agent.lock().await.is_some(),
        "recently-used agents stay cached"
    );

    // Idempotent: a second sweep finds nothing new.
    assert_eq!(
        state
            .evict_idle_agents(std::time::Duration::from_secs(30 * 60))
            .await,
        0
    );
}
