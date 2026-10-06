//! Command processing and execution for fs_cli-rs

use crate::client_command::{ClientCommand, ParseError};
use crate::log_level::{set_log_level, LogSetting};
use crate::originate_check::{check, OriginateCheck};
use crate::printer::Output;
use anyhow::{Error, Result};
use colored::Colorize;
use freeswitch_esl_tokio::{CommandFailure, EslClient, EslError, EslResponse};
use std::collections::HashMap;
use tracing::trace;

/// The body to display, or the refusal to frame.
///
/// Only a refused command becomes an error here. `api_result` peels the `+OK `
/// that a bare `+OK` reply consists of entirely and calls the empty remainder
/// a `ProtocolError`, whose `is_connection_error` is true — routing that
/// through the error path would read a successful command as a disconnect.
fn api_outcome(response: &EslResponse) -> Result<String, EslError> {
    match response.api_result() {
        Err(e)
            if e.command_failure()
                .is_some() =>
        {
            Err(e)
        }
        _ => Ok(response
            .body()
            .unwrap_or_default()
            .to_string()),
    }
}

/// Label and text for a refused command, or None when the failure carries no
/// text to frame. An unprefixed reply keeps all of it: the switch answers
/// several `uuid_*` APIs with a bare `-ERROR`.
fn frame_failure<'a>(failure: &CommandFailure<'a>) -> Option<(&'static str, &'a str)> {
    match failure {
        CommandFailure::Usage(text) => Some(("Usage", text)),
        CommandFailure::Err(text) | CommandFailure::Unprefixed(text) => Some(("API Error", text)),
        _ => None,
    }
}

/// How the switch answered a command that reached it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    Accepted,
    /// Refused with `-ERR` or `-USAGE`, already printed.
    Refused,
}

/// Command processor for FreeSWITCH CLI commands
pub struct CommandProcessor {
    output: Output,
    originate_check: OriginateCheck,
}

impl CommandProcessor {
    /// Create new command processor
    pub fn new(output: &Output, originate_check: OriginateCheck) -> Self {
        Self {
            output: output.clone(),
            originate_check,
        }
    }

    fn print_message(&self, message: &str) {
        self.output
            .print(message.to_string());
    }

    /// Handle command execution errors with proper formatting
    pub fn handle_error(&self, error: Error) {
        self.output
            .print_labeled_error("Error", &error);
    }

    /// Call the FreeSWITCH API and return the response body verbatim.
    ///
    /// Transport errors and refused commands propagate as `EslError`; callers
    /// frame the latter through `EslError::command_failure`.
    // qual:allow(srp, slm) reason: "thin wrapper pairing api call with outcome framing; another layer would not earn its place"
    async fn api_body(&self, client: &EslClient, command: &str) -> Result<String> {
        Ok(api_outcome(
            &client
                .api(command)
                .await?,
        )?)
    }

    /// Execute a FreeSWITCH command
    pub async fn execute_command(&self, client: &EslClient, command: &str) -> Result<Answer> {
        trace!("execute_command called with: '{}'", command);
        let sent = self.checked_originate(command);
        let command = sent.as_str();

        match self
            .handle_special_command(client, command)
            .await
        {
            Ok(Some(result)) => {
                self.print_message(&result);
                return Ok(Answer::Accepted);
            }
            Ok(None) => {}
            Err(e) => {
                return self
                    .report_refusal(e, None)
                    .map(|()| Answer::Refused)
            }
        }

        match self
            .api_body(client, command)
            .await
        {
            Ok(body) => {
                if !body
                    .trim()
                    .is_empty()
                {
                    self.print_message(&body);
                }
                Ok(Answer::Accepted)
            }
            Err(e) => self
                .report_refusal(e, None)
                .map(|()| Answer::Refused),
        }
    }

    /// The line to send for `command`, having said what the switch will install for an
    /// `originate` and rewritten it when the policy asks and the rewrite verifies.
    pub fn checked_originate(&self, command: &str) -> String {
        let checked = check(self.originate_check, command);
        if let Some(report) = checked.report {
            self.output
                .print_labeled("Originate", &report);
        }
        checked
            .rewritten
            .unwrap_or_else(|| command.to_string())
    }

    /// Print a refused command, prefixed `[context]` when given, and return Ok;
    /// any other error is handed back untouched.
    // qual:allow(coupling, deh) reason: "this match turns a refused command into user-facing text; handling the error here is the point"
    pub fn report_refusal(&self, error: Error, context: Option<&str>) -> Result<()> {
        match error
            .downcast_ref::<EslError>()
            .and_then(EslError::command_failure)
            .and_then(|f| frame_failure(&f))
        {
            Some((label, text)) => {
                let message = match context {
                    Some(context) => format!("[{}] {}", context, text),
                    None => text.to_string(),
                };
                self.output
                    .print_labeled(label, &message);
                Ok(())
            }
            None => Err(error),
        }
    }

    /// Handle special CLI commands that need custom processing
    async fn handle_special_command(
        &self,
        client: &EslClient,
        command: &str,
    ) -> Result<Option<String>> {
        let parts: Vec<&str> = command
            .split_whitespace()
            .collect();
        if parts.is_empty() {
            trace!("handle_special_command: empty command");
            return Ok(None);
        }

        trace!("handle_special_command: parts[0] = '{}'", parts[0]);

        match command.parse::<ClientCommand>() {
            Ok(ClientCommand::Log(level)) => {
                return self
                    .handle_log_command(client, level)
                    .await
            }
            Err(ParseError::InvalidLogLevel(level)) => {
                return Ok(Some(format!("Invalid log level: {}", level)))
            }
            _ => {}
        }

        if parts[0].eq_ignore_ascii_case("uptime") {
            let body = self
                .api_body(client, "status")
                .await?;
            return Ok(Some(self.extract_uptime(&body)));
        }

        Ok(None)
    }

