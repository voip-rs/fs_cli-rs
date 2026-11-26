//! Interactive session management
//!
//! Owns the main select! loop, event consumer task, and reconnection logic.
//!
//! A typed line goes to the switch as bgapi, not api. mod_event_socket runs an
//! api inline on the listener thread, so one slow command holds the socket and
//! every line typed after it, each timing out in turn. A bgapi is
//! acknowledged at once and answers with a BACKGROUND_JOB event, so a slow
//! command becomes a warning and its result still prints when it arrives.
//! Jobs run in parallel switch threads, so their effects can land in any order;
//! `command_order: serial` holds each typed line until every job before it has
//! answered, which restores the order api gave.
//!
//! BACKGROUND_JOB is subscribed with an `event` line of its own: the switch
//! refuses a whole line when one event in it is denied, and HEARTBEAT, which
//! arms the liveness timer, must not be lost with it. A user denied
//! BACKGROUND_JOB gets api on that connection: a command past -t times out,
//! and its reply, if it comes, prints labelled with the timeout's ticket.
// qual:allow(srp, file_length=370) reason: "one session lifecycle, well under the project's 2000-line cap"

use crate::channel_info::ChannelProvider;
use crate::client_command::{ClientCommand, ParseError};
use crate::commands::{Answer, CommandProcessor};
use crate::completion::CompletionRequest;
use crate::config::AppConfig;
use crate::connection::{
    connect_retrying, enable_logging, is_connection_error, is_permission_denied,
    subscribe_heartbeat, subscribe_to_events,
};
use crate::console_complete::get_console_complete;
use crate::jobs::{CommandOrder, Jobs, ResultLabel};
use crate::log_display::{display_log_event, format_channel_event, is_log_event, LogDestination};
use crate::printer::{Output, Printer};
use crate::readline::{build_macros, parse_function_key, run_readline_loop, ReadlineChannels};
use anyhow::Result;
use crossterm::{
    cursor::MoveTo,
    terminal::{Clear, ClearType},
    ExecutableCommand,
};
use freeswitch_esl_tokio::{
    CommandTicket, ConnectionStatus, DisconnectReason, EslClient, EslError, EslEvent,
    EslEventStream, EslEventType, EslResponse, EventFormat,
};
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{self, Write};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::{sleep_until, Duration, Instant};
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
            command_timeout: Duration::from_millis(config.command_timeout),
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
        let mode = setup_subscriptions(&client, config, output).await;
        let (to_loop, from_consumer) = mpsc::unbounded_channel();
        let mut event_task =
            spawn_event_consumer(events, output, log_destination.cloned(), to_loop);
        let mut conn = ConnectionJobs {
            mode,
            jobs: Jobs::new(ResultLabel::WhenLate),
            results: from_consumer,
            abandoned: HashSet::new(),
            order: config.command_order,
            queued: VecDeque::new(),
        };

        let result = run_command_loop(&client, ctx, &mut event_task, &mut conn).await;

        event_task.abort();
        while let Ok(message) = conn
            .results
            .try_recv()
        {
            conn.take(
                message,
                ctx.parts
                    .processor,
                output,
            );
        }
        conn.jobs
            .strand(output, stranded_reason(&result));
        conn.report_unsent(output);

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

/// How typed lines reach the switch on one connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommandMode {
    Jobs,
    Api,
}

/// Job mode needs the results; without BACKGROUND_JOB they would never arrive.
async fn choose_command_mode(client: &EslClient, output: &Output) -> CommandMode {
    match client
        .subscribe_events(EventFormat::Plain, &[EslEventType::BackgroundJob])
        .await
    {
        Ok(()) => CommandMode::Jobs,
        Err(e) => {
            output.print_labeled(
                "Warning",
                &format!(
                    "subscribing to BACKGROUND_JOB failed ({}); commands run as api, \
                     and one slower than -t times out",
                    e.display_chain()
                ),
            );
            CommandMode::Api
        }
    }
}

