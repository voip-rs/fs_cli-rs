//! bgapi jobs one connection submitted, from submission to their result.
//!
//! BACKGROUND_JOB is fired on the switch's global bus, so every subscribed
//! client receives every other client's results; one is ours only when its
//! Job-UUID was handed back to us by our own bgapi. A result may never come:
//! it is delivered on the connection that asked, so a reconnect strands every
//! job in flight, and the caller reports those rather than forgetting them.
//!
//! `Jobs` has one owner, the loop that submits and completes; it is never
//! shared behind a lock.

use crate::commands::{Answer, CommandProcessor};
use crate::printer::Output;
use anyhow::{Context, Result};
use freeswitch_esl_tokio::{BgJobTracker, EslClient, EslEvent};
use tokio::time::{Duration, Instant};
use tracing::debug;

/// One submitted command, keyed by its Job-UUID in the tracker.
#[derive(Debug)]
pub struct Job {
    pub command: String,
    pub submitted: Instant,
    /// The operator was told it is taking longer than expected.
    pub warned: bool,
}

/// How a result is introduced when it prints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResultLabel {
    /// `[command] result`, always: results of a batch interleave.
    Command,
    /// The bare result, as an api reply prints, unless the job was reported
    /// late; then `[Job-UUID command] result`, to match the warning.
    WhenLate,
}

#[derive(Debug)]
pub struct Jobs {
    tracker: BgJobTracker<Job>,
    label: ResultLabel,
}

impl Jobs {
    pub fn new(label: ResultLabel) -> Self {
        Self {
            tracker: BgJobTracker::new(),
            label,
        }
    }

    /// Submit `line` as bgapi, after the originate check. A refused submission
    /// is reported and answers `Refused`.
    pub async fn submit(
        &mut self,
        client: &EslClient,
        processor: &CommandProcessor,
        line: &str,
    ) -> Result<Answer> {
        let sent = processor.checked_originate(line);
        let command = sent.as_str();
        let job = Job {
            command: command.to_string(),
            submitted: Instant::now(),
            warned: false,
        };
        match self
            .tracker
            .bgapi(client, command, job)
            .await
        {
            Ok(uuid) => {
                debug!("bgapi {} submitted as job {}", command, uuid);
                Ok(Answer::Accepted)
            }
            // Context goes on afterwards: the refusal is classified from the
            // bare EslError.
            Err(e) => processor
                .report_refusal(anyhow::Error::new(e), None)
                .map(|()| Answer::Refused)
                .with_context(|| format!("bgapi {}", command)),
        }
    }

    /// Print the result `event` carries if it answers one of our jobs.
    /// `None` when the event is not one of ours.
    pub fn complete(
        &mut self,
        event: &EslEvent,
        processor: &CommandProcessor,
        output: &Output,
    ) -> Option<Answer> {
        let (job, result) = self
            .tracker
            .try_complete(event)?;
        let label = match self.label {
            ResultLabel::Command => Some(
                job.command
                    .clone(),
            ),
            ResultLabel::WhenLate if job.warned => Some(format!(
                "{} {}",
                result
                    .job_uuid()
                    .unwrap_or("?"),
                job.command
            )),
            ResultLabel::WhenLate => None,
        };
        Some(match result.parse_body() {
            Ok(parsed) => {
                // Interactive output matches an api reply, which keeps its `+OK`.
                let body = match self.label {
                    ResultLabel::Command => parsed,
                    ResultLabel::WhenLate => result
                        .body()
                        .unwrap_or(parsed),
                };
                match &label {
                    Some(label) => output.print(format!("[{}] {}", label, body.trim())),
                    None if body
                        .trim()
                        .is_empty() => {}
                    None => output.print(body.to_string()),
                }
                Answer::Accepted
            }
            Err(e) => match processor.report_refusal(anyhow::Error::new(e), label.as_deref()) {
                Ok(()) => Answer::Refused,
                Err(e) => {
                    processor.handle_error(e.context(format!("bgapi {}", job.command)));
                    Answer::Accepted
                }
            },
        })
    }

    /// When the next job not yet reported late becomes late.
    pub fn next_overdue(&mut self, after: Duration) -> Option<Instant> {
        let mut next: Option<Instant> = None;
        self.tracker
            .retain(|_, job| {
                if !job.warned {
                    let due = job.submitted + after;
                    next = Some(next.map_or(due, |n| n.min(due)));
                }
                true
            });
        next
    }

    /// Tell the operator about every job still running `after` its submission,
    /// once each; the job stays pending.
    pub fn warn_overdue(&mut self, now: Instant, after: Duration, output: &Output) {
        self.tracker
            .retain(|uuid, job| {
                if !job.warned && now >= job.submitted + after {
                    job.warned = true;
                    output.print_labeled(
                        "Warning",
                        &format!(
                            "job {} ({}) is taking longer than expected ({} ms)",
                            uuid,
                            job.command,
                            after.as_millis()
                        ),
                    );
                }
                true
            });
    }

    /// Report every pending job with why its result will not be shown, and
    /// forget it: a result arrives only on the connection that asked.
    pub fn strand(&mut self, output: &Output, reason: &str) {
        self.tracker
            .retain(|uuid, job| {
                output.print_labeled(
                    "Warning",
                    &format!("job {} ({}): {}", uuid, job.command, reason),
                );
                false
            });
    }

    /// One line per pending job: Job-UUID, elapsed time, command.
    pub fn list(&mut self, now: Instant) -> Vec<String> {
        let mut rows = Vec::new();
        self.tracker
            .retain(|uuid, job| {
                rows.push((
                    job.submitted,
                    format!(
                        "{}  {:>6} ms  running  {}",
                        uuid,
                        now.duration_since(job.submitted)
                            .as_millis(),
                        job.command
                    ),
                ));
                true
            });
        rows.sort();
        rows.into_iter()
            .map(|(_, row)| row)
            .collect()
    }

    pub fn pending_count(&self) -> usize {
        self.tracker
            .pending_count()
    }

    /// `command (Job-UUID)` for every pending job, sorted.
    pub fn outstanding(&mut self) -> Vec<String> {
        let mut pending = Vec::new();
        self.tracker
            .retain(|uuid, job| {
                pending.push(format!("{} ({})", job.command, uuid));
                true
            });
        pending.sort();
        pending
    }
}
