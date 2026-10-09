//! `iptv-rs`: single-binary IPTV relay.

use std::{net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};

use anyhow::{Context, Result};
use clap::Parser;
use iptv_server::{router, system_clock, AppState, ChannelStore};
use iptv_upstream::{
    CmgCipherFactory, FlowOptions, LiveClient, MediaPipeline, PipelineConfig,
    ReqwestTransport, USER_AGENT,
};
use iptv_wasm::AssetBundle;
use tracing::info;

#[derive(Debug, Parser)]
#[command(name = "iptv-rs", about = "Single-binary IPTV relay")]
struct Args {
    /// Address to listen on.
    #[arg(long, default_value = "127.0.0.1")]
    host: String,
    /// Port to listen on.
    #[arg(long, default_value_t = 8787)]
    port: u16,
    /// Channel list in YAML.
    #[arg(long, default_value = "/app/channels.yaml")]
    channels: PathBuf,
    /// Directory with the WASM assets and their manifest.json.
    #[arg(long, env = "IPTV_ASSETS_DIR", default_value = "./assets")]
    assets_dir: PathBuf,
    /// Serve a built web console from this directory (disabled when unset).
    #[arg(long, env = "IPTV_WEB_DIR")]
    web_dir: Option<PathBuf>,
    /// Log at info level.
    #[arg(long, default_value_t = false)]
    verbose: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let default_filter = if args.verbose { "info" } else { "warn" };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| default_filter.into()),
        )
        .init();

    // Fail fast if the channel file is missing or malformed.
    let channels = Arc::new(ChannelStore::load(&args.channels, Duration::from_secs(1))?);

    let assets_dir = args.assets_dir.clone();
    let assets = tokio::task::spawn_blocking(move || AssetBundle::load(&assets_dir))
        .await
        .context("asset loading task failed")?
        .with_context(|| format!("load assets from {}", args.assets_dir.display()))?;
    info!(
        dir = %args.assets_dir.display(),
        modules = assets.report().modules_compiled,
        "loaded runtime assets"
    );

    let transport = Arc::new(ReqwestTransport::new(USER_AGENT, Duration::from_secs(20))?);
    let live = LiveClient::new(transport.clone(), &assets, FlowOptions::relay_defaults());
    let pipeline = MediaPipeline::new(
        live,
        transport,
        Arc::new(CmgCipherFactory::new(assets)),
        PipelineConfig::default(),
    );
    let state = AppState::new(channels, pipeline, system_clock());
    let app = router(state, args.web_dir.as_deref());

    let addr: SocketAddr = format!("{}:{}", args.host, args.port)
        .parse()
        .with_context(|| format!("invalid listen address {}:{}", args.host, args.port))?;
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("bind {addr}"))?;
    info!(%addr, "iptv-rs listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        // If the handler cannot be installed, never complete this branch.
        if tokio::signal::ctrl_c().await.is_err() {
            std::future::pending::<()>().await;
        }
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
}
