//! Commands handled by the client itself, parsed before anything is sent.

use crate::log_level::LogSetting;
use std::fmt;
use std::str::FromStr;

/// A line the client answers itself.
#[derive(Debug, PartialEq)]
pub enum ClientCommand {
    Help,
    Clear,
    History,
    Quit,
    /// `/jobs`: the jobs still waiting for their result.
    Jobs,
    /// `/log <level>`, or bare `/log` for the level list.
    Log(Option<LogSetting>),
}

#[derive(Debug, PartialEq)]
pub enum ParseError {
    /// The line belongs to FreeSWITCH, not to us.
    NotClientCommand,
    /// `/log` named a level that does not exist; the line stops here rather
    /// than reaching the server as an api command.
    InvalidLogLevel(String),
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParseError::NotClientCommand => write!(f, "not a client command"),
            ParseError::InvalidLogLevel(level) => write!(f, "Invalid log level: {}", level),
        }
    }
}

impl FromStr for ClientCommand {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut parts = s.split_whitespace();
        let head = parts
            .next()
            .ok_or(ParseError::NotClientCommand)?
            .to_lowercase();

        match head.as_str() {
            "/help" | "help" => Ok(ClientCommand::Help),
            "/clear" => Ok(ClientCommand::Clear),
            "/history" => Ok(ClientCommand::History),
            "/quit" | "/exit" | "/bye" => Ok(ClientCommand::Quit),
            "/jobs" => Ok(ClientCommand::Jobs),
            "/log" | "log" => match parts.next() {
                None => Ok(ClientCommand::Log(None)),
                Some(level) => level
                    .parse::<LogSetting>()
                    .map(|l| ClientCommand::Log(Some(l)))
                    .map_err(|_| ParseError::InvalidLogLevel(level.to_string())),
            },
            _ => Err(ParseError::NotClientCommand),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_every_alias() {
        assert_eq!("/help".parse(), Ok(ClientCommand::Help));
        assert_eq!("help".parse(), Ok(ClientCommand::Help));
        assert_eq!("/clear".parse(), Ok(ClientCommand::Clear));
        assert_eq!("/history".parse(), Ok(ClientCommand::History));
        assert_eq!("/jobs".parse(), Ok(ClientCommand::Jobs));
        for quit in ["/quit", "/exit", "/bye"] {
            assert_eq!(quit.parse(), Ok(ClientCommand::Quit));
        }
    }

    #[test]
    fn log_takes_an_optional_level() {
        assert_eq!("/log".parse(), Ok(ClientCommand::Log(None)));
        assert_eq!(
            "log DEBUG".parse(),
            Ok(ClientCommand::Log(Some(LogSetting::Level(
                freeswitch_esl_tokio::LogLevel::Debug
            ))))
        );
    }

    #[test]
    fn bad_log_level_never_reaches_the_server() {
        assert_eq!(
            "/log bogus".parse::<ClientCommand>(),
            Err(ParseError::InvalidLogLevel("bogus".to_string()))
        );
    }

    #[test]
    fn server_commands_are_not_client_commands() {
        assert_eq!(
            "status".parse::<ClientCommand>(),
            Err(ParseError::NotClientCommand)
        );
        assert_eq!(
            "uptime".parse::<ClientCommand>(),
            Err(ParseError::NotClientCommand)
        );
    }
}
