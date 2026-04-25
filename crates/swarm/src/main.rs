//! Dyson swarm hub — binary entry point.
//!
//! ```text
//! swarm --bind 0.0.0.0:8080 --data-dir ./hub-data
//! ```

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use clap::Parser;
use tracing_subscriber::EnvFilter;

use swarm::{Hub, McpApiKey};
use swarm::config::SwarmConfig;
use swarm::http::build_router;
use swarm::key::HubKeyPair;
use swarm::tls;

/// CLI arguments.
#[derive(Debug, Parser)]
#[command(name = "swarm", about = "The Dyson swarm hub")]
struct Args {
    /// Address to bind the HTTP server to.
    #[arg(long, default_value = "127.0.0.1:8080")]
    bind: SocketAddr,

    /// Directory where `hub.key` and `blobs/` live.
    #[arg(long, default_value = "./hub-data")]
    data_dir: PathBuf,

    /// Reap nodes whose last heartbeat is older than this many seconds.
    #[arg(long, default_value_t = 90)]
    heartbeat_timeout_secs: u64,

    /// `tracing` env filter.
    #[arg(long, default_value = "info")]
    log_level: String,

    // -- TLS --

    /// Path to TLS certificate chain (PEM format).  Use with --private-key.
    #[arg(long, requires = "private_key", conflicts_with = "letsencrypt")]
    cert: Option<PathBuf>,

    /// Path to TLS private key (PEM format).  Use with --cert.
    #[arg(long, requires = "cert", conflicts_with = "letsencrypt")]
    private_key: Option<PathBuf>,

    /// Use Let's Encrypt for automatic TLS certificates (TLS-ALPN-01 challenge).
    #[arg(long, requires = "domain")]
    letsencrypt: bool,

    /// Domain name (for Let's Encrypt certificate provisioning).
    #[arg(long)]
    domain: Option<String>,

    /// Contact email for Let's Encrypt registration.
    #[arg(long, requires = "letsencrypt")]
    letsencrypt_email: Option<String>,

    /// Directory to cache Let's Encrypt certificates.
    #[arg(long, requires = "letsencrypt", default_value = ".swarm-certs")]
    cert_cache_dir: PathBuf,

    /// Allow running without TLS (plain HTTP) on external interfaces.
    /// Not required for localhost/127.0.0.1/::1 — those skip TLS automatically.
    #[arg(long)]
    dangerous_no_tls: bool,

    /// Allow running without authentication on external interfaces.
    /// Not required for localhost/127.0.0.1/::1 — those skip the check automatically.
    #[arg(long)]
    dangerous_no_auth: bool,

