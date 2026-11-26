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
use tracing::debug;

/// One submitted command, keyed by its Job-UUID in the tracker.
#[derive(Debug)]
pub struct Job {
    pub command: String,
}

#[derive(Debug, Default)]
pub struct Jobs {
    tracker: BgJobTracker<Job>,
}

impl Jobs {
    pub fn new() -> Self {
        Self::default()
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
        Some(match result.parse_body() {
            Ok(body) => {
                output.print(format!("[{}] {}", job.command, body.trim()));
                Answer::Accepted
            }
            Err(e) => match processor.report_refusal(anyhow::Error::new(e), Some(&job.command)) {
                Ok(()) => Answer::Refused,
                Err(e) => {
                    processor.handle_error(e.context(format!("bgapi {}", job.command)));
                    Answer::Accepted
                }
            },
        })
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
