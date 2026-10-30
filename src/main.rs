use std::{net::SocketAddr, time::Duration};

use anyhow::Context;
use clap::{Parser, Subcommand};
use hookrelay::{AppState, config::Config, forward::ForwardSettings};
use tokio::sync::watch;
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
        /// Attempts per forwarded hook, including the first, before it is dead-lettered.
        #[arg(long, env = "HOOKRELAY_FORWARD_MAX_ATTEMPTS", default_value_t = 8,
              value_parser = clap::value_parser!(u32).range(1..))]
        forward_max_attempts: u32,
        /// Timeout for each forwarding attempt, in seconds.
        #[arg(long, env = "HOOKRELAY_FORWARD_TIMEOUT_SECS", default_value_t = 10)]
        forward_timeout_secs: u64,
        /// Delay before the first retry, in milliseconds. Doubles on each retry.
        #[arg(long, env = "HOOKRELAY_RETRY_BASE_MS", default_value_t = 1000)]
        retry_base_ms: u64,
        /// Maximum delay between retries, in seconds.
        #[arg(long, env = "HOOKRELAY_RETRY_MAX_SECS", default_value_t = 3600)]
        retry_max_secs: u64,
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
            forward_max_attempts,
            forward_timeout_secs,
            retry_base_ms,
            retry_max_secs,
        } => {
            // Non-UTF-8 vars can't be ours, and `env::vars()` would panic on them.
            let vars = std::env::vars_os().filter_map(|(key, value)| {
                Some((key.into_string().ok()?, value.into_string().ok()?))
            });
            let config = Config::from_vars(vars, Duration::from_secs(stripe_tolerance_secs))
                .context("invalid configuration")?;
            let forward = ForwardSettings {
                max_attempts: forward_max_attempts,
                timeout: Duration::from_secs(forward_timeout_secs),
                retry_base: Duration::from_millis(retry_base_ms),
                retry_max: Duration::from_secs(retry_max_secs),
            };
            serve(addr, &database_url, config, forward).await
        }
    }
}

async fn serve(
    addr: SocketAddr,
    database_url: &str,
    config: Config,
    forward: ForwardSettings,
) -> anyhow::Result<()> {
    for (source, scheme) in config.verified_sources() {
        tracing::info!(source, ?scheme, "signature verification enabled");
    }
    for (source, target) in config.targets() {
        // Origin only: the path or query may carry a token.
        let target = target.origin().ascii_serialization();
        tracing::info!(source, %target, "forwarding enabled");
    }

    let pool = hookrelay::db::connect(database_url)
        .await
        .with_context(|| format!("failed to open database {database_url}"))?;

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("failed to bind {addr}"))?;
    tracing::info!(%addr, "hookrelay listening");

    let client = hookrelay::forward::client(&forward).context("failed to build HTTP client")?;
    let state = AppState::new(pool.clone(), config);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let forwarder = tokio::spawn(hookrelay::forward::run(
        pool,
        client,
        forward,
        state.wake_forwarder.clone(),
        shutdown_signal(shutdown_rx.clone()),
    ));

    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        tracing::info!("shutting down");
        let _ = shutdown_tx.send(true);
    });

    axum::serve(listener, hookrelay::router(state))
        .with_graceful_shutdown(shutdown_signal(shutdown_rx))
        .await?;
    forwarder.await.context("forwarder task failed")?;
    Ok(())
}

async fn shutdown_signal(mut rx: watch::Receiver<bool>) {
    // An error means the sender is gone, which only happens on the way out anyway.
    let _ = rx.wait_for(|stop| *stop).await;
}
