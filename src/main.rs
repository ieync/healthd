//! `healthd` — the daemon entry point.
//!
//! Wiring only: parse arguments, load config, open the database, start one
//! supervised thread per data source plus a maintenance thread, then serve the
//! IPC socket on the main thread. Everything is plain `std::thread` + sleep;
//! there is no async runtime, so an idle daemon is a handful of sleeping
//! threads and one blocked `accept`.
//!
//! There is deliberately no signal handler: SIGTERM from dinit just ends the
//! process. SQLite's WAL makes that crash-safe, and `ipc::bind` cleans up the
//! stale socket on the next start.

mod alert;
mod config;
mod db;
mod ipc;
mod poll;

use std::error::Error;
use std::path::PathBuf;
use std::process::ExitCode;
use std::thread;
use std::time::Duration;

use clap::Parser;

use crate::config::Config;
use crate::poll::Hub;

/// How often retention and downsampling run.
///
/// WHY 6 h: the work is a few thousand rows, so frequent runs are pointless,
/// but keeping the gap short bounds how much un-downsampled data can pile up.
const MAINTENANCE_EVERY: Duration = Duration::from_secs(6 * 3_600);

/// Command-line options.
#[derive(Parser)]
#[command(
    version,
    about = "Lightweight system health daemon (NVMe, battery, CPU, RAM)"
)]
struct Args {
    /// Config file (default: $XDG_CONFIG_HOME/healthd/config.toml)
    #[arg(long, value_name = "FILE")]
    config: Option<PathBuf>,
    /// Database file (default: ~/.local/share/healthd/health.db)
    #[arg(long, value_name = "FILE")]
    db: Option<PathBuf>,
    /// Unix socket (default: $HEALTHD_SOCK or ~/.run/healthd.sock)
    #[arg(long, value_name = "FILE")]
    socket: Option<PathBuf>,
    /// Print the built-in default configuration and exit
    #[arg(long)]
    print_default_config: bool,
}

/// Process entry: turns an error into a message and a non-zero exit code so
/// dinit can see the failure.
fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("healthd: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Set everything up and block serving IPC.
fn run() -> Result<(), Box<dyn Error>> {
    let args = Args::parse();
    if args.print_default_config {
        print!("{}", config::DEFAULT_TOML);
        return Ok(());
    }

    let cfg = Config::load(args.config.as_deref())?;
    let db_path = match args.db {
        Some(p) => p,
        None => config::default_db_path()?,
    };
    let sock_path = args
        .socket
        .unwrap_or_else(healthd_proto::default_socket_path);

    let hub = Hub::new(db::Db::open(&db_path)?, &cfg);
    hub.maintain();

    poll::spawn("cpu", &hub, poll::cpu::run)?;
    poll::spawn("ram", &hub, poll::ram::run)?;
    poll::spawn("battery", &hub, poll::battery::run)?;
    poll::spawn("nvme", &hub, poll::nvme::run)?;
    poll::spawn("maintenance", &hub, maintenance_loop)?;

    ipc::serve(&hub, &sock_path)?;
    Ok(())
}

/// Periodically run retention and downsampling.
fn maintenance_loop(hub: &Hub) {
    loop {
        thread::sleep(MAINTENANCE_EVERY);
        hub.maintain();
    }
}
