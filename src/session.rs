//! Interactive session management
//!
//! Owns the main select! loop, event consumer task, and reconnection logic.
// qual:allow(srp, file_length=370) reason: "one session lifecycle, well under the project's 2000-line cap"

use crate::channel_info::ChannelProvider;
use crate::client_command::{ClientCommand, ParseError};
use crate::commands::CommandProcessor;
use crate::completion::CompletionRequest;
use crate::config::AppConfig;
use crate::connection::{
    connect_retrying, enable_logging, is_connection_error, is_permission_denied,
    subscribe_heartbeat, subscribe_to_events,
};
use crate::console_complete::get_console_complete;
use crate::log_display::{display_log_event, format_channel_event, is_log_event, LogDestination};
use crate::printer::{Output, Printer};
use crate::readline::{build_macros, parse_function_key, run_readline_loop, ReadlineChannels};
use anyhow::Result;
use crossterm::{
    cursor::MoveTo,
    terminal::{Clear, ClearType},
    ExecutableCommand,
};
use freeswitch_esl_tokio::{ConnectionStatus, DisconnectReason, EslClient, EslEventStream};
use std::collections::HashMap;
use std::io::{self, Write};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::Duration;
use tracing::{debug, error, info, trace, warn};

const LIVENESS_TIMEOUT: Duration = Duration::from_secs(30);

#[cfg(unix)]
type SavedTerminal = Option<libc::termios>;
#[cfg(not(unix))]
type SavedTerminal = ();

// qual:allow(complexity, unsafe) reason: "tcgetattr has no safe form"
#[cfg(unix)]
fn save_terminal_state() -> SavedTerminal {
    use std::mem::MaybeUninit;
    unsafe {
        let mut termios = MaybeUninit::uninit();
        if libc::tcgetattr(libc::STDIN_FILENO, termios.as_mut_ptr()) == 0 {
            Some(termios.assume_init())
        } else {
            None
        }
    }
}

#[cfg(unix)]
fn restore_terminal_state(saved: &SavedTerminal) {
    if let Some(termios) = saved {
        unsafe {
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, termios);
        }
    }
}

#[cfg(not(unix))]
fn save_terminal_state() -> SavedTerminal {}

#[cfg(not(unix))]
fn restore_terminal_state(_saved: &SavedTerminal) {}

/// Why the command loop exited
enum SessionEnd {
    Quit,
    Disconnected(DisconnectCause),
}

/// What told us the connection was gone. A liveness timeout arrives as
/// `HeartbeatExpired`, and is honoured like any other disconnect.
enum DisconnectCause {
    Status(DisconnectReason),
    Command(anyhow::Error),
    Unknown,
}

impl std::fmt::Display for DisconnectCause {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DisconnectCause::Status(reason) => write!(f, "{}", reason),
            DisconnectCause::Command(e) => write!(f, "{:#}", e),
            DisconnectCause::Unknown => write!(f, "reason unknown"),
        }
    }
}

/// Run interactive CLI mode with reconnection support
pub async fn run_interactive_mode(
    client: EslClient,
    events: EslEventStream,
    config: &AppConfig,
    log_destination: Option<LogDestination>,
) -> Result<()> {
    let mut output = Output::new(config.color);

    setup_subscriptions(&client, config).await;
    println!("FreeSWITCH CLI ready. Type 'help' for commands, '/quit' to exit.\n");

    let macros = build_macros(config);
    let (readline_chans, mut chans) = ReadlineChannels::new(macros.clone());

    let original_termios = save_terminal_state();

    let readline_handle = spawn_readline(readline_chans, config);

    output.set_printer(receive_printer(chans.printer).await);
    let processor = CommandProcessor::new(&output, config.originate_check);

    let channel_provider = ChannelProvider::new(config.max_auto_complete_uuid);

    let mut ctx = CommandLoopCtx {
        parts: SessionParts {
            processor: &processor,
            output: &output,
            macros: &macros,
            channel_provider: &channel_provider,
        },
        cmd_rx: &mut chans.commands,
        quit_rx: &mut chans.quit,
        completion_rx: &mut chans.completions,
    };

    let session_result = run_reconnect_loop(
        client,
        events,
        config,
        &mut ctx,
        &output,
        log_destination.as_ref(),
    )
    .await;

    shutdown_readline(readline_handle, session_result.is_err(), original_termios).await;

    session_result
}

fn spawn_readline(chans: ReadlineChannels, config: &AppConfig) -> JoinHandle<Result<()>> {
    let config = config.clone();
    tokio::task::spawn_blocking(move || run_readline_loop(chans, &config))
}

