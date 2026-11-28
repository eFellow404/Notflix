//! CLI entry point for Notflix — Local DVD-Rip TV Show Player.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use clap::Parser;
use tokio::net::TcpListener;

use notflix::ffmpeg_bin::{
    auto_ffmpeg_enabled, default_ffmpeg_dir, resolve_ffmpeg_in, resolve_ffprobe_in, FfmpegManager,
    DEFAULT_DOWNLOAD_TIMEOUT_SEC,
};
use notflix::server::{build_router, AppState};
use notflix::storage::{default_db_path, NotflixStore};
use notflix::updater::{
    auto_update_on_startup, check_for_updates_with_fallback, cleanup_old_binary_artifacts,
    default_repo_dir, relaunch_updated_binary, should_relaunch_after_startup_update, UpdateResult,
    DEFAULT_FETCH_TIMEOUT_SEC,
};

#[derive(Debug, Parser)]
#[command(
    name = "notflix",
    version,
    about = "Notflix — Local DVD-Rip TV Show Player"
)]
struct Cli {
    /// Bind host (default: 127.0.0.1)
    #[arg(long, default_value = "127.0.0.1")]
    host: String,

    /// Bind port (default: 8080)
    #[arg(long, default_value_t = 8080)]
    port: u16,

    /// Optional path to SQLite database file
    #[arg(long)]
    db: Option<PathBuf>,

    /// Optional override directory for static web assets (defaults to embedded assets)
    #[arg(long)]
    static_dir: Option<PathBuf>,

    /// Suppress HTTP access logs
    #[arg(long)]
    quiet: bool,

    /// Skip automatic update check on startup
    #[arg(long)]
    no_auto_update: bool,

    /// Seconds to wait when contacting remote update server
    #[arg(long, default_value_t = DEFAULT_FETCH_TIMEOUT_SEC)]
    update_timeout: f64,

    /// Check for updates, print status, and exit
    #[arg(long)]
    check_update: bool,

    /// Do not automatically open Notflix in your default web browser on startup
    #[arg(long)]
    no_open: bool,

    /// Skip automatic download of ffmpeg/ffprobe binaries on startup
    #[arg(long)]
    no_ffmpeg_download: bool,
}

fn auto_update_enabled(cli_disabled: bool) -> bool {
    if cli_disabled {
        return false;
    }
    if std::env::var("NOTFLIX_SKIP_AUTO_UPDATE").ok().as_deref() == Some("1") {
        return false;
    }
    notflix::util::env_flag_enabled("NOTFLIX_AUTO_UPDATE")
}

fn init_tracing(quiet: bool) {
    if quiet {
        return;
    }
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_target(false)
        .with_env_filter(filter)
        .try_init();
}

fn maybe_run_startup_update(
    no_auto_update: bool,
    update_timeout: f64,
    db_path: &Path,
) -> Option<UpdateResult> {
    if !auto_update_enabled(no_auto_update) {
        return None;
    }
    let res = auto_update_on_startup(None, Some(db_path), update_timeout);
    if res.updated {
        println!("[updater] {}", res.message);
        if let Some(backup) = &res.backup_path {
            println!("[updater] Database backup saved to: {backup}");
        }
        if res.remote.as_deref() != Some("github-releases") {
            println!("[updater] Source updated. Rebuild or restart Notflix to load updated code.");
        }
    } else if matches!(
        res.status.as_str(),
        "offline" | "dirty" | "diverged" | "error"
    ) {
        println!("[updater] {}", res.message);
    }
    Some(res)
}

fn maybe_relaunch_after_update(startup_update: Option<&UpdateResult>) -> Option<ExitCode> {
    let res = startup_update?;
    let already_relaunched = std::env::var("NOTFLIX_RELAUNCHED").ok().as_deref() == Some("1");
    let exe_path = std::env::current_exe().unwrap_or_default();
    if !should_relaunch_after_startup_update(res, &exe_path, already_relaunched) {
        return None;
    }
    println!("[updater] Launching updated Notflix binary...");
    match relaunch_updated_binary() {
        Ok(code) => Some(code),
        Err(err) => {
            eprintln!(
                "[updater] Could not auto-relaunch updated binary ({err}); continuing with current process."
            );
            None
        }
    }
}

fn maybe_open_browser(no_open: bool, host: &str, actual_addr: SocketAddr, url: &str) {
    if no_open || std::env::var("NOTFLIX_NO_OPEN").ok().as_deref() == Some("1") {
        return;
    }
    let browser_url = if host == "0.0.0.0" || host == "::" {
        format!("http://127.0.0.1:{}", actual_addr.port())
    } else {
        url.to_string()
    };
    notflix::open_browser(&browser_url);
}

fn init_ffmpeg_manager(no_ffmpeg_download: bool) -> Arc<FfmpegManager> {
    if !auto_ffmpeg_enabled(no_ffmpeg_download) {
        return Arc::new(FfmpegManager::new_disabled());
    }
    let manager = Arc::new(FfmpegManager::new());
    let dir = default_ffmpeg_dir();
    if resolve_ffmpeg_in(&dir).is_none() || resolve_ffprobe_in(&dir).is_none() {
        manager.mark_downloading("Downloading ffmpeg/ffprobe...");
        let bg_ffmpeg = Arc::clone(&manager);
        tokio::task::spawn_blocking(move || {
            let _ = bg_ffmpeg.ensure_binaries(None, None, DEFAULT_DOWNLOAD_TIMEOUT_SEC);
        });
    }
    manager
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    init_tracing(cli.quiet);
    cleanup_old_binary_artifacts();

    let repo_dir = default_repo_dir();
    if cli.check_update {
        let result = check_for_updates_with_fallback(&repo_dir, true, cli.update_timeout);
        println!("[{}] {}", result.status, result.message);
        return ExitCode::SUCCESS;
    }

    let resolved_db = cli.db.unwrap_or_else(default_db_path);
    let startup_update =
        maybe_run_startup_update(cli.no_auto_update, cli.update_timeout, &resolved_db);

    if let Some(code) = maybe_relaunch_after_update(startup_update.as_ref()) {
        return code;
    }

    let store = match NotflixStore::new(Some(resolved_db)) {
        Ok(s) => s,
        Err(err) => {
            eprintln!("Failed to initialize database: {err}");
            return ExitCode::FAILURE;
        }
    };

    let listener = match TcpListener::bind((cli.host.as_str(), cli.port)).await {
        Ok(l) => l,
        Err(err) => {
            eprintln!("Failed to bind to {}:{}: {err}", cli.host, cli.port);
            return ExitCode::FAILURE;
        }
    };

    let actual_addr: SocketAddr = listener
        .local_addr()
        .unwrap_or_else(|_| SocketAddr::from(([127, 0, 0, 1], cli.port)));
    let url = format!("http://{actual_addr}");

    println!("Notflix is running at {url}");
    println!("Database: {}", store.db_path().display());
    println!("Press Ctrl+C to stop.");

    maybe_open_browser(cli.no_open, &cli.host, actual_addr, &url);

    let ffmpeg_manager = init_ffmpeg_manager(cli.no_ffmpeg_download);
    let state = AppState::new(store, cli.static_dir, None, startup_update, cli.quiet)
        .with_ffmpeg_manager(ffmpeg_manager);
    let app = build_router(state);

    if let Err(err) = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
    {
        eprintln!("Server error: {err}");
        return ExitCode::FAILURE;
    }

    println!("\nShutting down Notflix...");
    ExitCode::SUCCESS
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}
