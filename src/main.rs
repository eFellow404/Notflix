//! CLI entry point for Notflix — Local DVD-Rip TV Show Player.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::Parser;
use tokio::net::TcpListener;

use notflix::server::{build_router, AppState};
use notflix::storage::{default_db_path, NotflixStore};
use notflix::updater::{
    auto_update_on_startup, check_for_updates, default_repo_dir, UpdateResult,
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

    /// Skip automatic remote Git update check on startup
    #[arg(long)]
    no_auto_update: bool,

    /// Seconds to wait when contacting remote Git server
    #[arg(long, default_value_t = DEFAULT_FETCH_TIMEOUT_SEC)]
    update_timeout: f64,

    /// Check remote Git repository for updates, print status, and exit
    #[arg(long)]
    check_update: bool,
}

fn auto_update_enabled(cli_disabled: bool) -> bool {
    if cli_disabled {
        return false;
    }
    if std::env::var("NOTFLIX_SKIP_AUTO_UPDATE").ok().as_deref() == Some("1") {
        return false;
    }
    let env_flag = std::env::var("NOTFLIX_AUTO_UPDATE")
        .unwrap_or_else(|_| "1".to_string())
        .trim()
        .to_ascii_lowercase();
    !matches!(env_flag.as_str(), "0" | "false" | "no" | "off")
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
    repo_dir: &Path,
    db_path: &Path,
) -> Option<UpdateResult> {
    if !auto_update_enabled(no_auto_update) {
        return None;
    }
    let res = auto_update_on_startup(Some(repo_dir), Some(db_path), update_timeout);
    if res.updated {
        println!("[updater] {}", res.message);
        if let Some(backup) = &res.backup_path {
            println!("[updater] Database backup saved to: {backup}");
        }
        println!("[updater] Source updated. Rebuild or restart Notflix to load updated code.");
    } else if matches!(
        res.status.as_str(),
        "offline" | "dirty" | "diverged" | "error"
    ) {
        println!("[updater] {}", res.message);
    }
    Some(res)
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    init_tracing(cli.quiet);

    let repo_dir = default_repo_dir();
    if cli.check_update {
        let result = check_for_updates(Some(&repo_dir), cli.update_timeout);
        println!("[{}] {}", result.status, result.message);
        return ExitCode::SUCCESS;
    }

    let resolved_db = cli.db.unwrap_or_else(default_db_path);
    let startup_update = maybe_run_startup_update(
        cli.no_auto_update,
        cli.update_timeout,
        &repo_dir,
        &resolved_db,
    );

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

    println!("Notflix is running at http://{actual_addr}");
    println!("Database: {}", store.db_path().display());
    println!("Press Ctrl+C to stop.");

    let state = AppState::new(
        store,
        cli.static_dir,
        Some(repo_dir),
        startup_update,
        cli.quiet,
    );
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
