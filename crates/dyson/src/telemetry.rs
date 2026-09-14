//! Opt-in, metadata-only OTLP tracing. Never export application log events.
use opentelemetry::trace::TracerProvider;
use opentelemetry_sdk::trace::SdkTracerProvider;
use tracing_subscriber::{Layer, registry::LookupSpan};

pub struct Guard(Option<SdkTracerProvider>);
impl Drop for Guard {
    fn drop(&mut self) {
        if let Some(provider) = &self.0 {
            let _ = provider.shutdown();
        }
    }
}

/// Only an explicit endpoint enables export. Failure leaves the application usable.
pub fn init(service: &'static str) -> Guard {
    if std::env::var("OTEL_SDK_DISABLED").is_ok_and(|v| v.eq_ignore_ascii_case("true"))
        || ![
            "OTEL_EXPORTER_OTLP_ENDPOINT",
            "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT",
        ]
        .iter()
        .any(|key| std::env::var(key).is_ok_and(|v| !v.trim().is_empty()))
    {
        return Guard(None);
    }
    start_provider(service)
}

fn start_provider(service: &'static str) -> Guard {
    // reqwest's blocking client must be constructed outside a Tokio runtime.
    // Both application entry points initialize logging from async main.
    std::thread::spawn(move || {
        let exporter = opentelemetry_otlp::SpanExporter::builder()
            .with_http()
            .build();
        let Ok(exporter) = exporter else {
            // Exporter errors may contain endpoint credentials. Do not print them.
            eprintln!("OTLP exporter initialization failed; tracing disabled");
            return Guard(None);
        };
        let provider = SdkTracerProvider::builder()
            .with_resource(
                opentelemetry_sdk::Resource::builder()
                    .with_service_name(
                        std::env::var("OTEL_SERVICE_NAME").unwrap_or_else(|_| service.to_owned()),
                    )
                    .build(),
            )
            .with_batch_exporter(exporter)
            .build();
        Guard(Some(provider))
    })
    .join()
    .unwrap_or_else(|_| {
        eprintln!("OTLP exporter worker initialization failed; tracing disabled");
        Guard(None)
    })
}

pub fn layer<S>(guard: &Guard) -> Option<impl Layer<S> + use<S>>
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
{
    guard.0.as_ref().map(|p| {
        tracing_opentelemetry::layer()
            .with_tracer(p.tracer("dyson"))
            .with_filter(tracing_subscriber::filter::filter_fn(|m| {
                m.is_span() && m.target() == "dyson_otel"
            }))
    })
}

tokio::task_local! {
    /// Correlation is independent of whether local OTel export is configured.
    pub(crate) static CONVERSATION_ID: Option<String>;
}

/// Propagate correlation only to the configured Swarm proxy, never model vendors.
pub fn inject_proxy_context(
    request: reqwest::RequestBuilder,
    url: &str,
) -> reqwest::RequestBuilder {
    let Ok(base) = std::env::var("SWARM_PROXY_URL") else {
        return request;
    };
    inject_proxy_context_for_base(request, &base, url)
}

fn inject_proxy_context_for_base(
    mut request: reqwest::RequestBuilder,
    base: &str,
    url: &str,
) -> reqwest::RequestBuilder {
    use opentelemetry::{propagation::TextMapPropagator, trace::TraceContextExt};
    use tracing_opentelemetry::OpenTelemetrySpanExt;
    if !is_proxy_url(base, url) {
        return request;
    }
    if let Ok(Some(id)) = CONVERSATION_ID.try_with(Clone::clone) {
        request = request.header("x-dyson-conversation-id", id);
    }
    let context = tracing::Span::current().context();
    if !context.span().span_context().is_valid() {
        return request;
    }
    let mut headers = std::collections::HashMap::new();
    opentelemetry_sdk::propagation::TraceContextPropagator::new()
        .inject_context(&context, &mut headers);
    headers
        .into_iter()
        .fold(request, |req, (k, v)| req.header(k, v))
}

