// ===========================================================================
// /api/conversations/:id/events — Server-Sent Events stream.
//
// One subscriber per `EventSource` open.  Frames are `data: <json>\n\n`
// where the JSON is a `SseEvent` enum variant.  The stream closes
// after the `Done` event so the client's `.close()` is the natural
// next step.
// ===========================================================================

use std::convert::Infallible;

use http_body_util::{BodyExt, StreamBody};
use hyper::body::{Bytes, Frame};
use hyper::{Response, StatusCode};
use tokio::sync::broadcast;

use super::super::responses::{Resp, not_found};
use super::super::state::HttpState;
use super::super::wire::SseEvent;

pub(super) async fn events(state: &HttpState, id: &str) -> Resp {
    let handle = match state.chats.lock().await.get(id).cloned() {
        Some(h) => h,
        None => return not_found(),
    };
    let mut rx = handle.events.subscribe();
    // Build the SSE byte stream by hand so we don't depend on
    // tokio-stream's `sync` feature (would add to deps).  Each
    // broadcast::recv outcome → one frame.
    let body_stream = async_stream::stream! {
        loop {
            match rx.recv().await {
                Ok(evt) => {
                    yield Ok::<_, Infallible>(Frame::data(Bytes::from(format_sse(&evt))));
                    if matches!(evt, SseEvent::Done) { break; }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    yield Ok(Frame::data(Bytes::from_static(b": lag\n\n")));
                }
                Err(broadcast::error::RecvError::Closed) => break,
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

fn format_sse(evt: &SseEvent) -> String {
    let json = serde_json::to_string(evt).unwrap_or_else(|_| "{}".to_string());
    format!("data: {json}\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_sse_produces_one_frame_per_event() {
        let evt = SseEvent::Text { delta: "hi".to_string() };
        let frame = format_sse(&evt);
        assert!(frame.starts_with("data: "));
        assert!(frame.ends_with("\n\n"));
        assert!(frame.contains(r#""type":"text""#));
        assert!(frame.contains(r#""delta":"hi""#));
    }

    #[test]
    fn format_sse_handles_done_with_empty_payload() {
        let frame = format_sse(&SseEvent::Done);
        assert_eq!(frame, "data: {\"type\":\"done\"}\n\n");
    }
}
