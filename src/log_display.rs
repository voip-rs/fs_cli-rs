//! Log display functionality for fs_cli-rs

use crate::printer::{ColorMode, LogSink, Output, Printer};
use anyhow::{Context, Result};
use colored::{ColoredString, Colorize};
use freeswitch_esl_tokio::{EslEvent, EslEventType, EventHeader, HeaderLookup};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tracing::debug;

pub fn is_log_event(event: &EslEvent) -> bool {
    event
        .header_str("Content-Type")
        .is_some_and(|ct| ct.eq_ignore_ascii_case("log/data"))
}

/// FreeSWITCH's numeric DEBUG level, assumed when Log-Level is missing or unparseable.
const DEFAULT_LOG_LEVEL: u32 = 7;

/// Display a log event with appropriate formatting and colors.
pub fn display_log_event(event: &EslEvent, output: &Output) {
    if let Some(line) = format_log_line(event, output.color()) {
        output.print(line);
    }
}

/// One display line for a log event, or None when it carries no text.
pub fn format_log_line(event: &EslEvent, color: ColorMode) -> Option<String> {
    let log_level = event
        .header(EventHeader::LogLevel)
        .and_then(|raw| {
            raw.parse::<u32>()
                .ok()
                .or_else(|| {
                    debug!(
                        "unparseable Log-Level {:?}, defaulting to {}",
                        raw, DEFAULT_LOG_LEVEL
                    );
                    None
                })
        })
        .unwrap_or(DEFAULT_LOG_LEVEL);

    let message = event
        .body()
        .unwrap_or("")
        .trim();
    if message.is_empty() {
        return None;
    }

    Some(match color {
        ColorMode::Never => message.to_string(),
        ColorMode::Tag => format_colored_log_tag_only(message, log_level),
        ColorMode::Line => format_colored_log_full_line(message, log_level),
    })
}

/// The `--log-file` destination: log lines only, on their own writer.
#[derive(Clone)]
pub struct LogDestination {
    printer: Printer,
    color: ColorMode,
    broken: Arc<AtomicBool>,
}

impl LogDestination {
    /// `-` is stdout, anything else a file opened for append.
    pub fn open(spec: &str, requested: ColorMode) -> Result<Self> {
        let (sink, color) = if spec == "-" {
            (LogSink::stdout(), requested)
        } else {
            let file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(spec)
                .with_context(|| format!("cannot open log destination {}", spec))?;
            (LogSink::file(file), ColorMode::Never)
        };
        let broken = sink.broken_flag();
        Ok(Self {
            printer: Printer::with_external(sink),
            color,
            broken,
        })
    }

    /// Write a log event; anything else is left to the caller.
    pub fn write_event(&self, event: &EslEvent) {
        if let Some(line) = format_log_line(event, self.color) {
            self.printer
                .print(line);
        }
    }

    /// True once a write failed; the run ends instead of retrying every line.
    pub fn is_broken(&self) -> bool {
        self.broken
            .load(Ordering::Relaxed)
    }
}

/// `<number> name` for a channel, or None when neither is set.
pub fn format_caller_id(number: &str, name: &str) -> Option<String> {
    (!number.is_empty() || !name.is_empty()).then(|| format!("<{}> {}", number, name))
}

/// One line for a channel lifecycle event, or None for anything else.
pub fn format_channel_event(event: &EslEvent, output: &Output) -> Option<String> {
    let event_type = event.event_type()?;

    let label = match event_type {
        EslEventType::ChannelCreate => "CREATE",
        EslEventType::ChannelAnswer => "ANSWER",
        EslEventType::ChannelHangup => "HANGUP",
        _ => return None,
    };

    let channel = event
        .channel_name()
        .unwrap_or("unknown");
    let uuid = event
        .unique_id()
        .unwrap_or("?");

    let line = if event_type == EslEventType::ChannelHangup {
        let cause = match event.hangup_cause() {
            Ok(Some(c)) => c.to_string(),
            Ok(None) => "unknown".to_string(),
            Err(e) => format!("{:#}", anyhow::Error::new(e)),
        };
        format!("[{}] {} {} ({})", label, uuid, channel, cause)
    } else {
        let caller_id = format_caller_id(
            event
                .caller_id_number()
                .unwrap_or(""),
            event
                .caller_id_name()
                .unwrap_or(""),
        );
        match caller_id {
            Some(cid) => format!("[{}] {} {} {}", label, uuid, channel, cid),
            None => format!("[{}] {} {}", label, uuid, channel),
        }
    };

    Some(output.colorize(&line, |s| s.cyan()))
}

