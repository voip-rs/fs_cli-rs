// qual:allow(srp, file_length=333) reason: "project cap is 2000 lines per module"
//! Tab completion support for fs_cli-rs

use crate::log_level::LogSetting;
use rustyline::completion::{
    extract_word, longest_common_prefix, Completer, FilenameCompleter, Pair,
};
use rustyline::highlight::{CmdKind, Highlighter, MatchingBracketHighlighter};
use rustyline::hint::{Hinter, HistoryHinter};
use rustyline::validate::{self, MatchingBracketValidator, Validator};
use rustyline::{Context, Helper};
use std::borrow::Cow::{self, Borrowed, Owned};
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::trace;

/// How long Tab waits for the switch's candidates, and each lookup for its reply.
pub const COMPLETION_TIMEOUT: Duration = Duration::from_millis(500);

/// Typed completion item returned from all completion sources
#[derive(Debug)]
pub enum Completion {
    /// Regular completion candidate (display and replacement are the same)
    Candidate(String),
    /// UUID completion: display is full channel info, replacement is the UUID followed by a space
    Uuid { uuid: String, display: String },
    /// Direct write directive — replaces the entire current token
    Write(String),
}

/// Completion request from readline thread to main thread
#[derive(Debug)]
pub struct CompletionRequest {
    pub line: String,
    pub pos: usize,
    pub response_tx: std::sync::mpsc::SyncSender<Vec<Completion>>,
}

/// Add a trailing space to the single candidate's replacement if not already present.
/// No-op when the slice is empty or has more than one element.
fn add_trailing_space(candidates: &mut [Pair]) {
    if let [candidate] = candidates {
        if !candidate
            .replacement
            .ends_with(' ')
        {
            candidate
                .replacement
                .push(' ');
        }
    }
}

const FS_COMMANDS: &[&str] = &[
    // Basic commands
    "status",
    "version",
    "uptime",
    "help",
    // Show commands
    "show",
    "show channels",
    "show channels count",
    "show calls",
    "show registrations",
    "show modules",
    "show interfaces",
    "show api",
    "show application",
    "show codec",
    "show file",
    "show timer",
    "show tasks",
    "show complete",
    // Control commands
    "reload",
    "reloadxml",
    "reload mod_sofia",
    "reload mod_dialplan_xml",
    "originate",
    // Sofia commands
    "sofia",
    "sofia status",
    "sofia profile",
    "sofia profile internal",
    "sofia profile external",
    "sofia global",
    // Channel commands
    "uuid_answer",
    "uuid_hangup",
    "uuid_transfer",
    "uuid_bridge",
    "uuid_park",
    "uuid_hold",
    "uuid_break",
    "uuid_kill",
    // Conference commands
    "conference",
    "conference list",
    "conference kick",
    "conference mute",
    "conference unmute",
    // System commands
    "fsctl",
    "fsctl pause",
    "fsctl resume",
    "fsctl shutdown",
    "fsctl crash",
    "fsctl send_sighup",
    "load",
    "unload",
    "bgapi",
    // Log commands
    "console",
    "log",
    "uuid_dump",
    // Database commands
    "db",
    "group",
    "user_exists",
    // Other common commands
    "hupall",
    "pause",
    "resume",
    "shutdown",
    "expr",
    "eval",
    "expand",
    "global_getvar",
    "global_setvar",
];

/// Turn ESL completion items into rustyline candidates.
///
/// When several candidates share a prefix longer than the typed word, every
/// replacement becomes that prefix so a Tab narrows instead of doing nothing.
fn esl_candidates(completions: Vec<Completion>, current_word: &str) -> Vec<Pair> {
    let mut candidates: Vec<Pair> = completions
        .into_iter()
        .filter_map(|completion| match completion {
            Completion::Write(text) => Some(Pair {
                display: text.clone(),
                replacement: text,
            }),
            Completion::Uuid { uuid, display } => uuid
                .starts_with(current_word)
                .then(|| Pair {
                    display,
                    replacement: format!("{} ", uuid),
                }),
            Completion::Candidate(s) => s
                .starts_with(current_word)
                .then(|| Pair {
                    display: s.clone(),
                    replacement: s,
                }),
        })
        .collect();

    add_trailing_space(&mut candidates);

    if candidates.len() > 1 {
        if let Some(lcp) = longest_common_prefix(&candidates).map(str::to_string) {
            if lcp.len() > current_word.len() {
                for candidate in &mut candidates {
                    candidate.replacement = lcp.clone();
                }
            }
        }
    }

    candidates
}

