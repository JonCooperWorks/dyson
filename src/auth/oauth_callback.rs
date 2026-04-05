// ===========================================================================
// OAuth callback server — temporary HTTP server for receiving authorization
// codes during the OAuth 2.0 Authorization Code flow.
//
// LEARNING OVERVIEW
//
// What this module does:
//   Starts a temporary HTTP server on a random loopback port that listens
//   for a single OAuth callback.  When the user completes authorization in
//   their browser, the OAuth server redirects to this callback URL with an
//   authorization code.  The server captures the code, sends an HTML success
//   page to the browser, and shuts down.
//
// Controller-agnostic design:
//   This server runs independently of any controller.  It binds to
//   127.0.0.1:<random-port> on the Dyson host.  The MCP skill layer starts
//   it and includes the port in the redirect_uri.  The user clicks the auth
//   URL from whatever controller they're using (Terminal, Telegram, etc.)
//   and the callback hits this server directly.
//
// Timeout behavior:
//   The server automatically shuts down after 5 minutes if no callback is
//   received.  This prevents leaked server tasks if the user abandons the
//   OAuth flow.
//
// Hyper pattern:
//   Uses the same hyper HTTP/1.1 server pattern as McpHttpServer in
//   src/skill/mcp/serve.rs — TcpListener on 127.0.0.1:0, one task per
//   connection, service_fn for request dispatch.
// ===========================================================================

use std::convert::Infallible;
use std::time::Duration;

use http_body_util::Full;
use hyper::body::Bytes;
use hyper::server::conn::http1;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use crate::error::Result;

// ---------------------------------------------------------------------------
// Callback result
// ---------------------------------------------------------------------------

/// The authorization code received from the OAuth callback.
///
/// The `state` parameter is validated by the callback server before
/// sending this result — callers don't need to re-check it.
#[derive(Debug)]
pub struct CallbackResult {
    pub code: String,
}

// ---------------------------------------------------------------------------
// Callback server
// ---------------------------------------------------------------------------

/// Start a temporary HTTP server on `127.0.0.1:0` that waits for an OAuth callback.
///
/// Returns `(port, task_handle, receiver)`.  The receiver yields the authorization
/// code when the callback fires.  The server auto-shuts down after `timeout`.
pub async fn start_callback_server(
    expected_state: &str,
    timeout: Duration,
) -> Result<(u16, JoinHandle<()>, oneshot::Receiver<CallbackResult>)> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let port = addr.port();

    let (tx, rx) = oneshot::channel::<CallbackResult>();

    // Move the expected state into the server task.
    let expected_state = expected_state.to_string();

    tracing::info!(port = port, "OAuth callback server listening");

    let handle = tokio::spawn(async move {
        // Wrap the sender in an Option so we can take() it exactly once.
        let tx = std::sync::Arc::new(tokio::sync::Mutex::new(Some(tx)));

        // Race the listener against a timeout.
        let result = tokio::time::timeout(timeout, async {
            loop {
                let (stream, _addr) = match listener.accept().await {
                    Ok(conn) => conn,
                    Err(e) => {
                        tracing::warn!(error = %e, "OAuth callback accept error");
                        continue;
                    }
                };

                let spawn_state = expected_state.clone();
                let spawn_tx = tx.clone();

                tokio::spawn(async move {
                    let io = TokioIo::new(stream);

                    let service = hyper::service::service_fn(move |req| {
                        let expected_state = spawn_state.clone();
                        let tx = spawn_tx.clone();
                        async move {
                            handle_callback(req, &expected_state, tx).await
                        }
                    });

                    if let Err(e) = http1::Builder::new().serve_connection(io, service).await {
                        tracing::debug!(error = %e, "OAuth callback connection error");
                    }
                });

                // Check if the sender has been consumed (callback received).
                let guard = tx.lock().await;
                if guard.is_none() {
                    break;
                }
            }
        })
        .await;

        if result.is_err() {
            tracing::warn!(
                timeout_secs = timeout.as_secs(),
                "OAuth callback server timed out — no callback received"
            );
        }
    });

    Ok((port, handle, rx))
}