    /// Handle /log command, with no level meaning "list the levels"
    // qual:allow(srp, slm) reason: "small helper turning a set_log_level reply into the two lines /log can show; another layer would not earn its place"
    pub async fn handle_log_command(
        &self,
        client: &EslClient,
        level: Option<LogSetting>,
    ) -> Result<Option<String>> {
        let Some(setting) = level else {
            return Ok(Some(LogSetting::help_text()));
        };

        match set_log_level(client, setting).await? {
            None => Ok(Some(match setting {
                LogSetting::Level(level) => {
                    format!("+OK log level {} [{}]", level, level.as_number())
                }
                LogSetting::NoLog => "+OK log level nolog".to_string(),
            })),
            Some(reply) => Ok(Some(format!("Failed to set log level: {}", reply))),
        }
    }

    /// Extract uptime information from status output
    // qual:allow(srp, slm) reason: "small helper scanning status output for the uptime line; another layer would not earn its place"
    fn extract_uptime(&self, status_output: &str) -> String {
        for line in status_output.lines() {
            if line.contains("UP")
                && (line.contains("years") || line.contains("days") || line.contains("hours"))
            {
                return line
                    .trim()
                    .to_string();
            }
        }
        "Uptime information not found".to_string()
    }

    /// Show help information with the effective (merged) function key bindings.
    pub fn show_help(&self, macros: &HashMap<String, String>) {
        let mut fnkey_lines = String::new();
        for (i, cmd) in crate::readline::fn_key_bindings(macros) {
            fnkey_lines.push_str(&format!("  F{:<3} = {}\n", i, cmd));
        }

        let help_text = format!(
            r#"
FreeSWITCH CLI Commands:

Basic Commands:
  status                    - Show system status
  version                   - Show FreeSWITCH version
  uptime                    - Show system uptime

Show Commands:
  show channels             - List active channels
  show channels count       - Show channel count
  show calls                - Show active calls
  show registrations        - Show SIP registrations
  show modules              - List loaded modules
  show interfaces           - Show interfaces

Control Commands:
  reload [module]           - Reload module or XML config
  originate <url> <dest>    - Originate a call

Function Key Shortcuts (customizable in config):
{}
Built-in Commands:
  /help                     - Show this help
  /quit, /exit, /bye        - Exit the CLI
  /history                  - Show command history
  /clear                    - Clear screen

Configuration:
  Profiles can be configured in ~/.config/fs_cli.yaml or /etc/freeswitch/fs_cli.yaml
  Use --config to specify a custom configuration file path
  Use --list-profiles to see available profiles
  Default configuration is created automatically if missing

You can execute any FreeSWITCH API command directly.
Use Tab for command completion and Up/Down arrows for history.
"#,
            fnkey_lines
        );

        let formatted_help = self
            .output
            .colorize(&help_text, |s| s.cyan());
        self.print_message(&formatted_help);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn failure_of(reply_text: &str) -> EslError {
        EslError::CommandFailed {
            reply_text: reply_text.to_string(),
        }
    }

    fn response_with_body(body: &str) -> EslResponse {
        EslResponse::new(indexmap::IndexMap::new(), Some(body.to_string()))
    }

    /// A successful command must never reach the error path: `api_result`
    /// answers a bare `+OK` with a ProtocolError that reads as a disconnect.
    #[test]
    fn only_a_refused_reply_becomes_an_error() {
        for body in ["+OK\n", "+OK 42 sessions\n", "", "   \n", "some payload\n"] {
            assert!(
                api_outcome(&response_with_body(body)).is_ok(),
                "body {:?} must be displayable, not an error",
                body
            );
        }

        for body in ["-ERR no such channel\n", "-USAGE: <uuid>\n", "-ERROR\n"] {
            let err = api_outcome(&response_with_body(body))
                .expect_err("a refusal must reach the error path");
            assert!(
                err.command_failure()
                    .is_some(),
                "body {:?} must carry a framable failure",
                body
            );
        }
    }

    #[test]
    fn a_displayed_body_is_the_wire_body_verbatim() {
        assert_eq!(
            api_outcome(&response_with_body("+OK\n")).unwrap(),
            "+OK\n",
            "the +OK prefix is what a bare +OK reply consists of"
        );
    }

    #[test]
    fn a_bare_error_reply_keeps_its_whole_text() {
        let err = failure_of("-ERROR");
        let failure = err
            .command_failure()
            .unwrap();
        assert_eq!(frame_failure(&failure), Some(("API Error", "-ERROR")));
    }

    #[test]
    fn err_and_usage_replies_are_peeled_and_labeled() {
        let err = failure_of("-ERR no such channel");
        assert_eq!(
            frame_failure(
                &err.command_failure()
                    .unwrap()
            ),
            Some(("API Error", "no such channel"))
        );

        let usage = failure_of("-USAGE: <uuid>");
        assert_eq!(
            frame_failure(
                &usage
                    .command_failure()
                    .unwrap()
            ),
            Some(("Usage", "<uuid>"))
        );
    }
}
