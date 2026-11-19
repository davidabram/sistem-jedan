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
    let service = mcp::Server.serve(rmcp::transport::stdio()).await?;
    match service.waiting().await? {
        QuitReason::Closed => tracing::info!("client disconnected"),
        QuitReason::Cancelled => tracing::info!("session cancelled"),
        QuitReason::JoinError(error) => return Err(Error::Session(error)),
        reason => tracing::info!(?reason, "session ended"),
    }
    Ok(())
}
