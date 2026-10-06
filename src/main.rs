//! fs_cli-rs: Interactive FreeSWITCH CLI client using ESL

use anyhow::{Context, Result};
use freeswitch_esl_tokio::EslClient;
use std::io::IsTerminal;
use std::process::ExitCode;
use tracing::{debug, info};

mod args;
mod batch;
mod channel_info;
mod client_command;
mod commands;
mod completion;
mod config;
mod connection;
mod console_complete;
mod esl_debug;
mod legacy_config;
mod log_display;
mod log_level;
mod originate_check;
mod printer;
mod readline;
mod session;

use args::Args;
use batch::BatchError;
use config::AppConfig;
use connection::{connect_to_freeswitch_with_retry, is_unreachable, print_connect_error};
use esl_debug::EslDebugLevel;
use log_display::LogDestination;

/// Stock fs_cli's codes: nothing was sent, or a command was sent and its
/// outcome is unknown.
const EXIT_NOT_CONNECTED: u8 = 255;
const EXIT_OUTCOME_UNKNOWN: u8 = 254;

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    match run().await {
        Ok(code) => code,
        Err(e) => {
            eprintln!("{:#}", e);
            ExitCode::FAILURE
        }
    }
}

// qual:allow(iosp) reason: "entry point wiring the program together; splitting it would invent indirection"
async fn run() -> Result<ExitCode> {
    let config = Args::parse_and_merge()?;

    setup_logging(config.debug);

    if !usable_mode(&config) {
        eprintln!(
            "fs_cli: interactive mode needs a terminal on both stdin and stdout.\n\
             Give commands with -x/-X, or a log destination with --log-file PATH."
        );
        return Ok(ExitCode::FAILURE);
    }

    // Opened before connecting so an unwritable path fails without a session.
    let log_destination = match &config.log_file {
        Some(spec) => Some(LogDestination::open(spec, config.color)?),
        None => None,
    };

    debug!("About to connect to FreeSWITCH");
    let (client, events) = match connect_to_freeswitch_with_retry(&config).await {
        Ok(pair) => {
            debug!("Successfully connected to FreeSWITCH");
            pair
        }
        Err(e) => {
            print_connect_error(&e, &config);
            return Ok(ExitCode::from(EXIT_NOT_CONNECTED));
        }
    };

    if !config
        .execute
        .is_empty()
    {
        if let Err(e) = batch::run_batch(&client, events, &config, log_destination).await {
            eprintln!("{:#}", e.source);
            return Ok(batch_exit_code(&e));
        }
        disconnect(&client).await;
    } else if terminal_available() {
        if let Err(e) =
            session::run_interactive_mode(client, events, &config, log_destination).await
        {
            eprintln!("{:#}", e);
            // Returning would wait on the readline thread, blocked on stdin.
            std::process::exit(1);
        }
    } else {
        let destination = log_destination.context("interactive mode needs a terminal")?;
        batch::run_streaming(&client, events, &config, destination).await?;
        disconnect(&client).await;
    }

    Ok(ExitCode::SUCCESS)
}

fn batch_exit_code(error: &BatchError) -> ExitCode {
    if error.dispatched {
        ExitCode::from(EXIT_OUTCOME_UNKNOWN)
    } else if is_unreachable(&error.source) {
        ExitCode::from(EXIT_NOT_CONNECTED)
    } else {
        ExitCode::FAILURE
    }
}

/// Every command was answered by now, so a failed goodbye is reported but
/// does not fail the run.
async fn disconnect(client: &EslClient) {
    info!("Disconnecting from FreeSWITCH...");
    if let Err(e) = client
        .disconnect()
        .await
    {
        eprintln!("fs_cli: disconnecting from FreeSWITCH: {:#}", e);
    }
}

/// rustyline needs a terminal on both streams before it will build its external
/// printer, so that pair is what decides whether interactive mode is possible.
fn terminal_available() -> bool {
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

/// Interactive mode is the only mode that needs a terminal.
fn usable_mode(config: &AppConfig) -> bool {
    !config
        .execute
        .is_empty()
        || config
            .log_file
            .is_some()
        || terminal_available()
}

fn setup_logging(debug_level: EslDebugLevel) {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(debug_level.tracing_filter())
        .with_target(false)
        .with_thread_ids(false)
        .with_file(false)
        .with_line_number(false)
        .init();
}