/// Completions for the level argument of `/log`/`log`, checked before the
/// `/`-prefix skip below since this is a client command, not an ESL one.
fn log_level_completions(line: &str, pos: usize) -> Option<(usize, Vec<Pair>)> {
    let trimmed_start = line.len()
        - line
            .trim_start()
            .len();
    let rest = &line[trimmed_start..];
    let head_len = rest
        .find(' ')
        .unwrap_or(rest.len());
    let head = &rest[..head_len];
    if !head.eq_ignore_ascii_case("log") && !head.eq_ignore_ascii_case("/log") {
        return None;
    }

    let head_end = trimmed_start + head_len;
    if pos <= head_end {
        return None;
    }
    // Cursor already past the level argument (a further word follows it).
    if line[head_end..pos]
        .trim_start()
        .contains(' ')
    {
        return None;
    }

    let (start, current_word) = extract_word(line, pos, None, |c| c == ' ');
    let candidates: Vec<Pair> = LogSetting::level_names()
        .into_iter()
        .filter(|name| name.starts_with(current_word))
        .map(|name| Pair {
            display: name.to_string(),
            replacement: name.to_string(),
        })
        .collect();
    Some((start, candidates))
}

/// FreeSWITCH CLI completer with command suggestions
pub struct FsCliCompleter {
    filename_completer: FilenameCompleter,
    history_hinter: HistoryHinter,
    bracket_highlighter: MatchingBracketHighlighter,
    bracket_validator: MatchingBracketValidator,
    completion_tx: Option<mpsc::UnboundedSender<CompletionRequest>>,
}

impl FsCliCompleter {
    pub fn new(completion_tx: mpsc::UnboundedSender<CompletionRequest>) -> Self {
        Self {
            filename_completer: FilenameCompleter::new(),
            history_hinter: HistoryHinter::new(),
            bracket_highlighter: MatchingBracketHighlighter::new(),
            bracket_validator: MatchingBracketValidator::new(),
            completion_tx: Some(completion_tx),
        }
    }

    /// Get command completions for a given input
    fn complete_command(&self, line: &str, pos: usize) -> rustyline::Result<(usize, Vec<Pair>)> {
        let (start, current_word) = extract_word(line, pos, None, |c| c == ' ');

        let matches: Vec<Pair> = FS_COMMANDS
            .iter()
            .copied()
            .filter(|cmd| {
                // For multi-word commands, check if they start with current line
                if cmd.starts_with(&line[..start]) {
                    // Get the next word in the command after current position
                    let remaining = &cmd[start..];
                    if let Some(next_space) = remaining.find(' ') {
                        let next_word = &remaining[..next_space];
                        next_word.starts_with(current_word)
                    } else {
                        remaining.starts_with(current_word)
                    }
                } else {
                    // Single word commands
                    start == 0 && cmd.starts_with(current_word)
                }
            })
            .map(|cmd| {
                // Extract just the word we're completing
                let remaining = &cmd[start..];
                let next_word = if let Some(space_pos) = remaining.find(' ') {
                    &remaining[..space_pos]
                } else {
                    remaining
                };

                Pair {
                    display: next_word.to_string(),
                    replacement: next_word[current_word.len()..].to_string(),
                }
            })
            .collect();

        Ok((pos, matches))
    }

    /// Get ESL-based completions from FreeSWITCH
    fn get_esl_completions(&self, line: &str, pos: usize) -> Vec<Completion> {
        trace!("get_esl_completions called for '{}' pos {}", line, pos);

        let Some(completion_tx) = &self.completion_tx else {
            trace!("No completion channel available");
            return Vec::new();
        };

        let (response_tx, response_rx) = std::sync::mpsc::sync_channel::<Vec<Completion>>(1);

        let request = CompletionRequest {
            line: line.to_string(),
            pos,
            response_tx,
        };

        if let Err(e) = completion_tx.send(request) {
            trace!("Failed to send completion request: {}", e);
            return Vec::new();
        }

        trace!("Sent completion request, waiting for response...");

        match response_rx.recv_timeout(COMPLETION_TIMEOUT) {
            Ok(completions) => {
                trace!("Received {} completions", completions.len());
                completions
            }
            Err(e) => {
                trace!("Completion response error: {}", e);
                Vec::new()
            }
        }
    }
}