/// Handle `GET /callback?code=<code>&state=<state>`.
async fn handle_callback(
    req: Request<hyper::body::Incoming>,
    expected_state: &str,
    tx: std::sync::Arc<tokio::sync::Mutex<Option<oneshot::Sender<CallbackResult>>>>,
) -> std::result::Result<Response<Full<Bytes>>, Infallible> {
    // Only handle GET /callback.
    if req.method() != hyper::Method::GET || !req.uri().path().starts_with("/callback") {
        return Ok(Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Full::new(Bytes::from("Not Found")))
            .unwrap());
    }

    // Parse query parameters using reqwest::Url for correct percent-decoding.
    let query = req.uri().query().unwrap_or("");
    let dummy_url = format!("http://localhost/callback?{query}");
    let params: Vec<(String, String)> = reqwest::Url::parse(&dummy_url)
        .map(|url| {
            url.query_pairs()
                .map(|(k, v)| (k.into_owned(), v.into_owned()))
                .collect()
        })
        .unwrap_or_default();

    let code = params.iter().find(|(k, _)| k == "code").map(|(_, v)| v.as_str());
    let state = params.iter().find(|(k, _)| k == "state").map(|(_, v)| v.as_str());

    // Check for OAuth error response (e.g., user denied access).
    if let Some((_, error)) = params.iter().find(|(k, _)| k == "error") {
        let description = params
            .iter()
            .find(|(k, _)| k == "error_description")
            .map(|(_, v)| v.as_str())
            .unwrap_or("unknown error");

        tracing::warn!(error = %error, description = %description, "OAuth authorization denied");

        return Ok(Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .header("Content-Type", "text/html; charset=utf-8")
            .body(Full::new(Bytes::from(format!(
                "<html><body><h1>Authorization Failed</h1><p>{error}: {description}</p>\
                 <p>You can close this tab.</p></body></html>"
            ))))
            .unwrap());
    }

    // Validate required parameters.
    let (Some(code), Some(state)) = (code, state) else {
        return Ok(Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .header("Content-Type", "text/html; charset=utf-8")
            .body(Full::new(Bytes::from(
                "<html><body><h1>Bad Request</h1>\
                 <p>Missing code or state parameter.</p></body></html>",
            )))
            .unwrap());
    };

    // Validate CSRF state parameter.
    if state != expected_state {
        tracing::warn!(
            expected = %expected_state,
            received = %state,
            "OAuth state mismatch — possible CSRF attack"
        );

        return Ok(Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .header("Content-Type", "text/html; charset=utf-8")
            .body(Full::new(Bytes::from(
                "<html><body><h1>State Mismatch</h1>\
                 <p>The state parameter does not match. This may be a CSRF attack.</p>\
                 <p>Please try authorizing again.</p></body></html>",
            )))
            .unwrap());
    }

    // Send the code through the oneshot channel.
    let mut guard = tx.lock().await;
    if let Some(sender) = guard.take() {
        let result = CallbackResult {
            code: code.to_string(),
        };

        if sender.send(result).is_err() {
            tracing::warn!("OAuth callback receiver dropped before code could be sent");
        } else {
            tracing::info!("OAuth authorization code received via callback");
        }
    }

    // Return a success page to the user's browser.
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "text/html; charset=utf-8")
        .body(Full::new(Bytes::from(
            "<html><body>\
             <h1>Authorization Complete</h1>\
             <p>You can close this tab and return to your conversation.</p>\
             </body></html>",
        )))
        .unwrap())
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn callback_server_starts_and_binds() {
        let (port, handle, _rx) = start_callback_server("test-state", Duration::from_secs(5))
            .await
            .unwrap();

        assert!(port > 0);
        handle.abort();
    }

    #[tokio::test]
    async fn callback_server_receives_code() {
        let (port, handle, rx) = start_callback_server("my-state", Duration::from_secs(5))
            .await
            .unwrap();

        // Simulate the OAuth redirect.
        let client = reqwest::Client::new();
        let resp = client
            .get(format!(
                "http://127.0.0.1:{port}/callback?code=auth-code-123&state=my-state"
            ))
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status(), 200);
        let body = resp.text().await.unwrap();
        assert!(body.contains("Authorization Complete"));

        // The code should be available on the receiver.
        let result = rx.await.unwrap();
        assert_eq!(result.code, "auth-code-123");
        // state is validated by the server — not in CallbackResult

        handle.abort();
    }

    #[tokio::test]
    async fn callback_server_rejects_wrong_state() {
        let (port, handle, _rx) = start_callback_server("correct-state", Duration::from_secs(5))
            .await
            .unwrap();

        let client = reqwest::Client::new();
        let resp = client
            .get(format!(
                "http://127.0.0.1:{port}/callback?code=code&state=wrong-state"
            ))
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status(), 400);
        let body = resp.text().await.unwrap();
        assert!(body.contains("State Mismatch"));

        handle.abort();
    }

    #[tokio::test]
    async fn callback_server_handles_missing_params() {
        let (port, handle, _rx) = start_callback_server("state", Duration::from_secs(5))
            .await
            .unwrap();

        let client = reqwest::Client::new();
        let resp = client
            .get(format!("http://127.0.0.1:{port}/callback"))
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status(), 400);

        handle.abort();
    }

    #[tokio::test]
    async fn callback_server_404_on_wrong_path() {
        let (port, handle, _rx) = start_callback_server("state", Duration::from_secs(5))
            .await
            .unwrap();

        let client = reqwest::Client::new();
        let resp = client
            .get(format!("http://127.0.0.1:{port}/wrong"))
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status(), 404);

        handle.abort();
    }
}