/// Idle-liveness is armed only when the HEARTBEAT subscription is permitted:
/// without those events the timer would trip on a healthy idle socket.
async fn setup_subscriptions(
    client: &EslClient,
    config: &AppConfig,
    output: &Output,
) -> CommandMode {
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
    choose_command_mode(client, output).await
}

/// What a late reply said: an api body, or a command reply's text.
fn late_reply_text(response: &EslResponse) -> &str {
    response
        .body()
        .or_else(|| response.reply_text())
        .unwrap_or("")
        .trim_end()
}

/// Spawn a task that consumes events and displays log/channel messages
fn spawn_event_consumer(
    mut events: EslEventStream,
    output: &Output,
    log_destination: Option<LogDestination>,
    to_loop: mpsc::UnboundedSender<ToLoop>,
) -> JoinHandle<()> {
    let output = output.clone();
    tokio::spawn(async move {
        while let Some(result) = events
            .recv()
            .await
        {
            match result {
                Ok(event) if event.event_type() == Some(EslEventType::BackgroundJob) => {
                    if to_loop
                        .send(ToLoop::JobResult(event))
                        .is_err()
                    {
                        debug!("command loop gone, BACKGROUND_JOB dropped");
                    }
                }
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
                Err(EslError::LateReply { response, ticket }) => {
                    if to_loop
                        .send(ToLoop::LateReply { response, ticket })
                        .is_err()
                    {
                        debug!(
                            "command loop gone, late reply to command {} dropped",
                            ticket
                        );
                    }
                }
                Err(e) => {
                    warn!("Event stream error: {}", e.display_chain());
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
    /// How long a job runs before the operator is told it is late.
    command_timeout: Duration,
}

/// What the event consumer hands the command loop.
enum ToLoop {
    JobResult(EslEvent),
    LateReply {
        response: Box<EslResponse>,
        ticket: CommandTicket,
    },
}

/// What one connection submitted and how its results reach the loop.
struct ConnectionJobs {
    mode: CommandMode,
    jobs: Jobs,
    results: mpsc::UnboundedReceiver<ToLoop>,
    /// Completion lookups that timed out: their late replies answer nobody.
    abandoned: HashSet<CommandTicket>,
    order: CommandOrder,
    /// Serial order: lines typed while a job is pending, oldest first.
    queued: VecDeque<String>,
}

impl ConnectionJobs {
    /// Print a job result or a late reply, unless the reply answers a
    /// completion lookup the operator has already moved past.
    fn take(&mut self, message: ToLoop, processor: &CommandProcessor, output: &Output) {
        match message {
            ToLoop::JobResult(event) => {
                self.jobs
                    .complete(&event, processor, output);
            }
            ToLoop::LateReply { ticket, .. }
                if self
                    .abandoned
                    .remove(&ticket) =>
            {
                debug!("completion lookup {} answered after its timeout", ticket);
            }
            ToLoop::LateReply { response, ticket } => output.print(format!(
                "[late reply to command {}] {}",
                ticket,
                late_reply_text(&response)
            )),
        }
    }

    /// Name every line that never reached the switch, and drop it.
    fn report_unsent(&mut self, output: &Output) {
        for line in self
            .queued
            .drain(..)
        {
            output.print_labeled("Warning", &format!("not sent: {}", line));
        }
    }
}

fn stranded_reason(end: &SessionEnd) -> &'static str {
    match end {
        SessionEnd::Quit => "still running at exit, result not shown",
        SessionEnd::Disconnected(_) => "connection lost, outcome unknown",
    }
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
    conn: &mut ConnectionJobs,
) -> SessionEnd {
    loop {
        let overdue = conn
            .jobs
            .next_overdue(
                ctx.parts
                    .command_timeout,
            );
        tokio::select! {
            Some(message) = conn.results.recv() => {
                conn.take(message, ctx.parts.processor, ctx.parts.output);
                if let Err(e) = submit_queued(&ctx.parts, client, conn).await {
                    if let Some(end) = report_or_disconnect(ctx.parts.processor, e) {
                        return end;
                    }
                }
            }
            _ = sleep_until(overdue.unwrap_or_else(Instant::now)), if overdue.is_some() => {
                conn.jobs.warn_overdue(Instant::now(), ctx.parts.command_timeout, ctx.parts.output);
            }
            result = &mut *event_task => {
                match result {
                    Err(ref e) if e.is_panic() => error!("Event consumer task panicked: {}", e),
                    Err(ref e) => error!("Event consumer task exited unexpectedly: {}", e),
                    Ok(()) => {}
                }
                return SessionEnd::Disconnected(classify_event_task_exit(client.status()));
            }
            Some(command) = ctx.cmd_rx.recv() => {
                if let Some(end) = handle_command_line(&ctx.parts, client, conn, command).await {
                    return end;
                }
            }
            Some(request) = ctx.completion_rx.recv() => {
                let completions =
                    get_console_complete(client, &request, ctx.parts.channel_provider, &mut conn.abandoned).await;
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
    conn: &mut ConnectionJobs,
    command: String,
) -> Option<SessionEnd> {
    match command.parse::<ClientCommand>() {
        Ok(ClientCommand::Jobs) => {
            let mut rows = conn
                .jobs
                .list(Instant::now());
            rows.extend(
                conn.queued
                    .iter()
                    .map(|line| format!("queued  {}", line)),
            );
            parts
                .output
                .print(if rows.is_empty() {
                    "no pending jobs".to_string()
                } else {
                    rows.join("\n")
                });
            None
        }
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
            if let Err(e) = send_line(parts, client, conn, &effective).await {
                return report_or_disconnect(parts.processor, e);
            }
            None
        }
    }
}

/// A line for the switch, as a job or, on an api connection, as api.
async fn send_line(
    parts: &SessionParts<'_>,
    client: &EslClient,
    conn: &mut ConnectionJobs,
    line: &str,
) -> Result<Answer> {
    if conn.mode == CommandMode::Api {
        return parts
            .processor
            .execute_command(client, line)
            .await;
    }
    if let Some(answer) = parts
        .processor
        .execute_special(client, line)
        .await
    {
        return answer;
    }
    let line = strip_bgapi(line);
    if conn.order == CommandOrder::Serial
        && (conn
            .jobs
            .pending_count()
            > 0
            || !conn
                .queued
                .is_empty())
    {
        conn.queued
            .push_back(line.to_string());
        return Ok(Answer::Accepted);
    }
    conn.jobs
        .submit(client, parts.processor, line)
        .await
}

/// Serial order: submit queued lines once nothing is pending. A refused
/// submission leaves nothing pending, so the next line follows at once.
async fn submit_queued(
    parts: &SessionParts<'_>,
    client: &EslClient,
    conn: &mut ConnectionJobs,
) -> Result<()> {
    while conn
        .jobs
        .pending_count()
        == 0
    {
        let Some(line) = conn
            .queued
            .pop_front()
        else {
            return Ok(());
        };
        conn.jobs
            .submit(client, parts.processor, &line)
            .await?;
    }
    Ok(())
}

/// `bgapi X` typed in job mode is X: the line already runs in the background.
fn strip_bgapi(line: &str) -> &str {
    match line.split_once(char::is_whitespace) {
        Some((head, rest)) if head.eq_ignore_ascii_case("bgapi") => rest.trim_start(),
        _ => line,
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
    fn bgapi_typed_in_job_mode_is_submitted_once() {
        assert_eq!(strip_bgapi("bgapi originate x &park"), "originate x &park");
        assert_eq!(strip_bgapi("BGAPI  status"), "status");
        assert_eq!(strip_bgapi("bgapifoo status"), "bgapifoo status");
        assert_eq!(strip_bgapi("status"), "status");
    }

    #[test]
    fn a_command_error_keeps_its_context_chain() {
        let inner = anyhow::anyhow!("socket is gone");
        let cause = DisconnectCause::Command(inner.context("api status failed"));
        assert_eq!(cause.to_string(), "api status failed: socket is gone");
    }
}