/// A session without the external printer still runs; its output just goes
/// straight to stdout and can collide with the prompt.
async fn receive_printer(rx: oneshot::Receiver<Printer>) -> Printer {
    match rx.await {
        Ok(p) => p,
        Err(e) => {
            error!("Failed to receive external printer: {}", e);
            Printer::none()
        }
    }
}

/// One iteration is one connection session; a disconnect either ends the run or
/// reconnects and re-runs the subscriptions the new connection needs.
async fn run_reconnect_loop(
    mut client: EslClient,
    mut events: EslEventStream,
    config: &AppConfig,
    ctx: &mut CommandLoopCtx<'_>,
    output: &Output,
    log_destination: Option<&LogDestination>,
) -> Result<()> {
    loop {
        let mut event_task = spawn_event_consumer(events, output, log_destination.cloned());

        let result = run_command_loop(&client, ctx, &mut event_task).await;

        event_task.abort();

        let dropped = client.dropped_event_count();
        if dropped > 0 {
            warn!("{} events dropped due to full queue", dropped);
        }

        match result {
            SessionEnd::Quit => {
                if let Err(e) = client
                    .disconnect()
                    .await
                {
                    warn!("Disconnect on exit failed: {:#}", e);
                }
                return Ok(());
            }
            SessionEnd::Disconnected(cause) => {
                if !config.reconnect {
                    return Err(anyhow::anyhow!("Connection to FreeSWITCH lost: {}", cause));
                }
                warn!("Connection lost ({}), reconnecting...", cause);
                let (new_client, new_events) = connect_retrying(config).await?;
                info!("Reconnected successfully");
                client = new_client;
                events = new_events;
                setup_subscriptions(&client, config).await;
            }
        }
    }
}

async fn shutdown_readline(
    handle: JoinHandle<Result<()>>,
    failed: bool,
    saved_terminal: SavedTerminal,
) {
    handle.abort();

    if failed {
        // The readline thread is blocked in rl.readline() and cannot be
        // interrupted, so rustyline never restores the terminal itself.
        restore_terminal_state(&saved_terminal);
        return;
    }

    // Clean exit: readline already broke its loop (user typed /quit or EOF),
    // so the handle resolves quickly.
    if let Err(e) = handle.await {
        if !e.is_cancelled() {
            warn!("Error waiting for readline thread: {}", e);
        }
    }
}

/// Idle-liveness is armed only when the HEARTBEAT subscription is permitted:
/// without those events the timer would trip on a healthy idle socket.
async fn setup_subscriptions(client: &EslClient, config: &AppConfig) {
    let subscription = if config.events {
        subscribe_to_events(client).await
    } else {
        subscribe_heartbeat(client).await
    };
    match subscription {
        Ok(()) => client.set_liveness_timeout(LIVENESS_TIMEOUT),
        Err(e) if is_permission_denied(&e) => {
            warn!(
                "event subscription denied ({:#}); idle-liveness disabled for this user",
                e
            );
        }
        Err(e) => warn!("Failed to subscribe to events: {:#}", e),
    }
    if !config.quiet {
        if let Err(e) = enable_logging(client, config.log_level).await {
            warn!("Failed to enable logging: {:#}", e);
        }
    }
}

/// Spawn a task that consumes events and displays log/channel messages
fn spawn_event_consumer(
    mut events: EslEventStream,
    output: &Output,
    log_destination: Option<LogDestination>,
) -> JoinHandle<()> {
    let output = output.clone();
    tokio::spawn(async move {
        while let Some(result) = events
            .recv()
            .await
        {
            match result {
                Ok(event) => {
                    if let Some(raw) = event.raw_body() {
                        info!(
                            "Event body contained invalid UTF-8 ({} bytes), shown with \u{FFFD} replacements",
                            raw.len()
                        );
                        trace!("Non-UTF-8 body bytes: {}", raw.escape_ascii());
                    }
                    if let Some(msg) = format_channel_event(&event, &output) {
                        output.print(msg);
                    } else if is_log_event(&event) {
                        display_log_event(&event, &output);
                        if let Some(destination) = &log_destination {
                            destination.write_event(&event);
                        }
                    }
                }
                Err(e) => {
                    warn!("Event stream error: {}", e);
                }
            }
        }
    })
}

/// Session-lifetime state; `client` and `event_task` stay out of it so a
/// reconnect can swap them without rebuilding this.
struct SessionParts<'a> {
    processor: &'a CommandProcessor,
    output: &'a Output,
    macros: &'a HashMap<String, String>,
    channel_provider: &'a ChannelProvider,
}

struct CommandLoopCtx<'a> {
    parts: SessionParts<'a>,
    cmd_rx: &'a mut mpsc::UnboundedReceiver<String>,
    quit_rx: &'a mut oneshot::Receiver<()>,
    completion_rx: &'a mut mpsc::UnboundedReceiver<CompletionRequest>,
}

