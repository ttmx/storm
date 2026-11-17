//! `stormd`: headless storm engine, controlled by the storm desktop app over HTTP.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context;
use clap::Parser;
use storm_engine::{EngineConfig, LocalBackend};
use storm_server::self_update::SelfUpdate;

#[derive(Parser, Debug)]
#[command(name = "stormd", version, about = "Headless storm torrent engine")]
struct Args {
    /// Address to listen on.
    #[arg(long, env = "STORMD_LISTEN", default_value_t = SocketAddr::from(([0, 0, 0, 0], storm_proto::DEFAULT_PORT)))]
    listen: SocketAddr,

    /// Where the database, session state and token live.
    #[arg(long, env = "STORMD_DATA_DIR")]
    data_dir: Option<PathBuf>,

    /// Default download folder (only used until changed in settings).
    #[arg(long, env = "STORMD_DOWNLOAD_DIR")]
    download_dir: Option<PathBuf>,

    /// API token. If unset, one is generated once and stored in `<data-dir>/token`.
    #[arg(long, env = "STORMD_TOKEN", hide_env_values = true)]
    token: Option<String>,

    /// Don't let the app update this server (e.g. when a package manager or
    /// container image owns the binary).
    #[arg(long, env = "STORMD_NO_SELF_UPDATE")]
    no_self_update: bool,
}

fn main() -> anyhow::Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    let restart = runtime.block_on(run())?;
    // Let the engine's tasks finish before handing over the process.
    runtime.shutdown_timeout(std::time::Duration::from_secs(5));
    match restart {
        Some(exe) => restart_into(&exe),
        None => Ok(()),
    }
}

/// Runs the server. Returns the exe to restart into after an update.
async fn run() -> anyhow::Result<Option<PathBuf>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,librqbit=warn,librqbit_dht=warn".into()),
        )
        .init();
    let args = Args::parse();

    let defaults = EngineConfig::platform_default()?;
    let data_dir = args.data_dir.unwrap_or_else(|| defaults.data_dir.join("stormd"));
    let config =
        EngineConfig { default_download_dir: args.download_dir.unwrap_or(defaults.default_download_dir), data_dir };
    std::fs::create_dir_all(&config.data_dir)?;

    let (token, token_source) = match args.token {
        Some(t) if !t.trim().is_empty() => (t.trim().to_string(), None),
        _ => load_or_create_token(&config.data_dir.join("token"))?,
    };

    let updater: Option<Arc<SelfUpdate>> = if args.no_self_update {
        None
    } else {
        SelfUpdate::for_this_build().inspect_err(|why| tracing::info!("updates from the app are off: {why}")).ok()
    };

    let backend = LocalBackend::start(config.clone()).await?;
    let app = storm_server::router_with_updates(backend.clone(), token.clone(), updater.clone());
    let listener =
        tokio::net::TcpListener::bind(args.listen).await.with_context(|| format!("binding {}", args.listen))?;

    tracing::info!("data dir: {}", config.data_dir.display());
    tracing::info!("listening on http://{}", listener.local_addr()?);
    match token_source {
        Some(path) => tracing::info!("API token (stored in {}): {token}", path.display()),
        None => tracing::info!("API token: provided via --token / STORMD_TOKEN"),
    }

    let restart = Arc::new(std::sync::Mutex::new(None));
    let stopping = Arc::new(tokio::sync::Notify::new());
    let stop = {
        let (updater, restart, stopping) = (updater.clone(), restart.clone(), stopping.clone());
        async move {
            let update = async {
                match &updater {
                    Some(u) => u.restart_requested().await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                _ = tokio::signal::ctrl_c() => tracing::info!("shutting down"),
                _ = update => {
                    tracing::info!("restarting into the update");
                    *restart.lock().unwrap() = updater.map(|u| u.exe().to_owned());
                }
            }
            stopping.notify_one();
        }
    };
    // Open event streams (the app's websocket) never finish on their own, so
    // graceful shutdown only gets a few seconds.
    let serve = axum::serve(listener, app).with_graceful_shutdown(stop);
    tokio::select! {
        r = serve => r?,
        _ = async {
            stopping.notified().await;
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        } => tracing::info!("closing remaining connections"),
    }
    backend.shutdown().await;
    let restart = restart.lock().unwrap().take();
    Ok(restart)
}

/// Runs `exe` with our arguments in place of this process.
fn restart_into(exe: &Path) -> anyhow::Result<()> {
    let mut cmd = std::process::Command::new(exe);
    cmd.args(std::env::args_os().skip(1));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        // Only returns on failure.
        Err(cmd.exec()).with_context(|| format!("restarting into {}", exe.display()))
    }
    #[cfg(not(unix))]
    {
        cmd.spawn().with_context(|| format!("restarting into {}", exe.display()))?;
        Ok(())
    }
}

fn load_or_create_token(path: &Path) -> anyhow::Result<(String, Option<PathBuf>)> {
    if let Ok(t) = std::fs::read_to_string(path)
        && !t.trim().is_empty()
    {
        return Ok((t.trim().to_string(), Some(path.to_owned())));
    }
    let token = storm_server::generate_token();
    std::fs::write(path, &token).with_context(|| format!("writing {}", path.display()))?;
    Ok((token, Some(path.to_owned())))
}
