use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

struct QuestionClient(Arc<AtomicUsize>);
#[async_trait::async_trait]
impl LlmClient for QuestionClient {
    async fn stream(
        &self,
        _: &[Message],
        _: &str,
        _: &str,
        _: &[ToolDefinition],
        _: &std::collections::HashMap<String, Arc<dyn dyson::tool::Tool>>,
        _: &CompletionConfig,
    ) -> dyson::Result<StreamResponse> {
        let first = self.0.fetch_add(1, Ordering::SeqCst) == 0;
        let mut events = if first {
            vec![StreamEvent::ToolUseComplete {
                id: "choose-branch".into(),
                name: "request_human_input".into(),
                input: serde_json::json!({"question":"Which branch?"}),
            }]
        } else {
            vec![StreamEvent::TextDelta("Answer received".into())]
        };
        events.push(StreamEvent::MessageComplete {
            stop_reason: if first {
                StopReason::ToolUse
            } else {
                StopReason::EndTurn
            },
            output_tokens: Some(4),
        });
        Ok(StreamResponse {
            stream: Box::pin(tokio_stream::iter(events.into_iter().map(Ok))),
            tool_mode: ToolMode::Execute,
            input_tokens: None,
            swarm_llm_audit_id: None,
            provider: None,
            model: None,
        })
    }
}

async fn wait_state(base: &str, id: &str, expected: &str) -> serde_json::Value {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let response = get(&format!("{base}/api/conversations/{id}/run")).await;
            if response.status() == StatusCode::OK {
                let value = body_json(response).await;
                // Check the worker released the conversation as well as the durable state.
                if value["state"]["state"] == expected {
                    let chat =
                        body_json(get(&format!("{base}/api/conversations/{id}")).await).await;
                    if chat["live"] != true {
                        return value;
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("run state should settle")
}

async fn waiting_question() -> (Rig, Arc<AtomicUsize>, String, serde_json::Value) {
    let calls = Arc::new(AtomicUsize::new(0));
    let r = rig_with_auth_and_client(
        Arc::new(DangerousNoAuth),
        Box::new(QuestionClient(calls.clone())),
    )
    .await;
    let created = body_json(
        post_json(
            &format!("{}/api/conversations", r.base),
            &serde_json::json!({"title":"durable"}),
        )
        .await,
    )
    .await;
    let id = created["id"].as_str().unwrap().to_string();
    assert_eq!(
        post_json(
            &format!("{}/api/conversations/{id}/turn", r.base),
            &serde_json::json!({"prompt":"Ask me"})
        )
        .await
        .status(),
        StatusCode::ACCEPTED
    );
    let waiting = wait_state(&r.base, &id, "waiting_for_input").await;
    let run = waiting["run_id"].clone();
    (r, calls, id, run)
}

async fn reconstruct(r: &Rig, calls: Arc<AtomicUsize>) -> (String, JoinHandle<dyson::Result<()>>) {
    r._handle.abort();

    let settings = r.state.settings_snapshot();
    let registry = Arc::new(ClientRegistry::new_with_default_client_for_test(
        &settings,
        Box::new(QuestionClient(calls.clone())),
    ));
    let history: Arc<dyn ChatHistory> =
        Arc::new(DiskChatHistory::new(r.chat_dir.path().to_path_buf()).unwrap());
    let state = test_helpers::build_state(
        settings,
        registry,
        Some(history),
        None,
        Arc::new(DangerousNoAuth),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let worker = tokio::spawn(test_helpers::serve(state, listener));
    (base, worker)
}

#[tokio::test]
async fn durable_question_can_be_answered_after_http_process_reconstruction() {
    let (r, calls, id, run) = waiting_question().await;
    let (base, worker) = reconstruct(&r, calls.clone()).await;
    let prompts = body_json(get(&format!("{base}/api/mcp/elicitations")).await).await;
    let prompt = prompts["pending"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["conversation_id"] == id)
        .unwrap();
    assert_eq!(prompt["run_id"], run);
    let answer_url = format!(
        "{base}/api/mcp/elicitations/{}",
        prompt["id"].as_str().unwrap()
    );
    assert_eq!(
        post_json(
            &answer_url,
            &serde_json::json!({"action":"accept","content":{"answer":""}})
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        post_json(
            &answer_url,
            &serde_json::json!({"action":"accept","content":{"answer":"main"}})
        )
        .await
        .status(),
        StatusCode::ACCEPTED
    );
    let finished = wait_state(&base, &id, "finished").await;
    assert_eq!(finished["run_id"], run);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        post_json(
            &answer_url,
            &serde_json::json!({"action":"accept","content":{"answer":"main"}})
        )
        .await
        .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        post_json(
            &format!("{base}/api/conversations/{id}/resume"),
            &serde_json::json!({"run_id":run})
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    worker.abort();
}

#[tokio::test]
async fn suspended_run_can_be_cancelled_without_reconstructing_an_agent() {
    let (r, calls, id, run) = waiting_question().await;
    let (base, worker) = reconstruct(&r, calls.clone()).await;
    assert_eq!(
        post_json(
            &format!("{base}/api/conversations/{id}/cancel"),
            &serde_json::json!({})
        )
        .await
        .status(),
        StatusCode::OK
    );
    let finished = wait_state(&base, &id, "finished").await;
    assert_eq!(finished["run_id"], run);
    assert_eq!(finished["state"]["status"], "cancelled");
    let pending = body_json(get(&format!("{base}/api/mcp/elicitations")).await).await;
    assert!(pending["pending"].as_array().unwrap().is_empty());
    assert_eq!(
        post_json(
            &format!("{base}/api/conversations/{id}/resume"),
            &serde_json::json!({"run_id":run})
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    worker.abort();
}
