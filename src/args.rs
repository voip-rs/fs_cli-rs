//! Command-line argument parsing for fs_cli-rs

use crate::config::{AppConfig, BatchCommand, FsCliConfig, ProfileConfig};
use crate::esl_debug::EslDebugLevel;
use crate::log_level::LogSetting;
use crate::originate_check::OriginateCheck;
use crate::printer::ColorMode;
use anyhow::Result;
use clap::{ArgMatches, CommandFactory, FromArgMatches, Parser};
use std::path::PathBuf;

/// Interactive FreeSWITCH CLI client
#[derive(Parser, Debug, Clone)]
#[command(author, version, about, long_about = None)]
pub struct Args {
    /// Profile to use from configuration file
    #[arg(value_name = "PROFILE")]
    pub profile: Option<String>,

    /// FreeSWITCH hostname or IP address
    #[arg(short = 'H', long)]
    pub host: Option<String>,

    /// FreeSWITCH ESL port
    #[arg(short = 'P', long)]
    pub port: Option<u16>,

    /// ESL password
    #[arg(short = 'p', long)]
    pub password: Option<String>,

    /// Username for userauth (format: user@domain, e.g., admin@default)
    #[arg(short, long)]
    pub user: Option<String>,

    /// ESL debug level (0-7, higher = more verbose)
    #[arg(short, long)]
    pub debug: Option<u8>,

    /// Color mode for output (never, tag, line)
    #[arg(long, ignore_case = true)]
    pub color: Option<ColorMode>,

    /// Report what the switch will install for an originate (off, warn, fix)
    #[arg(long, ignore_case = true)]
    pub originate_check: Option<OriginateCheck>,

    /// With -x/-X: stop at a refused command and exit 3
    #[arg(long, num_args = 0..=1, default_missing_value = "true", action = clap::ArgAction::Set)]
    pub fail_on_error: Option<bool>,

    /// Execute commands and exit (can be used multiple times)
    #[arg(short = 'x', action = clap::ArgAction::Append, value_parser = single_line)]
    pub execute: Vec<String>,

    /// Execute commands as background jobs (bgapi), interleaved with -x
    #[arg(short = 'X', action = clap::ArgAction::Append, value_parser = single_line)]
    pub bg_execute: Vec<String>,

    /// Write the FreeSWITCH log stream to PATH ("-" for stdout)
    #[arg(long, value_name = "PATH")]
    pub log_file: Option<String>,

    /// Give up on -X results after MS milliseconds (default: wait forever)
    #[arg(long, value_name = "MS")]
    pub job_timeout: Option<u64>,

    /// History file path
    #[arg(long)]
    pub history_file: Option<PathBuf>,

    /// Connection timeout in milliseconds
    #[arg(short = 'T', long = "connect-timeout")]
    pub timeout: Option<u64>,

    /// Retry connection on failure
    #[arg(short, long, num_args = 0..=1, default_missing_value = "true", action = clap::ArgAction::Set)]
    pub retry: Option<bool>,

    /// Reconnect on connection loss
    #[arg(short = 'R', long, num_args = 0..=1, default_missing_value = "true", action = clap::ArgAction::Set)]
    pub reconnect: Option<bool>,

    /// Subscribe to events on startup
    #[arg(long, num_args = 0..=1, default_missing_value = "true", action = clap::ArgAction::Set)]
    pub events: Option<bool>,

    /// Log level for FreeSWITCH logs
    #[arg(short = 'l', long)]
    pub log_level: Option<LogSetting>,

    /// Disable automatic log subscription on startup
    #[arg(short = 'q', long, num_args = 0..=1, default_missing_value = "true", action = clap::ArgAction::Set)]
    pub quiet: Option<bool>,

    /// Configuration file path (if missing, a default is written)
    #[arg(long)]
    pub config: Option<PathBuf>,

    /// List available configuration profiles
    #[arg(long)]
    pub list_profiles: bool,
}

impl Args {
    /// Parse arguments and merge with configuration
    pub fn parse_and_merge() -> Result<AppConfig> {
        let matches = Self::command().get_matches();
        let args = Self::from_arg_matches(&matches)?;

        let config = FsCliConfig::load(
            args.config
                .clone(),
        )?;

        if args.list_profiles {
            Self::print_profiles_and_exit(&config);
        }

        let mut app_config = Self::resolve_profile(
            &config,
            args.profile
                .as_deref(),
        )?;
        args.apply_to(&mut app_config)?;
        app_config.execute = ordered_commands(&matches);
        Ok(app_config)
    }