/// Main command processing select! loop for one connection session.
async fn run_command_loop(
    client: &EslClient,
    ctx: &mut CommandLoopCtx<'_>,
    event_task: &mut JoinHandle<()>,
) -> SessionEnd {
    loop {
        tokio::select! {
            result = &mut *event_task => {
                match result {
                    Err(ref e) if e.is_panic() => error!("Event consumer task panicked: {}", e),
                    Err(ref e) => error!("Event consumer task exited unexpectedly: {}", e),
                    Ok(()) => {}
                }
                return SessionEnd::Disconnected(classify_event_task_exit(client.status()));
            }
            Some(command) = ctx.cmd_rx.recv() => {
                if let Some(end) = handle_command_line(&ctx.parts, client, command).await {
                    return end;
                }
            }
            Some(request) = ctx.completion_rx.recv() => {
                let completions =
                    get_console_complete(client, &request, ctx.parts.channel_provider).await;
                if let Err(e) = request.response_tx.send(completions) {
                    debug!("completion reply dropped for {:?}: {}", request.line, e);
                }
            }
            _ = &mut *ctx.quit_rx => {
                return SessionEnd::Quit;
            }
        }
    }
}

/// The event stream ended: the client's own status names the cause, unless it
/// still believes it is connected and there is nothing to report.
fn classify_event_task_exit(status: ConnectionStatus) -> DisconnectCause {
    match status {
        ConnectionStatus::Disconnected(reason) => DisconnectCause::Status(reason),
        _ => DisconnectCause::Unknown,
    }
}

/// Dispatch one line from the readline thread. Returns `Some(end)` if the
/// session should terminate, `None` to continue.
async fn handle_command_line(
    parts: &SessionParts<'_>,
    client: &EslClient,
    command: String,
) -> Option<SessionEnd> {
    match command.parse::<ClientCommand>() {
        Ok(ClientCommand::Help) => {
            parts
                .processor
                .show_help(parts.macros);
            None
        }
        Ok(ClientCommand::Clear) => {
            clear_terminal();
            None
        }
        // Both run on the readline thread, which owns the history and the quit
        // signal; they only reach here if that parse and this one disagree.
        Ok(ClientCommand::History) | Ok(ClientCommand::Quit) => None,
        Ok(ClientCommand::Log(level)) => match parts
            .processor
            .handle_log_command(client, level)
            .await
        {
            Ok(Some(message)) => {
                parts
                    .output
                    .print(message);
                None
            }
            Ok(None) => None,
            Err(e) => report_or_disconnect(parts.processor, e),
        },
        Err(ParseError::InvalidLogLevel(level)) => {
            parts
                .output
                .print(format!("Invalid log level: {}", level));
            None
        }
        Err(ParseError::NotClientCommand) => {
            let effective = parse_function_key(&command, parts.macros).unwrap_or(command);
            // A refusal is already printed; the session carries on either way.
            if let Err(e) = parts
                .processor
                .execute_command(client, &effective)
                .await
            {
                return report_or_disconnect(parts.processor, e);
            }
            None
        }
    }
}

fn clear_terminal() {
    let mut stdout = io::stdout();
    let result: io::Result<()> = (|| {
        stdout.execute(Clear(ClearType::All))?;
        stdout.execute(MoveTo(0, 0))?;
        stdout.flush()
    })();
    if let Err(e) = result {
        warn!("Failed to clear terminal: {}", e);
    }
}

/// End the session on a connection error, print anything else.
fn report_or_disconnect(processor: &CommandProcessor, e: anyhow::Error) -> Option<SessionEnd> {
    if is_connection_error(&e) {
        return Some(SessionEnd::Disconnected(DisconnectCause::Command(e)));
    }
    processor.handle_error(e);
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_disconnected_client_names_the_reason() {
        let cause = classify_event_task_exit(ConnectionStatus::Disconnected(
            DisconnectReason::ConnectionClosed,
        ));
        assert_eq!(cause.to_string(), "connection closed");
        assert!(matches!(cause, DisconnectCause::Status(_)));
    }

    /// The consumer task can only end with the stream, so a client that still
    /// reports Connected has nothing to tell the user beyond that.
    #[test]
    fn a_still_connected_client_falls_back_to_unknown() {
        let cause = classify_event_task_exit(ConnectionStatus::Connected);
        assert!(matches!(cause, DisconnectCause::Unknown));
        assert_eq!(cause.to_string(), "reason unknown");
    }

    #[test]
    fn a_command_error_keeps_its_context_chain() {
        let inner = anyhow::anyhow!("socket is gone");
        let cause = DisconnectCause::Command(inner.context("api status failed"));
        assert_eq!(cause.to_string(), "api status failed: socket is gone");
    }
}
