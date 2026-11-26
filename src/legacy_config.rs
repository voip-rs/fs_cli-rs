//! Reader for the C fs_cli `fs_cli.conf` format, for boxes whose scripts still
//! ship one. Only the keys a `-x`/`-X` run acts on are imported; the rest are
//! named once and dropped.

use crate::config::{FsCliConfig, ProfileConfig};
use crate::esl_debug::EslDebugLevel;
use crate::log_level::LogSetting;
use anyhow::{Context, Result};
use std::collections::{BTreeSet, HashMap};
use std::path::Path;
use std::str::FromStr;
use tracing::warn;

pub fn read(path: &Path) -> Result<FsCliConfig> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read legacy config file {}", path.display()))?;

    warn!(
        "Reading legacy config {}: keys outside host, port, user, password, debug, loglevel, quiet, timeout and connect-timeout are ignored. Convert it to YAML.",
        path.display()
    );

    Ok(parse(&content, path))
}

fn parse(content: &str, path: &Path) -> FsCliConfig {
    let mut fs_cli: HashMap<String, ProfileConfig> = HashMap::new();
    let mut ignored = BTreeSet::new();
    let mut connect_timeout = None;
    let mut command_timeout = None;

    for (category, var, val) in pairs(content) {
        if var.eq_ignore_ascii_case("connect-timeout") {
            connect_timeout = parse_number(&var, &val, path);
            continue;
        }
        if var.eq_ignore_ascii_case("timeout") {
            command_timeout = parse_number(&var, &val, path);
            continue;
        }

        let profile = fs_cli
            .entry(category)
            .or_default();

        if !apply(profile, &var, &val, path) {
            ignored.insert(var);
        }
    }

    if let Some(timeout) = connect_timeout {
        for profile in fs_cli.values_mut() {
            profile.timeout = timeout;
        }
    }
    if let Some(timeout) = command_timeout {
        for profile in fs_cli.values_mut() {
            profile.command_timeout = Some(timeout);
        }
    }

    if !ignored.is_empty() {
        warn!(
            "Ignored {} key(s) in {}: {}",
            ignored.len(),
            path.display(),
            ignored
                .into_iter()
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    FsCliConfig { fs_cli }
}

/// Returns false for a key this reader does not carry over.
fn apply(profile: &mut ProfileConfig, var: &str, val: &str, path: &Path) -> bool {
    match var
        .to_ascii_lowercase()
        .as_str()
    {
        "host" => profile.host = val.to_string(),
        "user" => profile.user = Some(val.to_string()),
        "password" => profile.password = val.to_string(),
        "quiet" => profile.quiet = esl_true(val),
        "port" => {
            if let Some(port) = parse_number(var, val, path) {
                profile.port = port;
            }
        }
        "debug" => match parse_number::<u8>(var, val, path).map(EslDebugLevel::try_from) {
            Some(Ok(debug)) => profile.debug = debug,
            Some(Err(e)) => warn!("{}: debug={}: {}", path.display(), val, e),
            None => {}
        },
        "loglevel" => match LogSetting::from_str(val) {
            Ok(level) => profile.log_level = level,
            Err(e) => warn!("{}: loglevel={}: {}", path.display(), val, e),
        },
        _ => return false,
    }
    true
}

fn parse_number<T: FromStr>(var: &str, val: &str, path: &Path) -> Option<T> {
    match val.parse() {
        Ok(number) => Some(number),
        Err(_) => {
            warn!(
                "{}: {}={} is not a number, ignored",
                path.display(),
                var,
                val
            );
            None
        }
    }
}

/// `esl_true`: the words C fs_cli accepts, plus any non-zero number.
fn esl_true(val: &str) -> bool {
    ["yes", "on", "true", "enabled", "active", "allow"]
        .iter()
        .any(|word| val.eq_ignore_ascii_case(word))
        || val
            .parse::<i64>()
            .is_ok_and(|number| number != 0)
}

/// `esl_config_next_pair`, minus the section locking fs_cli never turns on.
/// Values keep trailing whitespace because the C parser does.
fn pairs(content: &str) -> Vec<(String, String, String)> {
    let mut out = Vec::new();
    let mut category = String::new();

    for line in content.lines() {
        if let Some(name) = line
            .strip_prefix('[')
            .and_then(|rest| {
                rest.split(']')
                    .next()
            })
        {
            if !name.starts_with('+') {
                category = name.to_string();
            }
            continue;
        }

        if line.starts_with(['#', ';']) {
            continue;
        }

        if line.starts_with("__END__") {
            break;
        }

        let line = match line.find(';') {
            Some(i) if line[i + 1..].starts_with(';') => &line[..i],
            _ => line,
        };

        let Some((var, val)) = line.split_once('=') else {
            continue;
        };

        out.push((
            category.clone(),
            var.trim()
                .to_string(),
            val.strip_prefix('>')
                .unwrap_or(val)
                .trim_start()
                .to_string(),
        ));
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn parse_str(content: &str) -> FsCliConfig {
        parse(content, &PathBuf::from("/etc/fs_cli.conf"))
    }

    #[test]
    fn a_section_becomes_a_profile_carrying_the_batch_keys() {
        let config = parse_str(
            "[keepalived]\nhost=10.0.0.1\nport=8022\nuser=keepalived@system.local\npassword=secret\nloglevel=warning\nquiet=true\ndebug=3\n",
        );
        let profile = config
            .get_profile("keepalived")
            .expect("profile");

        assert_eq!(profile.host, "10.0.0.1");
        assert_eq!(profile.port, 8022);
        assert_eq!(
            profile
                .user
                .as_deref(),
            Some("keepalived@system.local")
        );
        assert_eq!(profile.password, "secret");
        assert!(profile.quiet);
        assert_eq!(profile.debug, EslDebugLevel::try_from(3).expect("level"));
        assert_eq!(
            profile
                .log_level
                .to_string(),
            "warning"
        );
    }

    #[test]
    fn keys_a_profile_omits_keep_the_built_in_defaults() {
        let config = parse_str("[sparse]\nhost=10.0.0.2\n");
        let profile = config
            .get_profile("sparse")
            .expect("profile");
        let default = ProfileConfig::default();

        assert_eq!(profile.port, default.port);
        assert_eq!(profile.password, default.password);
        assert_eq!(profile.user, default.user);
    }

    #[test]
    fn connect_timeout_reaches_every_profile_as_the_c_global_does() {
        let config = parse_str("[a]\nhost=10.0.0.1\nconnect-timeout=5000\n[b]\nhost=10.0.0.2\n");

        for name in ["a", "b"] {
            assert_eq!(
                config
                    .get_profile(name)
                    .expect("profile")
                    .timeout,
                5000
            );
        }
    }

    #[test]
    fn timeout_is_the_command_timeout_not_the_connect_one() {
        let config = parse_str("[a]\nhost=10.0.0.1\ntimeout=7000\n[b]\nhost=10.0.0.2\n");

        for name in ["a", "b"] {
            let profile = config
                .get_profile(name)
                .expect("profile");
            assert_eq!(profile.command_timeout, Some(7000));
            assert_eq!(profile.timeout, ProfileConfig::default().timeout);
        }
    }

    #[test]
    fn unimported_keys_leave_the_profile_alone() {
        let config = parse_str("[p]\nprompt-string=x\nlog-uuid=true\nkey_F1=help\n");
        let profile = config
            .get_profile("p")
            .expect("profile");

        assert_eq!(profile.host, ProfileConfig::default().host);
        assert_eq!(profile.macros, ProfileConfig::default().macros);
    }

    #[test]
    fn comments_and_end_marker_are_honoured() {
        let config =
            parse_str("[p]\n; comment\n# comment\nhost=10.0.0.1 ;; trailing\n__END__\nport=9999\n");
        let profile = config
            .get_profile("p")
            .expect("profile");

        assert_eq!(profile.host, "10.0.0.1 ");
        assert_eq!(profile.port, ProfileConfig::default().port);
    }

    #[test]
    fn an_arrow_assignment_reads_like_a_plain_one() {
        assert_eq!(
            parse_str("[p]\nhost=>10.0.0.3\n")
                .get_profile("p")
                .expect("profile")
                .host,
            "10.0.0.3"
        );
    }

    #[test]
    fn a_plus_section_does_not_open_a_profile() {
        let config = parse_str("[p]\nhost=10.0.0.1\n[+group]\nport=8022\n");

        assert_eq!(config.get_profile_names(), vec!["p".to_string()]);
        assert_eq!(
            config
                .get_profile("p")
                .expect("profile")
                .port,
            8022
        );
    }

    #[test]
    fn every_word_the_c_parser_calls_true_is_true() {
        for word in ["yes", "ON", "True", "enabled", "active", "allow", "1", "-2"] {
            assert!(esl_true(word), "{word}");
        }
        for word in ["no", "off", "false", "0", ""] {
            assert!(!esl_true(word), "{word}");
        }
    }
}
