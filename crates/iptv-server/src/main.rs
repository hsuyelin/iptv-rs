//! `iptv-rs`: single-binary IPTV relay.

use std::{net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};

use anyhow::{Context, Result};
use clap::Parser;
use iptv_server::{
    logging, probe, router, system_clock, AdminGate, AdminKey, AppState, ChannelStore,
};
use iptv_upstream::{
    CmgCipherFactory, FlowOptions, LiveClient, MediaPipeline, PipelineConfig,
    ReqwestTransport, USER_AGENT,
};
use iptv_wasm::AssetBundle;
use tracing::{debug, info, warn};

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
    /// Ask the relay on `--port` for `/health` and exit 0 when it answers 200 (Docker health checks).
    #[arg(long)]
    healthcheck: bool,
    /// More log detail: `-v` adds debug, `-vv` adds trace (RUST_LOG overrides both).
    #[arg(short, long, action = clap::ArgAction::Count)]
    verbose: u8,
}

/// Environment variable that holds the administrator key. When it is not set, a random
/// key is generated for this run and printed once.
const ADMIN_KEY_VAR: &str = "IPTV_ADMIN_KEY";

/// Reads the administrator key from the environment or generates one. The second value
/// is true when the key was generated.
fn admin_key() -> Result<(AdminKey, bool)> {
    match std::env::var(ADMIN_KEY_VAR) {
        Ok(value) if !value.is_empty() => {
            let key = AdminKey::parse(&value)
                .with_context(|| format!("{ADMIN_KEY_VAR} is not usable"))?;
            Ok((key, false))
        }
        _ => Ok((AdminKey::generate(), true)),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    if args.healthcheck {
        let addr = SocketAddr::from(([127, 0, 0, 1], args.port));
        return probe::probe(addr).await.map_err(Into::into);
    }
    logging::init(args.verbose);
    info!(
        version = env!("CARGO_PKG_VERSION"),
        host = %args.host,
        port = args.port,
        channels = %args.channels.display(),
        assets_dir = %args.assets_dir.display(),
        web_dir = ?args.web_dir,
        verbosity = args.verbose,
        pid = std::process::id(),
        "starting iptv-rs"
    );

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
    let (key, generated) = admin_key()?;
    let clock = system_clock();
    let admin = Arc::new(AdminGate::with_defaults(key.clone(), Arc::clone(&clock)));
    let state = AppState::new(channels, pipeline, admin, clock);
    let app = router(state, args.web_dir.as_deref());

    let addr: SocketAddr = format!("{}:{}", args.host, args.port)
        .parse()
        .with_context(|| format!("invalid listen address {}:{}", args.host, args.port))?;
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("bind {addr}"))?;
    if generated {
        // Printed straight to the terminal, not through the log: the key is a secret and
        // must not end up in collected logs. It changes on every start; set the variable
        // to keep one.
        eprintln!(
            "\nAdministrator key (generated for this run; set {ADMIN_KEY_VAR} to choose your own):\n  {}\nOpen the console at http://{addr}/<key> to see the admin pages.\n",
            key.expose()
        );
        warn!("administrator key was generated for this run");
    } else {
        info!("administrator key taken from {ADMIN_KEY_VAR}");
    }
    info!(%addr, "iptv-rs listening");
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await?;
    info!("iptv-rs stopped");
    Ok(())
}

async fn shutdown_signal() {
    debug!("waiting for a shutdown signal");
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
    info!("shutdown signal received; draining connections");
}
