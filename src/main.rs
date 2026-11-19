mod config;
mod js;
mod mcp;

use rmcp::ServiceExt;
use rmcp::service::{QuitReason, ServerInitializeError};
use tokio::task::JoinError;

#[expect(
    clippy::large_enum_variant,
    reason = "constructed at most once, on the exit path; boxing would only add an allocation"
)]
#[derive(Debug, thiserror::Error)]
enum Error {
    #[error("configuration error: {0}")]
    Config(#[from] config::ConfigError),
    #[error("MCP initialization failed: {0}")]
    Initialize(#[from] ServerInitializeError),
    #[error("MCP session task failed: {0}")]
    Session(#[from] JoinError),
}

#[expect(
    clippy::result_large_err,
    reason = "the error is returned once from main and never propagated through hot paths"
)]
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Error> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    tracing::info!(version = env!("CARGO_PKG_VERSION"), "starting sistem-jedan");
    let config = config::load(std::env::args_os().skip(1))
        .inspect_err(|error| tracing::error!(%error, "invalid configuration"))?;
    if let Some(file) = &config.file {
        tracing::info!(
            path = %file.path.display(),
            endpoint = file.provider.endpoint,
            model = file.provider.model,
            credentials = %file.provider.credential_source,
            "loaded configuration"
        );
    } else {
        tracing::info!("no configuration found; using default execution limits");
    }
    let execution = config.execution;
    tracing::info!(
        timeout_seconds = execution.limits.timeout.as_secs(),
        memory_limit_bytes = execution.limits.memory_limit,
        max_concurrent_requests = execution.max_concurrent_requests,
        max_calls_per_execution = execution.max_calls_per_execution,
        "execution limits"
    );
    let service = mcp::Server::new(execution.limits)
        .serve(rmcp::transport::stdio())
        .await?;
    match service.waiting().await? {
        QuitReason::Closed => tracing::info!("client disconnected"),
        QuitReason::Cancelled => tracing::info!("session cancelled"),
        QuitReason::JoinError(error) => return Err(Error::Session(error)),
        reason => tracing::info!(?reason, "session ended"),
    }
    Ok(())
}
