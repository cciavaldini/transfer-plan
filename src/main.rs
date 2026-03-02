mod app;
mod config;
mod queue;
mod transfer;
mod ui;
mod unmount;
mod worker;

use anyhow::Result;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing with RUST_LOG env var (default: info level)
    let env_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("transfer_plan=info"));
    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_target(false)
        .init();

    tracing::debug!("Starting transfer-plan");
    app::run().await
}
