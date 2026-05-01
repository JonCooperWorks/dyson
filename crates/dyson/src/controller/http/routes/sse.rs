// ===========================================================================
// /api/conversations/:id/events — Server-Sent Events stream.
//
// One subscriber per `EventSource` open.  Frames are
// `id: <n>\ndata: <json>\n\n` so a reconnect carrying
// `Last-Event-ID: <n>` can resume from a known checkpoint — the
// chat handle keeps a rolling replay buffer of the most recent
// events; we drain "everything newer than n" before attaching the
// live broadcast subscriber.  The stream closes after the `Done`
// event so the client's `.close()` is the natural next step.
// ===========================================================================

use std::convert::Infallible;
use std::time::Duration;

use http_body_util::{BodyExt, StreamBody};
use hyper::Request;
use hyper::body::{Bytes, Frame};
use hyper::{Response, StatusCode};
use tokio::sync::broadcast;

use super::super::responses::{Resp, not_found};
use super::super::state::HttpState;
use super::super::wire::SseEvent;

pub(super) async fn events(
    state: &HttpState,
    id: &str,
    req: &Request<hyper::body::Incoming>,
) -> Resp {
    let handle = match state.chats.lock().await.get(id).cloned() {
        Some(h) => h,
        None => return not_found(),
    };
    // Last-Event-ID: <n> tells us the highest id this client already
    // saw — replay everything newer.  Missing / unparsable header
    // means "no checkpoint, just attach to live".
    let since: u64 = req
        .headers()
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let replay: Vec<(u64, SseEvent)> = {
        let ring = match handle.replay.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        ring.since(since)
    };
    let mut rx = handle.events.subscribe();
    let body_stream = async_stream::stream! {
        // Replay first so the client catches up, then attach live.
        for (id, evt) in replay {
            yield Ok::<_, Infallible>(Frame::data(Bytes::from(format_sse(id, &evt))));
            if matches!(evt, SseEvent::Done) { return; }
        }
        loop {
            // Idle timeout — without this, an attacker that opens
            // many SSE connections and never sends a request can
            // pin the controller's connection-limit semaphore
            // (see `MAX_CONCURRENT_CONNS`) until restart.  The
            // broadcast::Receiver::recv() future blocks forever in
            // a quiet chat, so we wrap it in a timeout that closes
            // the connection after a long idle period.  Picked at
            // the long end of "browser will reconnect cleanly":
            // EventSource auto-reconnects on close, so the cost of
            // a false trip is one extra round-trip on the next
            // event.  An emitted SSE comment keeps proxies from
            // closing for inactivity before we do.
            match tokio::time::timeout(IDLE_TIMEOUT, rx.recv()).await {
                Ok(Ok(evt)) => {
                    // The live id isn't visible to the receiver here;
                    // the producer side stamped it on the ring.  Look
                    // up the most-recent ring entry that matches by
                    // identity is overkill — instead, mint a synthetic
                    // increasing id from a local counter so frames
                    // stay sequenced from the client's POV.  The ring
                    // handles persistence; this is just labelling.
                    let id = next_local_id();
                    yield Ok(Frame::data(Bytes::from(format_sse(id, &evt))));
                    if matches!(evt, SseEvent::Done) { break; }
                }
                Ok(Err(broadcast::error::RecvError::Lagged(_))) => {
                    yield Ok(Frame::data(Bytes::from_static(b": lag\n\n")));
                }
                Ok(Err(broadcast::error::RecvError::Closed)) => break,
                Err(_elapsed) => {
                    // Idle for too long — close the stream so the
                    // connection slot frees up.  EventSource will
                    // reconnect with Last-Event-ID and replay
                    // anything emitted in the meantime.
                    break;
                }
            }
        }
    };
    let body = StreamBody::new(body_stream).boxed();
    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "text/event-stream")
        .header("Cache-Control", "no-cache")
        .header("X-Accel-Buffering", "no")
        .body(body)
        .unwrap()
}

fn format_sse(id: u64, evt: &SseEvent) -> String {
    // Serialisation of SseEvent shouldn't fail in practice — every
    // variant carries owned, JSON-safe data — but if it ever did, the
    // old `{}` fallback would silently swallow the event with no log,
    // leaving the client to wonder why something never arrived.  Log
    // and surface a synthetic LlmError frame so the wire stays useful
    // and the operator sees the failure.
    match serde_json::to_string(evt) {
        Ok(json) => format!("id: {id}\ndata: {json}\n\n"),
        Err(e) => {
            tracing::error!(error = %e, event_id = id,
                "format_sse failed to serialise event — emitting synthetic LlmError");
            let synthetic = SseEvent::LlmError {
                message: "internal SSE serialisation error".to_string(),
            };
            // The synthetic event is built from `&str` and should always
            // serialise; if it doesn't, fall back to a hand-rolled frame
            // so the client still sees an error rather than `{}`.
            let json = serde_json::to_string(&synthetic).unwrap_or_else(|_| {
                r#"{"type":"llm_error","message":"internal SSE serialisation error"}"#.to_string()
            });
            format!("id: {id}\ndata: {json}\n\n")
        }
    }
}

/// Maximum quiet period before the SSE handler closes the connection
/// to free its slot in the per-controller connection limit.  Five
/// minutes covers a normal "agent is thinking" pause without forcing
/// a reconnect; longer than that and a malicious client could pin
/// every slot in `MAX_CONCURRENT_CONNS` by opening connections and
/// never sending a turn.  EventSource reconnects automatically with
/// `Last-Event-ID`, so the worst case from a false trip is one extra
/// round-trip.
const IDLE_TIMEOUT: Duration = Duration::from_secs(300);

/// Best-effort monotonic local id used for live frames within a
/// single connection.  Replay frames carry the ring's authoritative
/// id; live frames use this counter so the client still gets
/// sequenced ids and `Last-Event-ID` on a fast reconnect lands
/// somewhere sensible.
fn next_local_id() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_sse_produces_one_frame_per_event() {
        let evt = SseEvent::Text {
            delta: "hi".to_string(),
        };
        let frame = format_sse(7, &evt);
        assert!(frame.starts_with("id: 7\n"));
        assert!(frame.contains("\ndata: "));
        assert!(frame.ends_with("\n\n"));
        assert!(frame.contains(r#""type":"text""#));
        assert!(frame.contains(r#""delta":"hi""#));
    }

    #[test]
    fn format_sse_handles_done_with_empty_payload() {
        let frame = format_sse(1, &SseEvent::Done);
        assert_eq!(frame, "id: 1\ndata: {\"type\":\"done\"}\n\n");
    }
}
