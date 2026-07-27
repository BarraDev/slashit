//! `slashitd` — SlashIt without a window.
//!
//! Runs the queue executor and the IPC server so agents keep working, and the
//! `slashit` CLI keeps working, with no desktop session. Suitable for a
//! systemd user unit; see `docs/architecture/daemon.md`.
//!
//! This binary is deliberately thin. Everything it does lives in the library
//! next to the GUI's code, so the two cannot drift.

use clap::Parser;
use slashit_ui_lib::daemon::{self, DaemonOptions};

#[derive(Parser)]
#[command(
    name = "slashitd",
    about = "Run SlashIt headlessly: queue executor plus IPC server, no window",
    version
)]
struct Cli {
    /// Log every event, including per-chunk agent output.
    #[arg(long, short)]
    verbose: bool,

    /// Override a feature flag for this run, as `name=true` or `name=false`.
    ///
    /// Takes precedence over the environment and over features.toml, and is
    /// never written back. Repeat for several flags.
    #[arg(long = "feature", value_name = "NAME=BOOL")]
    features: Vec<String>,
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let cli = Cli::parse();

    // The overrides are passed through unparsed: `config::features` owns the
    // accepted spellings and the "unknown flag" message, and validating them
    // here as well would be a second place for the two to disagree.
    let options = DaemonOptions {
        verbose: cli.verbose,
        feature_overrides: cli.features,
    };

    match daemon::run(options).await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("slashitd: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}