    /// `--list-profiles`: print every configured profile name and exit.
    fn print_profiles_and_exit(config: &FsCliConfig) -> ! {
        println!("Available profiles:");
        let mut profile_names = config.get_profile_names();
        profile_names.sort();
        for name in profile_names {
            println!("  {}", name);
        }
        std::process::exit(0);
    }

    /// The named profile, or the default profile when none was named, or an
    /// error listing what is available when a named one does not exist.
    fn resolve_profile(config: &FsCliConfig, profile: Option<&str>) -> Result<AppConfig> {
        let profile_name = profile.unwrap_or("default");
        let explicitly_named = profile.is_some();

        match config.get_profile(profile_name) {
            Ok(profile) => Ok(profile.into_app_config()),
            Err(_) if !explicitly_named => Ok(ProfileConfig::default().into_app_config()),
            Err(_) => {
                let mut names = config.get_profile_names();
                names.sort();
                Err(anyhow::anyhow!(
                    "Profile '{}' not found. Available profiles: {}",
                    profile_name,
                    names.join(", ")
                ))
            }
        }
    }

    /// Apply CLI argument overrides to an already-loaded AppConfig.
    // qual:allow(complexity, max_cyclomatic=15) reason: "flat sequence of one if-let override per CLI flag; the clearest form, splitting would scatter it"
    pub fn apply_to(&self, config: &mut AppConfig) -> Result<()> {
        if let Some(host) = &self.host {
            config.host = host.clone();
        }
        if let Some(port) = self.port {
            config.port = port;
        }
        if let Some(password) = &self.password {
            config.password = password.clone();
        }
        if let Some(user) = &self.user {
            config.user = Some(user.clone());
        }
        if let Some(debug) = self.debug {
            config.debug = EslDebugLevel::try_from(debug)?;
        }
        if let Some(color) = self.color {
            config.color = color;
        }
        if let Some(originate_check) = self.originate_check {
            config.originate_check = originate_check;
        }
        if let Some(fail_on_error) = self.fail_on_error {
            config.fail_on_error = fail_on_error;
        }
        if let Some(log_file) = &self.log_file {
            config.log_file = Some(log_file.clone());
        }
        if let Some(job_timeout) = self.job_timeout {
            config.job_timeout = Some(job_timeout);
        }
        if let Some(history_file) = &self.history_file {
            config.history_file = Some(history_file.clone());
        }
        if let Some(timeout) = self.timeout {
            config.timeout = timeout;
        }
        if let Some(retry) = self.retry {
            config.retry = retry;
        }
        if let Some(reconnect) = self.reconnect {
            config.reconnect = reconnect;
        }
        if let Some(events) = self.events {
            config.events = events;
        }
        if let Some(log_level) = self.log_level {
            config.log_level = log_level;
        }
        if let Some(quiet) = self.quiet {
            config.quiet = quiet;
        }
        Ok(())
    }
}

/// Refused here rather than by the library, which would fail it only after
/// the batch counts a command as sent.
fn single_line(command: &str) -> Result<String, String> {
    if command.contains(['\n', '\r', '\0']) {
        return Err("a command must not contain a line break or NUL".to_string());
    }
    Ok(command.to_string())
}

