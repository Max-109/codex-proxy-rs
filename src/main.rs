mod auth;
mod cli;
mod codex;
mod config;
mod error;
mod openai;
mod server;

use clap::Parser;
use cli::Cli;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "codex_proxy=info".into()),
        )
        .init();

    Cli::parse().run().await
}