fn is_proxy_url(base: &str, target: &str) -> bool {
    let (Ok(base), Ok(target)) = (reqwest::Url::parse(base), reqwest::Url::parse(target)) else {
        return false;
    };
    base.origin() == target.origin()
        && target
            .path()
            .starts_with(&format!("{}/", base.path().trim_end_matches('/')))
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry_sdk::trace::InMemorySpanExporter;
    use tracing_subscriber::prelude::*;

    #[test]
    fn exports_only_explicit_spans_and_preserves_parentage() {
        let exporter = InMemorySpanExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();
        let guard = Guard(Some(provider.clone()));
        let subscriber = tracing_subscriber::registry().with(layer(&guard));
        tracing::subscriber::with_default(subscriber, || {
            let parent = tracing::info_span!(target: "dyson_otel", "agent.turn");
            let _entered = parent.enter();
            tracing::info!(target: "dyson_otel", secret = "must-not-export", "private event");
            let legacy = tracing::info_span!("ordinary.log", prompt = "must-not-export");
            let _legacy = legacy.enter();
            let child = tracing::info_span!(target: "dyson_otel", "tool.execute");
            let _child = child.enter();
        });
        provider.force_flush().unwrap();
        let spans = exporter.get_finished_spans().unwrap();
        assert_eq!(spans.len(), 2);
        let parent = spans.iter().find(|s| s.name == "agent.turn").unwrap();
        let child = spans.iter().find(|s| s.name == "tool.execute").unwrap();
        assert_eq!(child.parent_span_id, parent.span_context.span_id());
        assert_eq!(
            child.span_context.trace_id(),
            parent.span_context.trace_id()
        );
        assert!(spans.iter().all(|s| s.events.is_empty()));
        assert!(!format!("{spans:?}").contains("must-not-export"));
    }
}

#[test]
fn proxy_context_does_not_escape_to_other_destinations() {
    assert!(is_proxy_url(
        "https://swarm.test/llm",
        "https://swarm.test/llm/openrouter/v1/chat/completions"
    ));
    assert!(!is_proxy_url(
        "https://swarm.test/llm",
        "https://vendor.test/llm/openrouter"
    ));
    assert!(!is_proxy_url(
        "https://swarm.test/llm",
        "https://swarm.test/llm-evil/openrouter"
    ));
    assert!(!is_proxy_url(
        "https://swarm.test/llm",
        "http://swarm.test/llm/openrouter"
    ));
    assert!(!is_proxy_url(
        "https://swarm.test/llm",
        "https://swarm.test:444/llm/openrouter"
    ));
}

#[test]
fn exports_otlp_over_http_and_flushes_on_shutdown() {
    use opentelemetry_otlp::WithExportConfig;
    use std::io::{Read, Write};
    use tracing_subscriber::prelude::*;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    let server = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut socket = loop {
            if let Ok((socket, _)) = listener.accept() {
                break socket;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "no OTLP request received"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        socket
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        let mut data = Vec::new();
        loop {
            let mut buf = [0u8; 4096];
            let n = socket.read(&mut buf).unwrap();
            assert!(n > 0);
            data.extend_from_slice(&buf[..n]);
            if let Some(end) = data.windows(4).position(|w| w == b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&data[..end]).to_lowercase();
                let length: usize = headers
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:"))
                    .unwrap()
                    .trim()
                    .parse()
                    .unwrap();
                if data.len() >= end + 4 + length {
                    break;
                }
            }
        }
        socket
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .unwrap();
        data
    });
    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_http()
        .with_endpoint(format!("http://{address}/v1/traces"))
        .with_timeout(std::time::Duration::from_secs(3))
        .build()
        .unwrap();
    let provider = SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .build();
    let guard = Guard(Some(provider));
    tracing::subscriber::with_default(tracing_subscriber::registry().with(layer(&guard)), || {
        let span = tracing::info_span!(target: "dyson_otel", "agent.turn");
        let _entered = span.enter();
        tracing::info!(prompt = "secret-test-prompt", "must stay local");
    });
    drop(guard);
    let data = server.join().unwrap();
    assert!(data.starts_with(b"POST /v1/traces HTTP/1.1"));
    assert!(String::from_utf8_lossy(&data).contains("application/x-protobuf"));
    assert!(!String::from_utf8_lossy(&data).contains("secret-test-prompt"));
}

#[tokio::test(flavor = "current_thread")]
async fn initializes_and_shuts_down_inside_async_main() {
    let guard = start_provider("test");
    assert!(guard.0.is_some());
    drop(guard);
}

#[tokio::test]
async fn conversation_headers_are_task_scoped_and_only_sent_to_swarm() {
    async fn headers(id: &str, target: &str) -> reqwest::header::HeaderMap {
        CONVERSATION_ID
            .scope(Some(id.to_owned()), async {
                tokio::task::yield_now().await;
                inject_proxy_context_for_base(
                    reqwest::Client::new().post(target),
                    "https://swarm.example/llm",
                    target,
                )
                .build()
                .unwrap()
                .headers()
                .clone()
            })
            .await
    }
    let proxy = "https://swarm.example/llm/openrouter/v1/chat/completions";
    let (a, b) = tokio::join!(headers("chat-a", proxy), headers("chat-b", proxy));
    assert_eq!(a["x-dyson-conversation-id"], "chat-a");
    assert_eq!(b["x-dyson-conversation-id"], "chat-b");
    assert!(
        !headers("private-chat", "https://vendor.example/v1/chat/completions")
            .await
            .contains_key("x-dyson-conversation-id")
    );
    assert!(
        !headers("private-chat", "https://swarm.example/other")
            .await
            .contains_key("x-dyson-conversation-id")
    );
}
