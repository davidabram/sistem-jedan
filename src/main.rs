mod config;
mod cpu;
mod decisions;
mod diagnostic;
mod js;
mod mcp;
mod memory;

use std::io;
use std::net::SocketAddr;
use std::process::ExitCode;
use std::sync::Arc;

use rmcp::ServiceExt;
use rmcp::service::QuitReason;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use tokio::task::JoinError;

use crate::config::Transport;

const HTTP_PATH: &str = "/mcp";

#[derive(Debug, thiserror::Error)]
enum Error {
    #[error("configuration error: {0}")]
    Config(#[from] config::ConfigError),
    #[error("Decisions client error: {0}")]
    Decisions(#[from] decisions::DecisionsError),
    #[error("MCP initialization failed: {0}")]
    Initialize(String),
    #[error("MCP session task failed: {0}")]
    Session(#[from] JoinError),
    #[error("failed to bind {address}: {source}")]
    Bind {
        address: SocketAddr,
        #[source]
        source: io::Error,
    },
    #[error("HTTP server failed: {0}")]
    Http(#[source] io::Error),
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    match serve().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!(message = %diagnostic::message(error), "server failed");
            ExitCode::FAILURE
        }
    }
}

async fn serve() -> Result<(), Error> {
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "starting sistem-jedan");
    let config = config::load(std::env::args_os().skip(1))?;
    let execution = config.execution;
    let client = if let Some(file) = config.file {
        tracing::info!(
            path = %file.path.display(),
            endpoint = %file.provider.endpoint,
            model = file.provider.model,
            "loaded configuration"
        );
        Some(decisions::Client::new(
            file.provider,
            execution.limits.timeout,
        )?)
    } else {
        tracing::info!("no configuration found; using default execution limits");
        None
    };
    tracing::info!(
        timeout_seconds = execution.limits.timeout.as_secs(),
        memory_limit_bytes = execution.limits.memory_limit.get(),
        vcpu_per_execution = ?execution.limits.vcpu_per_execution,
        max_concurrent_requests = execution.max_concurrent_requests,
        max_concurrent_executions = execution.max_concurrent_executions,
        max_calls_per_execution = execution.max_calls_per_execution,
        "execution limits"
    );
    let server = mcp::Server::new(execution, client);
    match config.transport {
        Transport::Stdio => serve_stdio(server).await,
        Transport::Http {
            address,
            allowed_hosts,
        } => serve_http(server, address, allowed_hosts).await,
    }
}

async fn serve_stdio(server: mcp::Server) -> Result<(), Error> {
    let service = server
        .serve(rmcp::transport::stdio())
        .await
        .map_err(|error| Error::Initialize(diagnostic::message(error)))?;
    match service.waiting().await? {
        QuitReason::Closed => tracing::info!("client disconnected"),
        QuitReason::Cancelled => tracing::info!("session cancelled"),
        QuitReason::JoinError(error) => return Err(Error::Session(error)),
        reason => tracing::info!(?reason, "session ended"),
    }
    Ok(())
}

async fn serve_http(
    server: mcp::Server,
    address: SocketAddr,
    allowed_hosts: Vec<String>,
) -> Result<(), Error> {
    let mut config = StreamableHttpServerConfig::default();
    config.allowed_hosts.extend(allowed_hosts);
    let shutdown = config.cancellation_token.clone();
    tracing::info!(allowed_hosts = ?config.allowed_hosts, "allowed Host headers");
    let service = StreamableHttpService::new(
        move || Ok(server.clone()),
        Arc::new(LocalSessionManager::default()),
        config,
    );
    let router = axum::Router::new().nest_service(HTTP_PATH, service);
    let listener = tokio::net::TcpListener::bind(address)
        .await
        .map_err(|source| Error::Bind { address, source })?;
    let address = listener.local_addr().map_err(Error::Http)?;
    tracing::info!(url = %format_args!("http://{address}{HTTP_PATH}"), "serving MCP over HTTP");
    axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            shutdown_signal().await;
            tracing::info!("shutting down");
            shutdown.cancel();
        })
        .await
        .map_err(Error::Http)
}

async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    match signal(SignalKind::terminate()) {
        Ok(mut terminate) => {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = terminate.recv() => {}
            }
        }
        Err(_) => {
            let _ = tokio::signal::ctrl_c().await;
        }
    }
}
