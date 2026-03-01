mod app;
mod config;
mod queue;
mod transfer;
mod ui;
mod unmount;
mod worker;

use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    app::run().await
}
