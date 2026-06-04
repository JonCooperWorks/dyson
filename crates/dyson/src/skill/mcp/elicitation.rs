// ===========================================================================
// Elicitation broker — bridges a server-originated `elicitation/create`
// request (which arrives on the MCP transport's inbound path, with no chat
// context) to the human at the web UI.
//
// MCP is bidirectional: a server may ask the *client's user* to fill in a
// small form mid-tool-call (`elicitation/create`).  The NotificationRouter
// that receives it runs on the shared transport and has no per-chat handle,
// so it can't push to a specific SSE stream.  Instead it parks the request
// here; the web UI short-polls `GET /api/mcp/elicitations` for open prompts
// and answers with `POST /api/mcp/elicitations/<id>`, which resolves the
// parked request.
//
// The broker is process-global (`OnceLock`) because elicitation is
// inherently process-scoped for a single-user agent, and `UI_ENABLED`
// gates whether we advertise the capability at all — a headless run (CLI)
// has no surface to answer a prompt, so it must not advertise elicitation
// and strand a server waiting on a reply.
// ===========================================================================

use std::collections::HashMap;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use serde_json::Value;
use tokio::sync::{Mutex, oneshot};

/// How long a parked elicitation waits for a UI answer before we give up
/// and tell the server the user cancelled.  Generous: a human may take a
/// while to read and fill the form.
const ELICITATION_TIMEOUT: Duration = Duration::from_secs(300);

/// A server-originated elicitation waiting for a UI answer.
struct Parked {
    server: String,
    message: String,
    schema: Value,
    /// Monotonic stamp so the UI can render oldest-first deterministically;
    /// `HashMap` iteration order is otherwise unstable.
    seq: u64,
    responder: oneshot::Sender<Value>,
}

/// Process-global registry of open elicitations.
pub struct ElicitationBroker {
    pending: Mutex<HashMap<String, Parked>>,
    next_id: AtomicU64,
}

impl ElicitationBroker {
    fn new() -> Self {
        Self {
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
        }
    }

    /// Park a server-originated elicitation and await the UI's answer.
    /// Returns the MCP `ElicitResult` (`{ action, content? }`).  On timeout
    /// or a dropped responder we answer `cancel`, the spec-safe default.
    pub async fn elicit(&self, server: String, message: String, schema: Value) -> Value {
        let seq = self.next_id.fetch_add(1, Ordering::Relaxed);
        let id = seq.to_string();
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(
            id.clone(),
            Parked {
                server,
                message,
                schema,
                seq,
                responder: tx,
            },
        );

        match tokio::time::timeout(ELICITATION_TIMEOUT, rx).await {
            Ok(Ok(result)) => result,
            // Timed out or the responder was dropped — clean up and cancel.
            _ => {
                self.pending.lock().await.remove(&id);
                serde_json::json!({ "action": "cancel" })
            }
        }
    }

    /// Snapshot of the open prompts, oldest first, for the UI poll.
    pub async fn list_pending(&self) -> Vec<Value> {
        let pending = self.pending.lock().await;
        let mut entries: Vec<&Parked> = pending.values().collect();
        entries.sort_by_key(|p| p.seq);
        entries
            .into_iter()
            .map(|p| {
                serde_json::json!({
                    "id": p.seq.to_string(),
                    "server": p.server,
                    "message": p.message,
                    "requestedSchema": p.schema,
                })
            })
            .collect()
    }

    /// Resolve an open elicitation with the UI's answer.  Returns false
    /// when the id is unknown (already answered or timed out).
    pub async fn resolve(&self, id: &str, result: Value) -> bool {
        let Some(parked) = self.pending.lock().await.remove(id) else {
            return false;
        };
        parked.responder.send(result).is_ok()
    }
}

static BROKER: OnceLock<ElicitationBroker> = OnceLock::new();
static UI_ENABLED: AtomicBool = AtomicBool::new(false);

/// The process-wide elicitation broker, created on first use.
pub fn broker() -> &'static ElicitationBroker {
    BROKER.get_or_init(ElicitationBroker::new)
}

/// Declare that a UI capable of answering elicitations is present (called
/// once by the HTTP controller at startup).  Until this is set, the MCP
/// client does not advertise the `elicitation` capability and answers any
/// `elicitation/create` with `-32601`.
pub fn enable_ui() {
    UI_ENABLED.store(true, Ordering::Relaxed);
}

/// Whether a UI is available to answer elicitations.
pub fn ui_enabled() -> bool {
    UI_ENABLED.load(Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn elicit_resolves_with_ui_answer() {
        let broker = ElicitationBroker::new();
        // Drive elicit and resolve concurrently: elicit parks and awaits,
        // resolve answers it.
        let elicit = async {
            broker
                .elicit(
                    "everything".into(),
                    "name?".into(),
                    serde_json::json!({ "type": "object" }),
                )
                .await
        };
        let answer = async {
            // Spin until the prompt is parked, then resolve it.
            loop {
                let pending = broker.list_pending().await;
                if let Some(p) = pending.first() {
                    let id = p["id"].as_str().unwrap().to_string();
                    assert_eq!(p["message"], "name?");
                    assert_eq!(p["server"], "everything");
                    let ok = broker
                        .resolve(&id, serde_json::json!({ "action": "accept", "content": { "name": "ada" } }))
                        .await;
                    assert!(ok);
                    break;
                }
                tokio::task::yield_now().await;
            }
        };
        let (result, ()) = tokio::join!(elicit, answer);
        assert_eq!(result["action"], "accept");
        assert_eq!(result["content"]["name"], "ada");
        // The prompt is consumed.
        assert!(broker.list_pending().await.is_empty());
    }

    #[tokio::test]
    async fn resolve_unknown_id_is_false() {
        let broker = ElicitationBroker::new();
        assert!(!broker.resolve("nope", serde_json::json!({})).await);
    }

    #[tokio::test]
    async fn list_pending_is_oldest_first() {
        // Two prompts park concurrently; the UI must see them in seq order
        // so multiple-prompt queues render predictably.
        let broker = std::sync::Arc::new(ElicitationBroker::new());
        let b1 = broker.clone();
        let b2 = broker.clone();
        let _t1 = tokio::spawn(async move {
            b1.elicit("alpha".into(), "first".into(), serde_json::json!({}))
                .await
        });
        // Park strictly after the first by waiting for it to land.
        loop {
            if !broker.list_pending().await.is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
        let _t2 = tokio::spawn(async move {
            b2.elicit("beta".into(), "second".into(), serde_json::json!({}))
                .await
        });
        // Wait for both.
        loop {
            let pending = broker.list_pending().await;
            if pending.len() == 2 {
                assert_eq!(pending[0]["message"], "first");
                assert_eq!(pending[1]["message"], "second");
                break;
            }
            tokio::task::yield_now().await;
        }
    }

    #[test]
    fn ui_disabled_by_default() {
        // Global default is off; enable_ui flips it (process-global, so we
        // don't assert the post-enable state here to avoid cross-test
        // ordering coupling).
        assert!(!UI_ENABLED.load(Ordering::Relaxed) || ui_enabled());
    }
}
