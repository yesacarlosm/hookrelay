use std::net::SocketAddr;

use anyhow::Context;
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the HTTP server.
    Serve {
        /// Address to listen on.
        #[arg(long, env = "HOOKRELAY_ADDR", default_value = "127.0.0.1:8080")]
        addr: SocketAddr,
        /// SQLite database URL.
        #[arg(
            long,
            env = "HOOKRELAY_DATABASE_URL",
            default_value = "sqlite://hookrelay.db"
        )]
        database_url: String,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    match Cli::parse().command {
        Command::Serve { addr, database_url } => serve(addr, &database_url).await,
    }
}

async fn serve(addr: SocketAddr, database_url: &str) -> anyhow::Result<()> {
    let pool = hookrelay::db::connect(database_url)
        .await
        .with_context(|| format!("failed to open database {database_url}"))?;

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("failed to bind {addr}"))?;
    tracing::info!(%addr, "hookrelay listening");

    axum::serve(listener, hookrelay::router(pool))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
            tracing::info!("shutting down");
        })
        .await?;
    Ok(())
}
