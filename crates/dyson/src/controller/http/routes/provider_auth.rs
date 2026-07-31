//! Subscription-backed provider authentication.
//!
//! Managed Dysons have no terminal front door, so the one-time Codex device
//! flow is brokered through the authenticated Dyson SPA. The Codex CLI still
//! owns the OAuth exchange and token refresh; this route exposes only the
//! verification URL, short-lived user code, and coarse connection state.

use std::process::Stdio;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hyper::StatusCode;
use serde::Serialize;
use tokio::io::{AsyncBufReadExt, BufReader};

use crate::config::LlmProvider;

use super::super::responses::{Resp, bad_request, json_ok, json_status, not_found};
use super::super::state::HttpState;

const DEVICE_FLOW_TTL_SECS: u64 = 15 * 60;

#[derive(Clone, Debug)]
struct CodexLoginState {
    state: &'static str,
    verification_uri: Option<String>,
    user_code: Option<String>,
    expires_at: Option<u64>,
    error: Option<String>,
}

impl Default for CodexLoginState {
    fn default() -> Self {
        Self {
            state: "disconnected",
            verification_uri: None,
            user_code: None,
            expires_at: None,
            error: None,
        }
    }
}

#[derive(Serialize)]
struct CodexAuthDto {
    available: bool,
    connected: bool,
    state: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    verification_uri: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    user_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    expires_at: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

static CODEX_LOGIN: OnceLock<Mutex<CodexLoginState>> = OnceLock::new();

fn login_state() -> &'static Mutex<CodexLoginState> {
    CODEX_LOGIN.get_or_init(|| Mutex::new(CodexLoginState::default()))
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn codex_provider_enabled(state: &HttpState) -> bool {
    state
        .settings_snapshot()
        .providers
        .values()
        .any(|provider| provider.provider_type == LlmProvider::Codex)
}

fn snapshot(connected: bool) -> CodexAuthDto {
    let state = login_state()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    CodexAuthDto {
        available: true,
        connected,
        state: if connected { "connected" } else { state.state },
        verification_uri: (!connected).then_some(state.verification_uri).flatten(),
        user_code: (!connected).then_some(state.user_code).flatten(),
        expires_at: (!connected).then_some(state.expires_at).flatten(),
        error: (!connected).then_some(state.error).flatten(),
    }
}

fn codex_command() -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new("codex");
    cmd.env_clear()
        .envs(crate::llm::cli_subprocess::sanitized_child_env(
            std::env::vars(),
        ));
    cmd
}

