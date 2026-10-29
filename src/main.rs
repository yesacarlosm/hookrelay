use std::{net::SocketAddr, time::Duration};

use anyhow::Context;
use clap::{Parser, Subcommand};
use hookrelay::{AppState, config::Config};
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
        /// Maximum age (and clock skew) of a Stripe signature timestamp, in seconds.
        #[arg(long, env = "HOOKRELAY_STRIPE_TOLERANCE_SECS", default_value_t = 300)]
        stripe_tolerance_secs: u64,
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
        Command::Serve {
            addr,
            database_url,
            stripe_tolerance_secs,
        } => {
            // Non-UTF-8 vars can't be ours, and `env::vars()` would panic on them.
            let vars = std::env::vars_os().filter_map(|(key, value)| {
                Some((key.into_string().ok()?, value.into_string().ok()?))
            });
            let config = Config::from_vars(vars, Duration::from_secs(stripe_tolerance_secs))
                .context("invalid signature verification config")?;
            serve(addr, &database_url, config).await
        }
    }
}

async fn serve(addr: SocketAddr, database_url: &str, config: Config) -> anyhow::Result<()> {
    for (source, scheme) in config.verified_sources() {
        tracing::info!(source, ?scheme, "signature verification enabled");
    }

    let pool = hookrelay::db::connect(database_url)
        .await
        .with_context(|| format!("failed to open database {database_url}"))?;

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("failed to bind {addr}"))?;
    tracing::info!(%addr, "hookrelay listening");

    axum::serve(listener, hookrelay::router(AppState::new(pool, config)))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
            tracing::info!("shutting down");
        })
        .await?;
    Ok(())
}