fn colorize_by_level(text: &str, log_level: u32) -> ColoredString {
    match log_level {
        0 => text
            .white()
            .bold(), // CONSOLE
        1 | 2 => text
            .red()
            .bold(), // ALERT / CRIT
        3 => text.red(),    // ERR
        4 => text.yellow(), // WARNING
        5 => text.cyan(),   // NOTICE
        6 => text.green(),  // INFO
        _ => text
            .yellow()
            .dimmed(), // DEBUG and higher
    }
}

fn format_colored_log_tag_only(message: &str, log_level: u32) -> String {
    if let Some(level_start) = message.find('[') {
        if let Some(level_end) = message[level_start..].find(']') {
            let level_end = level_start + level_end + 1;
            let before = &message[..level_start];
            let level_tag = &message[level_start..level_end];
            let after = &message[level_end..];

            let colored_level = colorize_by_level(level_tag, log_level);

            return format!("{}{}{}", before, colored_level, after);
        }
    }

    message.to_string()
}

fn format_colored_log_full_line(message: &str, log_level: u32) -> String {
    colorize_by_level(message, log_level).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use freeswitch_esl_tokio::EslEvent;

    #[test]
    fn is_log_event_with_log_data_content_type() {
        let mut event = EslEvent::new();
        event.set_header("Content-Type", "log/data");
        event.set_header("Log-Level", "6");
        assert!(is_log_event(&event));
    }

    #[test]
    fn is_log_event_rejects_non_log_events() {
        let mut event = EslEvent::new();
        event.set_header("Event-Name", "CHANNEL_CREATE");
        assert!(!is_log_event(&event));

        let empty_event = EslEvent::new();
        assert!(!is_log_event(&empty_event));
    }

    fn log_event(level: &str, body: &str) -> EslEvent {
        let mut event = EslEvent::new();
        event.set_header("Content-Type", "log/data");
        event.set_header("Log-Level", level);
        event.set_body(body);
        event
    }

    #[test]
    fn format_log_line_trims_and_leaves_never_uncolored() {
        let line = format_log_line(&log_event("3", "  boom  \n"), ColorMode::Never);
        assert_eq!(line, Some("boom".to_string()));
    }

    #[test]
    fn format_log_line_skips_an_empty_body() {
        assert_eq!(
            format_log_line(&log_event("6", "  \n"), ColorMode::Never),
            None
        );
        assert_eq!(format_log_line(&EslEvent::new(), ColorMode::Never), None);
    }

    #[test]
    fn format_log_line_keeps_the_text_in_every_color_mode() {
        for mode in [ColorMode::Never, ColorMode::Tag, ColorMode::Line] {
            let line = format_log_line(&log_event("3", "x [ERR] boom"), mode).expect("a log line");
            assert!(
                line.contains("[ERR]") && line.contains("boom"),
                "{:?}",
                line
            );
        }
    }

    #[test]
    fn format_colored_log_tag_only_with_tag() {
        let message = "2024 [NOTICE] something happened";
        let formatted = format_colored_log_tag_only(message, 5);
        assert!(formatted.contains("2024 "));
        assert!(formatted.contains("[NOTICE]"));
        assert!(formatted.contains(" something happened"));
    }

    #[test]
    fn format_colored_log_tag_only_without_tag() {
        let message = "no bracket tag here";
        assert_eq!(format_colored_log_tag_only(message, 5), message);
    }
}