impl Helper for FsCliCompleter {}

impl Completer for FsCliCompleter {
    type Candidate = Pair;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        ctx: &Context<'_>,
    ) -> rustyline::Result<(usize, Vec<Self::Candidate>)> {
        if let Some((start, mut candidates)) = log_level_completions(line, pos) {
            add_trailing_space(&mut candidates);
            return Ok((start, candidates));
        }

        if !line
            .trim_start()
            .starts_with('/')
        {
            let esl_completions = self.get_esl_completions(line, pos);

            if !esl_completions.is_empty() {
                let (start, current_word) = extract_word(line, pos, None, |c| c == ' ');
                let candidates = esl_candidates(esl_completions, current_word);
                if !candidates.is_empty() {
                    return Ok((start, candidates));
                }
            }
        }

        let (start, mut candidates) = self.complete_command(line, pos)?;
        add_trailing_space(&mut candidates);

        if candidates.is_empty() && (line.contains('/') || line.contains('\\')) {
            let (file_start, file_candidates) = self
                .filename_completer
                .complete(line, pos, ctx)?;
            return Ok((file_start, file_candidates));
        }

        Ok((start, candidates))
    }
}

impl Hinter for FsCliCompleter {
    type Hint = String;

    fn hint(&self, line: &str, pos: usize, ctx: &Context<'_>) -> Option<Self::Hint> {
        self.history_hinter
            .hint(line, pos, ctx)
    }
}

impl Highlighter for FsCliCompleter {
    fn highlight_prompt<'b, 's: 'b, 'p: 'b>(
        &'s self,
        prompt: &'p str,
        default: bool,
    ) -> Cow<'b, str> {
        if default {
            Borrowed(prompt)
        } else {
            Owned(format!("\x1b[1m{}\x1b[0m", prompt)) // Bold prompt when not default
        }
    }

    fn highlight_hint<'h>(&self, hint: &'h str) -> Cow<'h, str> {
        Owned(format!("\x1b[90m{}\x1b[0m", hint)) // Gray hint
    }

    fn highlight<'l>(&self, line: &'l str, pos: usize) -> Cow<'l, str> {
        self.bracket_highlighter
            .highlight(line, pos)
    }

    fn highlight_char(&self, line: &str, pos: usize, kind: CmdKind) -> bool {
        self.bracket_highlighter
            .highlight_char(line, pos, kind)
    }
}

impl Validator for FsCliCompleter {
    fn validate(
        &self,
        ctx: &mut validate::ValidationContext,
    ) -> rustyline::Result<validate::ValidationResult> {
        self.bracket_validator
            .validate(ctx)
    }

    fn validate_while_typing(&self) -> bool {
        self.bracket_validator
            .validate_while_typing()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_completes_levels_after_the_command_word() {
        let (start, candidates) = log_level_completions("/log e", 6).unwrap();
        assert_eq!(start, 5);
        let names: Vec<&str> = candidates
            .iter()
            .map(|p| {
                p.display
                    .as_str()
            })
            .collect();
        assert!(names.contains(&"err"));

        let (start, candidates) = log_level_completions("log ", 4).unwrap();
        assert_eq!(start, 4);
        assert!(candidates.len() > 1);
    }

    /// `/LOG debug` runs client-side, so its Tab must behave the same.
    #[test]
    fn log_completes_whatever_case_the_command_word_is_typed_in() {
        assert!(log_level_completions("/LOG ", 5).is_some());
        assert!(log_level_completions("LOG ", 4).is_some());
    }

    #[test]
    fn log_completion_does_not_fire_on_the_command_word_or_past_the_level() {
        assert!(log_level_completions("lo", 2).is_none());
        assert!(log_level_completions("log", 3).is_none());
        assert!(log_level_completions("log error extra", 15).is_none());
    }
}
