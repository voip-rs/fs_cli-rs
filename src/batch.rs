//! Non-interactive modes: the ordered `-x`/`-X` batch and the log stream.

use crate::commands::{Answer, CommandProcessor};
use crate::config::{AppConfig, BatchCommand};
use crate::connection::enable_logging;
use crate::log_display::{is_log_event, LogDestination};
use crate::printer::Output;
use anyhow::{anyhow, Context, Result};
use freeswitch_esl_tokio::{
    BgJobTracker, EslClient, EslEvent, EslEventStream, EslEventType, EventFormat,
};
use tokio::time::Duration;
use tracing::debug;

/// Why a drain stopped.
enum Drained {
    /// Nothing more is ready, or everything asked for has arrived.
    Idle,
    /// The log destination is gone (a closed `| head`); end the run.
    SinkClosed,
    /// The event stream ended before the pending jobs completed.
    StreamEnded,
}

/// A failed batch run, and whether a command may have reached the switch.
pub struct BatchError {
    pub dispatched: bool,
    pub source: anyhow::Error,
}

/// Run every `-x` / `-X` in the order given, then wait out the jobs.
/// `Refused` when any command or job was refused.
pub async fn run_batch(
    client: &EslClient,
    events: EslEventStream,
    config: &AppConfig,
    log: Option<LogDestination>,
) -> Result<Answer, BatchError> {
    let mut batch = Batch::new(client, events, config, log);
    let result = batch
        .run(config)
        .await;
    match result {
        Ok(()) if batch.refused => Ok(Answer::Refused),
        Ok(()) => Ok(Answer::Accepted),
        Err(source) => Err(BatchError {
            dispatched: batch.dispatched,
            source,
        }),
    }
}

/// Write log lines until a signal arrives.
pub async fn run_streaming(
    client: &EslClient,
    events: EslEventStream,
    config: &AppConfig,
    log: LogDestination,
) -> Result<()> {
    let mut batch = Batch::new(client, events, config, Some(log));
    batch
        .start_logging(config)
        .await?;

    loop {
        tokio::select! {
            received = batch.events.recv() => {
                match received {
                    Some(Ok(event)) => batch.handle_event(&event),
                    Some(Err(e)) => return Err(anyhow::Error::new(e).context("event stream")),
                    None => return Err(anyhow!("FreeSWITCH closed the event stream")),
                }
                if batch.sink_closed() {
                    return Ok(());
                }
            }
            result = shutdown_signal() => {
                result?;
                debug!("shutdown signal received, ending the log stream");
                return Ok(());
            }
        }
    }
}

#[cfg(unix)]
async fn shutdown_signal() -> Result<()> {
    use tokio::signal::unix::{signal, SignalKind};

    let mut term = signal(SignalKind::terminate()).context("installing the SIGTERM handler")?;
    tokio::select! {
        result = tokio::signal::ctrl_c() => result.context("waiting for SIGINT")?,
        _ = term.recv() => {}
    }
    Ok(())
}

#[cfg(not(unix))]
async fn shutdown_signal() -> Result<()> {
    tokio::signal::ctrl_c()
        .await
        .context("waiting for SIGINT")
}

/// One connection issuing commands and draining its own event stream.
struct Batch<'a> {
    client: &'a EslClient,
    events: EslEventStream,
    processor: CommandProcessor,
    output: Output,
    log: Option<LogDestination>,
    jobs: BgJobTracker<String>,
    /// Set before the first user command is handed to the library.
    dispatched: bool,
    refused: bool,
}

impl<'a> Batch<'a> {
    fn new(
        client: &'a EslClient,
        events: EslEventStream,
        config: &AppConfig,
        log: Option<LogDestination>,
    ) -> Self {
        let output = Output::new(config.color);
        Self {
            client,
            events,
            processor: CommandProcessor::new(&output, config.originate_check),
            output,
            log,
            jobs: BgJobTracker::new(),
            dispatched: false,
            refused: false,
        }
    }

    async fn run(&mut self, config: &AppConfig) -> Result<()> {
        if config
            .execute
            .iter()
            .any(|c| matches!(c, BatchCommand::BgApi(_)))
        {
            // BACKGROUND_JOB is a global-bus event: without this subscription no
            // result arrives at all, and with it every client's results do.
            self.client
                .subscribe_events(EventFormat::Plain, &[EslEventType::BackgroundJob])
                .await
                .context("subscribing to BACKGROUND_JOB")?;
        }
        self.start_logging(config)
            .await?;

        for command in &config.execute {
            let answer = match command {
                BatchCommand::Api(cmd) => {
                    self.run_api(cmd)
                        .await?
                }
                BatchCommand::BgApi(cmd) => {
                    self.submit_job(cmd)
                        .await?
                }
            };
            if answer == Answer::Refused {
                self.refused = true;
                if config.fail_on_error {
                    break;
                }
            }
            if let Drained::SinkClosed = self
                .drain_ready()
                .await?
            {
                return Ok(());
            }
        }

        self.wait_for_jobs(config.job_timeout)
            .await
    }