    /// Argon2id PHC hash of a static API key for MCP authentication.
    #[arg(long)]
    mcp_api_key_hash: Option<String>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    // Logging first so subsequent setup is visible.
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(args.log_level.as_str()));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();

    tracing::info!("swarm hub starting");

    let config = SwarmConfig {
        bind: args.bind,
        data_dir: args.data_dir.clone(),
        heartbeat_timeout: Duration::from_secs(args.heartbeat_timeout_secs),
    };

    // Ensure data dir exists.
    std::fs::create_dir_all(&config.data_dir)?;

    // Load the signing key.  The hub itself never generates one — that
    // is the job of the `swarm-keygen` binary.  Failing loudly here
    // keeps key provisioning explicit and out of the hot path.
    let key_path = config.key_path();
    let key = HubKeyPair::load(&key_path).map_err(|e| {
        eprintln!("error: {e}");
        eprintln!(
            "\nGenerate one with:\n    swarm-keygen --out {}",
            key_path.display()
        );
        e
    })?;
    println!(
        "Hub public key (add to node config): {}",
        key.public_key_config()
    );

    // Validate TLS and auth configuration before consuming args.
    let tls_mode = validate_tls(&args)?;
    validate_auth(&args)?;

    // Parse + validate the API key hash if provided (fail fast).
    let mcp_api_key = args
        .mcp_api_key_hash
        .map(|h| McpApiKey::new(h).map_err(|e| format!("--mcp-api-key-hash: {e}")))
        .transpose()?;

    // Build shared state.
    let hub = Hub::new(key, &config.data_dir, mcp_api_key).await?;

    // Spawn the heartbeat reaper.  It exits when the hub broadcasts
    // shutdown so tokio's runtime can terminate cleanly on Ctrl-C.
    //
    // The same ticker also reaps terminal TaskRecords older than 24h
    // from the TaskStore — no separate background task needed.
    {
        let hub_for_reaper = hub.clone();
        let timeout = config.heartbeat_timeout;
        let task_ttl = Duration::from_secs(24 * 60 * 60);
        let shutdown_fut = hub.shutdown_notified();
        tokio::spawn(async move {
            tokio::pin!(shutdown_fut);
            // Reaper cadence.  A 15s tick against a 24h task TTL means an
            // individual task may survive the deadline by up to one
            // interval before being collected — acceptable given the
            // TTL is already an imprecise "don't hold records forever"
            // bound rather than a strict deletion SLA.  A result that
            // lands mid-iteration bumps `last_update` on the record,
            // which the reaper re-reads under the task store's lock on
            // the next tick, so there is no loss of freshly-active tasks.
            let mut ticker = tokio::time::interval(Duration::from_secs(15));
            // Idempotency sweep cadence: every 20 ticks (~5 min).  The
            // index also sweeps opportunistically on insert past its
            // soft cap, so this is the belt for write-silent hubs.
            const IDEMPOTENCY_SWEEP_EVERY_N_TICKS: u64 = 20;
            let mut tick_count: u64 = 0;
            loop {
                tokio::select! {
                    _ = ticker.tick() => {
                        let reaped = hub_for_reaper.registry.reap_stale(timeout).await;
                        for id in reaped {
                            tracing::warn!(node_id = %id, "reaped stale node");
                        }
                        let reaped_tasks = hub_for_reaper.tasks.reap(task_ttl).await;
                        if reaped_tasks > 0 {
                            tracing::info!(reaped_tasks, "reaped terminal tasks past TTL");
                        }
                        tick_count = tick_count.wrapping_add(1);
                        if tick_count.is_multiple_of(IDEMPOTENCY_SWEEP_EVERY_N_TICKS) {
                            hub_for_reaper.idempotency.sweep().await;
                        }
                    }
                    _ = &mut shutdown_fut => {
                        tracing::debug!("reaper shutting down");
                        break;
                    }
                }
            }
        });
    }

    // Build the axum router.
    let app = build_router(hub.clone());
    let addr = config.bind.to_string();

    // Graceful shutdown: when a signal arrives, broadcast shutdown through
    // the hub so every open SSE stream ends (via `take_until`), then let
    // axum/hyper drain the remaining connections.
    let hub_for_shutdown = hub.clone();
    let graceful = async move {
        shutdown_signal().await;
        tracing::info!("closing SSE streams");
        hub_for_shutdown.trigger_shutdown();
    };

    match tls_mode {
        tls::TlsMode::None => {
            let listener = tokio::net::TcpListener::bind(&addr).await?;
            tracing::info!(addr = %listener.local_addr()?, "HTTP server listening (plain)");
            axum::serve(listener, app)
                .with_graceful_shutdown(graceful)
                .await?;
        }
        tls::TlsMode::Manual { ref cert, ref key } => {
            tracing::info!(%addr, "HTTPS server listening (manual TLS)");
            tls::serve_manual_tls(app, &addr, cert, key, graceful).await?;
        }
        tls::TlsMode::LetsEncrypt { ref domain, ref email, ref cache_dir } => {
            tracing::info!(%addr, %domain, "HTTPS server listening (Let's Encrypt)");
            tls::serve_letsencrypt(
                app, &addr, domain, email.as_deref(), cache_dir, graceful,
            ).await?;
        }
    }

    tracing::info!("swarm hub shut down");
    Ok(())
}

const fn is_loopback(addr: &SocketAddr) -> bool {
    addr.ip().is_loopback()
}

fn validate_tls(args: &Args) -> Result<tls::TlsMode, Box<dyn std::error::Error>> {
    if args.letsencrypt {
        let domain = args.domain.clone().expect("clap requires --domain with --letsencrypt");
        return Ok(tls::TlsMode::LetsEncrypt {
            domain,
            email: args.letsencrypt_email.clone(),
            cache_dir: args.cert_cache_dir.clone(),
        });
    }

    if let (Some(cert), Some(key)) = (&args.cert, &args.private_key) {
        return Ok(tls::TlsMode::Manual {
            cert: cert.clone(),
            key: key.clone(),
        });
    }

    if args.dangerous_no_tls || is_loopback(&args.bind) {
        return Ok(tls::TlsMode::None);
    }

    Err(format!(
        "TLS is required when binding to a non-localhost address ({}).\n\n\
         Provide TLS certificates:\n  \
           --cert <path> --private-key <path>\n\n\
         Or use Let's Encrypt:\n  \
           --letsencrypt --domain <domain>\n\n\
         Or explicitly disable TLS (not recommended):\n  \
           --dangerous-no-tls",
        args.bind
    ).into())
}

fn validate_auth(args: &Args) -> Result<(), Box<dyn std::error::Error>> {
    if is_loopback(&args.bind) || args.dangerous_no_auth || args.mcp_api_key_hash.is_some() {
        return Ok(());
    }

    Err(format!(
        "No authentication configured for non-localhost address ({}).\n\n\
         The hub's MCP and registration endpoints are open. Either:\n  \
           --mcp-api-key-hash <PHC_STRING>   (gates both /mcp and /swarm/register)\n  \
           --dangerous-no-auth",
        args.bind
    ).into())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }

    tracing::info!("shutdown signal received");
}
