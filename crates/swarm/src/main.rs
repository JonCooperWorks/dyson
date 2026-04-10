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

use swarm::Hub;
use swarm::config::SwarmConfig;
use swarm::http::build_router;
use swarm::key::HubKeyPair;

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

    // Load or generate the signing key.
    let key_path = config.key_path();
    let is_new_key = !key_path.exists();
    let key = HubKeyPair::load_or_generate(&key_path)?;

    if is_new_key {
        println!("Generated new hub signing key at {}", key_path.display());
    }
    println!(
        "Hub public key (add to node config): {}",
        key.public_key_config()
    );

    // Build shared state.
    let hub = Hub::new(key, &config.data_dir)?;

    // Spawn the heartbeat reaper.
    {
        let registry = hub.registry.clone();
        let timeout = config.heartbeat_timeout;
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(15));
            loop {
                ticker.tick().await;
                let reaped = registry.reap_stale(timeout).await;
                for id in reaped {
                    tracing::warn!(node_id = %id, "reaped stale node");
                }
            }
        });
    }

    // Build the axum router.
    let app = build_router(hub.clone());
    let listener = tokio::net::TcpListener::bind(config.bind).await?;
    let local_addr = listener.local_addr()?;
    tracing::info!(%local_addr, "HTTP server listening");

    // Graceful shutdown on SIGINT / SIGTERM.
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    tracing::info!("swarm hub shut down");
    Ok(())
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
