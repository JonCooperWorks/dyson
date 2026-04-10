// ===========================================================================
// TLS support — manual certificates and Let's Encrypt (ACME).
//
// Two modes:
//
// 1. Manual: the user provides --cert and --private-key PEM files.
//    We load them into a rustls ServerConfig and serve TLS directly.
//
// 2. Let's Encrypt: the user provides --domain and --letsencrypt.
//    We use rustls-acme to automatically provision and renew certificates
//    via the TLS-ALPN-01 challenge (no separate port 80 needed).
//
// Both modes build a TlsAcceptor and delegate to serve_tls_loop(), which
// handles TCP accept → TLS handshake → hyper/axum request serving.
// ===========================================================================

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::Router;
use tokio::net::TcpListener;
use tokio::task::JoinSet;
use tokio_rustls::TlsAcceptor;

type BoxError = Box<dyn std::error::Error>;

/// TLS configuration mode, determined from CLI args.
pub enum TlsMode {
    /// No TLS — serve plain HTTP.
    None,
    /// Manual TLS — user-provided certificate and private key.
    Manual { cert: PathBuf, key: PathBuf },
    /// Let's Encrypt automatic TLS via ACME TLS-ALPN-01.
    LetsEncrypt {
        domain: String,
        email: Option<String>,
        cache_dir: PathBuf,
    },
}

// ---------------------------------------------------------------------------
// Manual TLS.
// ---------------------------------------------------------------------------

/// Serve HTTPS with user-provided certificate and private key files.
pub async fn serve_manual_tls(
    app: Router,
    addr: &str,
    cert_path: &Path,
    key_path: &Path,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<(), BoxError> {
    let acceptor = build_manual_acceptor(cert_path, key_path)?;
    let listener = TcpListener::bind(addr).await?;
    serve_tls_loop(listener, acceptor, app, shutdown).await
}

/// Load PEM certificate and key files into a TlsAcceptor.
fn build_manual_acceptor(cert_path: &Path, key_path: &Path) -> Result<TlsAcceptor, BoxError> {
    let cert_file = std::fs::File::open(cert_path)?;
    let mut cert_reader = std::io::BufReader::new(cert_file);
    let certs: Vec<_> = rustls_pemfile::certs(&mut cert_reader).collect::<Result<_, _>>()?;
    if certs.is_empty() {
        return Err(format!("no certificates found in {}", cert_path.display()).into());
    }

    let key_file = std::fs::File::open(key_path)?;
    let mut key_reader = std::io::BufReader::new(key_file);
    let key = rustls_pemfile::private_key(&mut key_reader)?
        .ok_or_else(|| format!("no private key found in {}", key_path.display()))?;

    let mut server_config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)?;
    server_config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];

    Ok(TlsAcceptor::from(Arc::new(server_config)))
}

// ---------------------------------------------------------------------------
// Let's Encrypt (ACME).
// ---------------------------------------------------------------------------

/// Serve HTTPS with Let's Encrypt automatic certificate provisioning.
///
/// Uses rustls-acme with TLS-ALPN-01 challenge validation — the ACME
/// challenge and regular TLS traffic share the same port.  Certificates
/// are cached on disk so restarts don't trigger re-issuance.
pub async fn serve_letsencrypt(
    app: Router,
    addr: &str,
    domain: &str,
    email: Option<&str>,
    cache_dir: &Path,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<(), BoxError> {
    use rustls_acme::AcmeConfig;
    use rustls_acme::caches::DirCache;
    use tokio_stream::StreamExt;

    let mut acme = AcmeConfig::new([domain])
        .cache(DirCache::new(cache_dir.to_owned()))
        .directory_lets_encrypt(true);

    if let Some(email) = email {
        acme = acme.contact_push(format!("mailto:{email}"));
    }

    let state = acme.state();
    let resolver = state.resolver();

    tokio::spawn(async move {
        let mut state = std::pin::pin!(state);
        loop {
            match state.next().await {
                Some(Ok(ok)) => tracing::info!(?ok, "acme event"),
                Some(Err(err)) => tracing::error!(?err, "acme error"),
                None => break,
            }
        }
    });

    let mut server_config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_cert_resolver(resolver);
    server_config.alpn_protocols =
        vec![b"h2".to_vec(), b"http/1.1".to_vec(), b"acme-tls/1".to_vec()];

    let acceptor = TlsAcceptor::from(Arc::new(server_config));
    let listener = TcpListener::bind(addr).await?;

    serve_tls_loop(listener, acceptor, app, shutdown).await
}

// ---------------------------------------------------------------------------
// Shared TLS accept loop with graceful shutdown.
// ---------------------------------------------------------------------------

const DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
const ACCEPT_ERROR_BACKOFF: std::time::Duration = std::time::Duration::from_millis(10);

async fn serve_tls_loop(
    listener: TcpListener,
    acceptor: TlsAcceptor,
    app: Router,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<(), BoxError> {
    use hyper_util::rt::{TokioExecutor, TokioIo};
    use hyper_util::server::graceful::GracefulShutdown;
    use tower_service::Service;

    let graceful = GracefulShutdown::new();
    let mut connections = JoinSet::new();
    let shutdown = std::pin::pin!(shutdown);

    tokio::select! {
        _ = async {
            loop {
                match listener.accept().await {
                    Ok((tcp_stream, remote_addr)) => {
                        let acceptor = acceptor.clone();
                        let app = app.clone();
                        let watcher = graceful.watcher();

                        connections.spawn(async move {
                            let tls_stream = match acceptor.accept(tcp_stream).await {
                                Ok(stream) => stream,
                                Err(err) => {
                                    tracing::warn!(%remote_addr, %err, "TLS handshake failed");
                                    return;
                                }
                            };

                            let stream = TokioIo::new(tls_stream);
                            let hyper_service = hyper::service::service_fn(
                                move |request: hyper::Request<hyper::body::Incoming>| {
                                    let mut app = app.clone();
                                    async move { app.call(request.map(axum::body::Body::new)).await }
                                },
                            );

                            let builder =
                                hyper_util::server::conn::auto::Builder::new(TokioExecutor::new());
                            let conn = builder
                                .serve_connection_with_upgrades(stream, hyper_service);

                            if let Err(err) = watcher.watch(conn).await {
                                tracing::error!(%remote_addr, %err, "error serving connection");
                            }
                        });
                    }
                    Err(err) => {
                        tracing::error!(%err, "TCP accept error");
                        tokio::time::sleep(ACCEPT_ERROR_BACKOFF).await;
                    }
                }
            }
        } => {},

        _ = shutdown => {
            tracing::info!(in_flight = connections.len(), "TLS server shutting down");
        },
    }

    tokio::select! {
        _ = graceful.shutdown() => {
            tracing::info!("all TLS connections drained");
        }
        _ = tokio::time::sleep(DRAIN_TIMEOUT) => {
            tracing::warn!(
                remaining = connections.len(),
                "drain timeout reached, dropping remaining connections",
            );
        }
    }

    Ok(())
}
