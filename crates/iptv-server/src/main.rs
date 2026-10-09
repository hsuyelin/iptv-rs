//! `iptv-rs`: single-binary IPTV relay.

use std::{net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};

use anyhow::{Context, Result};
use clap::Parser;
use iptv_server::{
    logging, probe, router, system_clock, AdminGate, AdminKey, AppState, ChannelStore,
    Compat, FfmpegTranscoder, Settings,
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
    /// An ffmpeg program with libx264. When set, `?profile=compat` serves a lighter stream
    /// (720p Main profile, no B-frames, a keyframe at least every 2 s) for old devices.
    #[arg(long, env = "IPTV_COMPAT_FFMPEG")]
    compat_ffmpeg: Option<PathBuf>,
    /// Whether to offer the lighter stream when an ffmpeg is set: `off` (or `0`, `false`,
    /// `no`) turns it off, for example in a Docker image that ships ffmpeg. On by default.
    #[arg(
        long,
        env = "IPTV_COMPAT",
        default_value = "on",
        default_missing_value = "on",
        num_args = 0..=1,
        require_equals = false,
        value_parser = parse_toggle,
    )]
    compat: bool,
    /// Tallest picture of the compatibility stream, in pixels.
    #[arg(long, env = "IPTV_COMPAT_HEIGHT", default_value_t = 720)]
    compat_height: u32,
    /// Video bit rate of the compatibility stream, in kilobits per second.
    #[arg(long, env = "IPTV_COMPAT_KBPS", default_value_t = 2500)]
    compat_kbps: u32,
    /// Ask the relay on `--port` for `/health` and exit 0 when it answers 200 (Docker health checks).
    #[arg(long)]
    healthcheck: bool,
    /// More log detail: `-v` adds debug, `-vv` adds trace (RUST_LOG overrides both).
    #[arg(short, long, action = clap::ArgAction::Count)]
    verbose: u8,
}

/// Reads a switch: `on`, `1`, `true`, `yes` or `off`, `0`, `false`, `no` (also `y`, `n`, `t`,
/// `f`, in any case). An empty value counts as not given, which is on: a variable that an
/// orchestrator left empty must not stop the server from starting.
fn parse_toggle(value: &str) -> Result<bool, String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "" | "on" | "1" | "true" | "yes" | "y" | "t" => Ok(true),
        "off" | "0" | "false" | "no" | "n" | "f" => Ok(false),
        other => Err(format!("expected on or off, not `{other}`")),
    }
}

/// Re-encoded segments kept for players that ask for the same one again.
const COMPAT_CACHE_SEGMENTS: usize = 24;

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
        compat = args.compat_ffmpeg.is_some() && args.compat,
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
    let state = match (&args.compat_ffmpeg, args.compat) {
        (Some(program), true) => state.with_compat(compat(program, &args).await?),
        (Some(_), false) => {
            info!("the compatibility stream is switched off (IPTV_COMPAT / --compat)");
            state
        }
        (None, _) => state,
    };
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

/// Prepares the compatibility rendition, failing at start if the encoder cannot be used.
async fn compat(program: &std::path::Path, args: &Args) -> Result<Arc<Compat>> {
    let settings = Settings {
        height: args.compat_height,
        video_kbps: args.compat_kbps,
        ..Settings::default()
    };
    let transcoder = FfmpegTranscoder::new(program.to_path_buf(), settings);
    transcoder.check().await.with_context(|| {
        format!("--compat-ffmpeg {} cannot be used", program.display())
    })?;
    // Each encode may use two threads, so keep the number running at once well under the cores.
    let cores = std::thread::available_parallelism().map_or(2, usize::from);
    let parallel = (cores / 2).clamp(1, 4);
    info!(
        ffmpeg = %program.display(),
        height = args.compat_height,
        kbps = args.compat_kbps,
        parallel,
        "the compatibility stream is on (?profile=compat)"
    );
    Ok(Arc::new(Compat::new(
        Arc::new(transcoder),
        parallel,
        COMPAT_CACHE_SEGMENTS,
    )))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(extra: &[&str]) -> Result<Args, clap::Error> {
        Args::try_parse_from(["iptv-rs"].into_iter().chain(extra.iter().copied()))
    }

    #[test]
    fn the_lighter_stream_is_on_by_default_and_needs_an_ffmpeg() {
        let args = parse(&[]).unwrap();
        assert!(args.compat);
        assert!(args.compat_ffmpeg.is_none());
    }

    #[test]
    fn every_usual_spelling_of_off_and_on_is_understood() {
        for off in ["off", "OFF", "0", "false", "no", "n", "f"] {
            assert!(!parse(&["--compat", off]).unwrap().compat, "{off}");
            assert!(
                !parse(&[&format!("--compat={off}")]).unwrap().compat,
                "{off}"
            );
        }
        for on in ["on", "ON", "1", "true", "yes", "y", "t"] {
            assert!(parse(&["--compat", on]).unwrap().compat, "{on}");
        }
    }

    #[test]
    fn the_bare_flag_means_on() {
        assert!(parse(&["--compat"]).unwrap().compat);
        assert!(parse(&["--compat", "--port", "9000"]).unwrap().compat);
    }

    #[test]
    fn a_word_that_is_neither_is_refused_rather_than_guessed() {
        assert!(parse(&["--compat", "maybe"]).is_err());
        assert!(parse(&["--compat=2"]).is_err());
    }

    #[test]
    fn an_empty_value_counts_as_not_given() {
        assert!(parse(&["--compat="]).unwrap().compat);
        assert!(parse(&["--compat", " "]).unwrap().compat);
    }

    #[test]
    fn the_encoder_options_have_their_defaults() {
        let args = parse(&["--compat-ffmpeg", "/app/ffmpeg"]).unwrap();
        assert_eq!(
            args.compat_ffmpeg.as_deref(),
            Some(std::path::Path::new("/app/ffmpeg"))
        );
        assert_eq!((args.compat_height, args.compat_kbps), (720, 2500));
    }
}