/// `-x` and `-X` in the order they were typed. The derived `Vec<String>` fields
/// keep each flag's values apart, so only clap's indices restore the sequence.
fn ordered_commands(matches: &ArgMatches) -> Vec<BatchCommand> {
    let mut ordered: Vec<(usize, BatchCommand)> = Vec::new();
    for (id, wrap) in [
        ("execute", BatchCommand::Api as fn(String) -> BatchCommand),
        ("bg_execute", BatchCommand::BgApi),
    ] {
        let values = matches
            .get_many::<String>(id)
            .into_iter()
            .flatten();
        let indices = matches
            .indices_of(id)
            .into_iter()
            .flatten();
        ordered.extend(
            values
                .zip(indices)
                .map(|(value, index)| (index, wrap(value.clone()))),
        );
    }
    ordered.sort_by_key(|(index, _)| *index);
    ordered
        .into_iter()
        .map(|(_, command)| command)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::Args;
    use crate::config::{AppConfig, BatchCommand};
    use crate::esl_debug::EslDebugLevel;
    use crate::log_level::LogSetting;
    use crate::originate_check::OriginateCheck;
    use crate::printer::ColorMode;
    use clap::CommandFactory;
    use freeswitch_esl_tokio::LogLevel;
    use std::collections::HashMap;

    /// The YAML side parses case-insensitively; the flag must agree with it.
    #[test]
    fn color_accepts_any_case() {
        use clap::Parser;
        for spelling in ["never", "NEVER", "Never"] {
            let args = Args::try_parse_from(["fs_cli", "--color", spelling]).unwrap();
            assert_eq!(args.color, Some(ColorMode::Never));
        }
        assert!(Args::try_parse_from(["fs_cli", "--color", "rainbow"]).is_err());
    }

    #[test]
    fn a_command_spanning_lines_is_refused() {
        use clap::Parser;
        for flag in ["-x", "-X"] {
            for command in ["status\nexit", "status\0"] {
                assert!(Args::try_parse_from(["fs_cli", flag, command]).is_err());
            }
            assert!(Args::try_parse_from(["fs_cli", flag, "status"]).is_ok());
        }
    }

    fn make_args_no_overrides() -> Args {
        Args {
            profile: None,
            host: None,
            port: None,
            password: None,
            user: None,
            debug: None,
            color: None,
            originate_check: None,
            fail_on_error: None,
            execute: Vec::new(),
            bg_execute: Vec::new(),
            log_file: None,
            job_timeout: None,
            history_file: None,
            timeout: None,
            retry: None,
            reconnect: None,
            events: None,
            log_level: None,
            quiet: None,
            config: None,
            list_profiles: false,
        }
    }

    fn base_app_config() -> AppConfig {
        AppConfig {
            host: "localhost".to_string(),
            port: 8021,
            password: "test".to_string(),
            user: None,
            debug: EslDebugLevel::None,
            color: ColorMode::Line,
            log_file: None,
            job_timeout: None,
            history_file: None,
            timeout: 2000,
            retry: true,
            reconnect: true,
            events: true,
            log_level: LogSetting::Level(LogLevel::Debug),
            quiet: true,
            macros: HashMap::new(),
            execute: Vec::new(),
            max_auto_complete_uuid: 32,
            originate_check: OriginateCheck::Warn,
            fail_on_error: false,
        }
    }

    #[test]
    fn test_apply_to_preserves_config_when_no_cli_overrides() {
        let mut config = base_app_config();
        make_args_no_overrides()
            .apply_to(&mut config)
            .unwrap();
        assert!(config.retry);
        assert!(config.reconnect);
        assert!(config.events);
        assert!(config.quiet);
    }

    #[test]
    fn test_apply_to_overrides_config_with_cli_args() {
        let mut config = base_app_config();
        config.retry = false;
        config.reconnect = false;
        config.events = false;
        config.quiet = false;

        let mut args = make_args_no_overrides();
        args.retry = Some(true);
        args.reconnect = Some(true);
        args.events = Some(true);
        args.quiet = Some(true);

        args.apply_to(&mut config)
            .unwrap();
        assert!(config.retry);
        assert!(config.reconnect);
        assert!(config.events);
        assert!(config.quiet);
    }

    #[test]
    fn test_apply_to_host_and_port_override() {
        let mut config = base_app_config();
        let mut args = make_args_no_overrides();
        args.host = Some("192.168.1.1".to_string());
        args.port = Some(9021);

        args.apply_to(&mut config)
            .unwrap();
        assert_eq!(config.host, "192.168.1.1");
        assert_eq!(config.port, 9021);
    }

    /// `apply_to` no longer touches `execute`; the ordered list replaces it
    /// wholesale in `parse_and_merge`.
    #[test]
    fn test_apply_to_leaves_execute_alone() {
        let mut config = base_app_config();
        config.execute = vec![BatchCommand::Api("prior".to_string())];

        make_args_no_overrides()
            .apply_to(&mut config)
            .unwrap();
        assert_eq!(config.execute, vec![BatchCommand::Api("prior".to_string())]);
    }

    fn ordered_from(argv: &[&str]) -> Vec<BatchCommand> {
        let matches = Args::command()
            .try_get_matches_from(argv)
            .unwrap();
        super::ordered_commands(&matches)
    }

    #[test]
    fn no_command_flags_yield_nothing() {
        assert!(ordered_from(&["fs_cli"]).is_empty());
    }

    #[test]
    fn each_flag_keeps_its_own_order() {
        assert_eq!(
            ordered_from(&["fs_cli", "-x", "one", "-x", "two"]),
            vec![
                BatchCommand::Api("one".to_string()),
                BatchCommand::Api("two".to_string()),
            ]
        );
        assert_eq!(
            ordered_from(&["fs_cli", "-X", "one", "-X", "two"]),
            vec![
                BatchCommand::BgApi("one".to_string()),
                BatchCommand::BgApi("two".to_string()),
            ]
        );
    }

    #[test]
    fn interleaved_flags_keep_the_typed_order() {
        assert_eq!(
            ordered_from(&["fs_cli", "-x", "one", "-X", "two", "-x", "three"]),
            vec![
                BatchCommand::Api("one".to_string()),
                BatchCommand::BgApi("two".to_string()),
                BatchCommand::Api("three".to_string()),
            ]
        );
    }
}