async fn is_connected() -> bool {
    let mut cmd = codex_command();
    cmd.args(["login", "status"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    matches!(
        tokio::time::timeout(Duration::from_secs(5), cmd.status()).await,
        Ok(Ok(status)) if status.success()
    )
}

pub(super) async fn codex_status(state: &HttpState) -> Resp {
    if !codex_provider_enabled(state) {
        return not_found();
    }
    json_ok(&snapshot(is_connected().await))
}

pub(super) async fn codex_start(state: &HttpState) -> Resp {
    if !codex_provider_enabled(state) {
        return not_found();
    }
    if is_connected().await {
        return json_ok(&snapshot(true));
    }

    let reuse_pending = {
        let mut current = login_state()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let still_live = matches!(current.state, "starting" | "pending")
            && current.expires_at.is_none_or(|expiry| expiry > now_secs());
        if !still_live {
            *current = CodexLoginState {
                state: "starting",
                expires_at: Some(now_secs() + DEVICE_FLOW_TTL_SECS),
                ..CodexLoginState::default()
            };
        }
        still_live
    };
    if reuse_pending {
        return json_ok(&snapshot(false));
    }

    let mut cmd = codex_command();
    cmd.args(["login", "--device-auth"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);

    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(err) => {
            tracing::warn!(error = %err, "codex subscription login could not start");
            let mut current = login_state()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            current.state = "unavailable";
            current.error = Some("Codex CLI is not available in this Dyson image".to_owned());
            drop(current);
            return json_status(StatusCode::SERVICE_UNAVAILABLE, &snapshot(false));
        }
    };

    let Some(stdout) = child.stdout.take() else {
        let mut current = login_state()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        current.state = "failed";
        current.error = Some("Codex login did not expose its device flow".to_owned());
        drop(current);
        return json_status(StatusCode::SERVICE_UNAVAILABLE, &snapshot(false));
    };

    tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let clean = strip_ansi(&line);
            let mut current = login_state()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(uri) = verification_uri(&clean) {
                current.verification_uri = Some(uri);
            }
            if let Some(code) = device_code(&clean) {
                current.user_code = Some(code);
            }
            if current.verification_uri.is_some() && current.user_code.is_some() {
                current.state = "pending";
            }
        }

        let success = child.wait().await.is_ok_and(|status| status.success());
        let mut current = login_state()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if success {
            *current = CodexLoginState {
                state: "connected",
                ..CodexLoginState::default()
            };
        } else if current.state != "connected" {
            current.state = "failed";
            current.error = Some("The ChatGPT sign-in expired or was cancelled".to_owned());
        }
    });

    // Give the CLI a brief chance to print the URL and code so the first
    // response is useful. The SPA also polls, so a slow process is harmless.
    for _ in 0..20 {
        let ready = {
            let current = login_state()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            current.state != "starting"
        };
        if ready {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    json_status(StatusCode::ACCEPTED, &snapshot(false))
}

pub(super) async fn codex_logout(state: &HttpState) -> Resp {
    if !codex_provider_enabled(state) {
        return not_found();
    }
    let in_progress = {
        let current = login_state()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        matches!(current.state, "starting" | "pending")
    };
    if in_progress {
        return bad_request("finish or let the current device-code sign-in expire first");
    }

    let mut cmd = codex_command();
    cmd.arg("logout")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let ok = matches!(
        tokio::time::timeout(Duration::from_secs(5), cmd.status()).await,
        Ok(Ok(status)) if status.success()
    );
    if !ok {
        return json_status(
            StatusCode::SERVICE_UNAVAILABLE,
            &serde_json::json!({ "error": "Codex logout failed" }),
        );
    }
    *login_state()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = CodexLoginState::default();
    json_ok(&snapshot(false))
}

fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' && chars.peek() == Some(&'[') {
            chars.next();
            for next in chars.by_ref() {
                if ('@'..='~').contains(&next) {
                    break;
                }
            }
        } else {
            out.push(ch);
        }
    }
    out
}

fn verification_uri(line: &str) -> Option<String> {
    line.split_whitespace()
        .find(|word| word.starts_with("https://"))
        .map(|word| {
            word.trim_end_matches(|c: char| ",.;)".contains(c))
                .to_owned()
        })
}

fn device_code(line: &str) -> Option<String> {
    line.split_whitespace().find_map(|word| {
        let candidate = word.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '-');
        let (left, right) = candidate.split_once('-')?;
        let valid_half = |half: &str| {
            (4..=5).contains(&half.len())
                && half
                    .bytes()
                    .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
        };
        (valid_half(left) && valid_half(right)).then(|| candidate.to_owned())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_only_device_flow_fields_from_ansi_output() {
        let uri = strip_ansi("   \u{1b}[94mhttps://auth.openai.com/codex/device\u{1b}[0m");
        let code = strip_ansi("   \u{1b}[94m28FB-361C8\u{1b}[0m");
        assert_eq!(
            verification_uri(&uri).as_deref(),
            Some("https://auth.openai.com/codex/device")
        );
        assert_eq!(device_code(&code).as_deref(), Some("28FB-361C8"));
    }

    #[test]
    fn prose_is_not_mistaken_for_a_device_code() {
        assert_eq!(device_code("Continue only if you started this login"), None);
        assert_eq!(verification_uri("no link here"), None);
    }
}