    /// The log stream is what `--log-file` asks for, so a switch that refuses
    /// to send it ends the run rather than producing an empty file.
    async fn start_logging(&mut self, config: &AppConfig) -> Result<()> {
        if self
            .log
            .is_none()
        {
            return Ok(());
        }
        enable_logging(self.client, config.log_level)
            .await
            .context("enabling the log stream")
    }

    async fn run_api(&mut self, command: &str) -> Result<Answer> {
        self.dispatched = true;
        self.processor
            .execute_command(self.client, command)
            .await
    }

    async fn submit_job(&mut self, command: &str) -> Result<Answer> {
        let sent = self
            .processor
            .checked_originate(command);
        let command = sent.as_str();
        self.dispatched = true;
        match self
            .jobs
            .bgapi(self.client, command, command.to_string())
            .await
        {
            Ok(uuid) => {
                debug!("bgapi {} submitted as job {}", command, uuid);
                Ok(Answer::Accepted)
            }
            // Context goes on afterwards: the refusal is classified from the
            // bare EslError.
            Err(e) => self
                .processor
                .report_refusal(anyhow::Error::new(e), None)
                .map(|()| Answer::Refused)
                .with_context(|| format!("bgapi {}", command)),
        }
    }

    fn handle_event(&mut self, event: &EslEvent) {
        if let Some((command, result)) = self
            .jobs
            .try_complete(event)
        {
            match result.parse_body() {
                Ok(body) => self
                    .output
                    .print(format!("[{}] {}", command, body.trim())),
                Err(e) => match self
                    .processor
                    .report_refusal(anyhow::Error::new(e), Some(&command))
                {
                    Ok(()) => self.refused = true,
                    Err(e) => self
                        .processor
                        .handle_error(e.context(format!("bgapi {}", command))),
                },
            }
            return;
        }
        if is_log_event(event) {
            if let Some(destination) = &self.log {
                destination.write_event(event);
            }
        }
    }

    fn sink_closed(&self) -> bool {
        self.log
            .as_ref()
            .is_some_and(LogDestination::is_broken)
    }

    /// Consume whatever the stream already holds, without waiting for more.
    async fn drain_ready(&mut self) -> Result<Drained> {
        loop {
            let Ok(received) = tokio::time::timeout(
                Duration::ZERO,
                self.events
                    .recv(),
            )
            .await
            else {
                return Ok(Drained::Idle);
            };
            match received {
                Some(Ok(event)) => self.handle_event(&event),
                Some(Err(e)) => return Err(anyhow::Error::new(e).context("event stream")),
                None => return Ok(Drained::StreamEnded),
            }
            if self.sink_closed() {
                return Ok(Drained::SinkClosed);
            }
        }
    }

    /// Block until every outstanding job has reported, or the deadline passes.
    async fn wait_for_jobs(&mut self, timeout_ms: Option<u64>) -> Result<()> {
        if self
            .jobs
            .pending_count()
            == 0
        {
            return Ok(());
        }
        let outcome = match timeout_ms {
            Some(ms) => {
                match tokio::time::timeout(Duration::from_millis(ms), self.drain_until_done()).await
                {
                    Ok(result) => result?,
                    Err(_) => {
                        return Err(anyhow!(
                            "no result after {} ms for: {}",
                            ms,
                            self.outstanding()
                                .join(", ")
                        ))
                    }
                }
            }
            None => {
                self.drain_until_done()
                    .await?
            }
        };
        match outcome {
            Drained::StreamEnded => Err(anyhow!(
                "FreeSWITCH closed the event stream with no result for: {}",
                self.outstanding()
                    .join(", ")
            )),
            Drained::Idle | Drained::SinkClosed => Ok(()),
        }
    }

    async fn drain_until_done(&mut self) -> Result<Drained> {
        while self
            .jobs
            .pending_count()
            > 0
        {
            match self
                .events
                .recv()
                .await
            {
                Some(Ok(event)) => self.handle_event(&event),
                Some(Err(e)) => return Err(anyhow::Error::new(e).context("event stream")),
                None => return Ok(Drained::StreamEnded),
            }
            if self.sink_closed() {
                return Ok(Drained::SinkClosed);
            }
        }
        Ok(Drained::Idle)
    }

    /// `retain` is the only accessor carrying both the Job-UUID and the command
    /// that asked for it; every job is kept.
    fn outstanding(&mut self) -> Vec<String> {
        let mut pending = Vec::new();
        self.jobs
            .retain(|uuid, command| {
                pending.push(format!("{} ({})", command, uuid));
                true
            });
        pending.sort();
        pending
    }
}
