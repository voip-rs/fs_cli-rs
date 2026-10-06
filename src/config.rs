//! Configuration management for fs_cli-rs

use crate::esl_debug::EslDebugLevel;
use crate::log_level::LogSetting;
use crate::originate_check::OriginateCheck;
use crate::printer::ColorMode;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tracing::warn;

/// Top-level configuration structure matching the YAML format
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct FsCliConfig {
    pub fs_cli: HashMap<String, ProfileConfig>,
}

/// Configuration for a single profile. Every field a profile may leave out
/// falls back to `Default`, which states each default exactly once.
#[derive(Debug, Serialize, Clone)]
pub struct ProfileConfig {
    /// FreeSWITCH hostname or IP address
    pub host: String,

    /// FreeSWITCH ESL port
    pub port: u16,

    /// ESL password
    pub password: String,

    /// Username for authentication (optional)
    pub user: Option<String>,

    /// ESL debug level (0-7, higher = more verbose)
    pub debug: EslDebugLevel,

    /// Color mode for output
    pub color: ColorMode,

    /// History file path
    pub history_file: Option<PathBuf>,

    /// Connection timeout in milliseconds
    pub timeout: u64,

    /// Retry connection on failure
    pub retry: bool,

    /// Reconnect on connection loss
    pub reconnect: bool,

    /// Subscribe to events on startup
    pub events: bool,

    /// Log level for FreeSWITCH logs
    pub log_level: LogSetting,

    /// Disable automatic log subscription on startup
    pub quiet: bool,

    /// Function key macros
    pub macros: HashMap<String, String>,

    /// Maximum number of channels to show in auto-complete
    pub max_auto_complete_uuid: u32,

    /// What to do about an originate the switch may read differently
    pub originate_check: OriginateCheck,
}

impl Default for ProfileConfig {
    fn default() -> Self {
        Self {
            host: "localhost".to_string(),
            port: 8021,
            password: "ClueCon".to_string(),
            user: None,
            debug: EslDebugLevel::None,
            color: ColorMode::Line,
            history_file: None,
            timeout: 2000,
            retry: false,
            reconnect: false,
            events: false,
            log_level: LogSetting::Level(freeswitch_esl_tokio::LogLevel::Debug),
            quiet: false,
            macros: crate::readline::get_default_fnkeys(),
            max_auto_complete_uuid: 32,
            originate_check: OriginateCheck::Warn,
        }
    }
}

/// What a profile actually spelled out. A key left blank is YAML null, which
/// a plain `serde(default)` would reject rather than read as "unset".
#[derive(Deserialize, Default)]
#[serde(default)]
struct ProfileOverrides {
    host: Option<String>,
    port: Option<u16>,
    password: Option<String>,
    user: Option<String>,
    debug: Option<EslDebugLevel>,
    color: Option<ColorMode>,
    history_file: Option<PathBuf>,
    timeout: Option<u64>,
    retry: Option<bool>,
    reconnect: Option<bool>,
    events: Option<bool>,
    log_level: Option<LogSetting>,
    quiet: Option<bool>,
    macros: Option<HashMap<String, String>>,
    max_auto_complete_uuid: Option<u32>,
    originate_check: Option<OriginateCheck>,
}

impl<'de> Deserialize<'de> for ProfileConfig {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let set = ProfileOverrides::deserialize(d)?;
        let base = Self::default();
        Ok(Self {
            host: set
                .host
                .unwrap_or(base.host),
            port: set
                .port
                .unwrap_or(base.port),
            password: set
                .password
                .unwrap_or(base.password),
            user: set.user,
            debug: set
                .debug
                .unwrap_or(base.debug),
            color: set
                .color
                .unwrap_or(base.color),
            history_file: set.history_file,
            timeout: set
                .timeout
                .unwrap_or(base.timeout),
            retry: set
                .retry
                .unwrap_or(base.retry),
            reconnect: set
                .reconnect
                .unwrap_or(base.reconnect),
            events: set
                .events
                .unwrap_or(base.events),
            log_level: set
                .log_level
                .unwrap_or(base.log_level),
            quiet: set
                .quiet
                .unwrap_or(base.quiet),
            macros: set
                .macros
                .unwrap_or(base.macros),
            max_auto_complete_uuid: set
                .max_auto_complete_uuid
                .unwrap_or(base.max_auto_complete_uuid),
            originate_check: set
                .originate_check
                .unwrap_or(base.originate_check),
        })
    }
}

impl ProfileConfig {
    /// Convert to typed values for application use
    pub fn into_app_config(self) -> AppConfig {
        AppConfig {
            host: self.host,
            port: self.port,
            password: self.password,
            user: self.user,
            debug: self.debug,
            color: self.color,
            history_file: self.history_file,
            timeout: self.timeout,
            retry: self.retry,
            reconnect: self.reconnect,
            events: self.events,
            log_level: self.log_level,
            quiet: self.quiet,
            macros: self.macros,
            execute: Vec::new(),
            log_file: None,
            job_timeout: None,
            max_auto_complete_uuid: self.max_auto_complete_uuid,
            originate_check: self.originate_check,
        }
    }
}

/// One command given on the command line, in the order it was typed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BatchCommand {
    /// `-x`: a synchronous api call.
    Api(String),
    /// `-X`: a bgapi call, whose result arrives later as a BACKGROUND_JOB event.
    BgApi(String),
}

/// Typed application configuration after parsing and validation
#[derive(Debug, Clone)]
pub struct AppConfig {
    pub host: String,
    pub port: u16,
    pub password: String,
    pub user: Option<String>,
    pub debug: crate::esl_debug::EslDebugLevel,
    pub color: ColorMode,
    pub history_file: Option<PathBuf>,
    pub timeout: u64,
    pub retry: bool,
    pub reconnect: bool,
    pub events: bool,
    pub log_level: LogSetting,
    pub quiet: bool,
    pub macros: HashMap<String, String>,
    pub execute: Vec<BatchCommand>,
    pub log_file: Option<String>,
    pub job_timeout: Option<u64>,
    pub max_auto_complete_uuid: u32,
    pub originate_check: OriginateCheck,
}

/// The two names C fs_cli reads. Anything else is parsed as YAML, so an
/// explicit `--config` never silently changes format.
fn is_legacy(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension == "conf")
        || path
            .file_name()
            .is_some_and(|name| name == ".fs_cli_conf")
}

impl FsCliConfig {
    /// Load the explicit file, else the first default present, else write and
    /// use the default config. A missing explicit file is an error.
    pub fn load(config_path: Option<PathBuf>) -> Result<Self> {
        if let Some(path) = config_path {
            return Self::read_any(&path);
        }
        match Self::select_config_path(Self::get_default_config_paths(), |path| path.exists()) {
            Some(path) => Self::read_any(&path),
            None => {
                let default_config = Self::default();
                Self::write_default_config(&default_config);
                Ok(default_config)
            }
        }
    }

    fn select_config_path(
        defaults: Vec<PathBuf>,
        exists: impl Fn(&Path) -> bool,
    ) -> Option<PathBuf> {
        defaults
            .into_iter()
            .find(|path| exists(path))
    }

    fn read_any(path: &Path) -> Result<Self> {
        if is_legacy(path) {
            crate::legacy_config::read(path)
        } else {
            Self::read_file(path)
        }
    }

    fn read_file(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read config file {}", path.display()))?;
        serde_yaml::from_str(&content)
            .with_context(|| format!("Failed to parse config file {}", path.display()))
    }

    /// Best effort: an unwritable config dir must not stop a session that
    /// already has usable defaults in hand.
    fn write_default_config(config: &Self) {
        let Some(config_dir) = dirs::config_dir() else {
            warn!("No user config directory, not writing a default config");
            return;
        };

        if let Err(e) = std::fs::create_dir_all(&config_dir) {
            warn!(
                "Could not create config directory {}: {}",
                config_dir.display(),
                e
            );
        }

        let path = config_dir.join("fs_cli.yaml");
        match serde_yaml::to_string(config) {
            Ok(yaml) => {
                if let Err(e) = std::fs::write(&path, yaml) {
                    warn!(
                        "Could not write default config to {}: {}",
                        path.display(),
                        e
                    );
                }
            }
            Err(e) => warn!("Could not serialize default config: {}", e),
        }
    }

    /// Get list of default configuration file paths to try
    fn get_default_config_paths() -> Vec<PathBuf> {
        let mut paths = Vec::new();

        if let Some(config_dir) = dirs::config_dir() {
            paths.push(config_dir.join("fs_cli.yaml"));
        }

        if let Some(home_dir) = dirs::home_dir() {
            paths.push(home_dir.join(".fs_cli.yaml"));
        }

        paths.push(PathBuf::from("/etc/freeswitch/fs_cli.yaml"));

        if let Some(home_dir) = dirs::home_dir() {
            paths.push(home_dir.join(".fs_cli_conf"));
        }

        paths.push(PathBuf::from("/etc/fs_cli.conf"));

        paths
    }

    /// Get a profile by name
    pub fn get_profile(&self, name: &str) -> Result<ProfileConfig> {
        self.fs_cli
            .get(name)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("Profile '{}' not found", name))
    }

    /// Get list of available profile names
    pub fn get_profile_names(&self) -> Vec<String> {
        self.fs_cli
            .keys()
            .cloned()
            .collect()
    }
}

impl Default for FsCliConfig {
    fn default() -> Self {
        let mut fs_cli = HashMap::new();
        fs_cli.insert("default".to_string(), ProfileConfig::default());

        Self { fs_cli }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidates() -> Vec<PathBuf> {
        vec![
            PathBuf::from("/home/u/.config/fs_cli.yaml"),
            PathBuf::from("/home/u/.fs_cli.yaml"),
            PathBuf::from("/etc/freeswitch/fs_cli.yaml"),
        ]
    }

    #[test]
    fn a_missing_explicit_path_is_an_error_naming_it() {
        for name in ["no-such-fs_cli.yaml", "no-such-fs_cli.conf"] {
            let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("target")
                .join(name);
            let err = FsCliConfig::load(Some(path.clone())).unwrap_err();
            assert!(format!("{:#}", err).contains(
                &path
                    .display()
                    .to_string()
            ));
        }
    }

    #[test]
    fn the_defaults_are_tried_in_the_documented_order() {
        for first in 0..candidates().len() {
            let present = candidates().split_off(first);
            let chosen = FsCliConfig::select_config_path(candidates(), |path| {
                present
                    .iter()
                    .any(|p| p == path)
            });
            assert_eq!(chosen, Some(candidates()[first].clone()));
        }
    }

    #[test]
    fn no_candidate_present_selects_nothing() {
        assert_eq!(
            FsCliConfig::select_config_path(candidates(), |_| false),
            None
        );
    }

    #[test]
    fn default_paths_end_at_the_system_wide_file() {
        let paths = FsCliConfig::get_default_config_paths();
        assert_eq!(paths.last(), Some(&PathBuf::from("/etc/fs_cli.conf")));
        assert!(paths
            .iter()
            .any(|p| p.ends_with(".config/fs_cli.yaml")));
        assert!(paths
            .iter()
            .any(|p| p.ends_with(".fs_cli.yaml")));
    }

    #[test]
    fn every_yaml_candidate_is_tried_before_any_legacy_one() {
        let paths = FsCliConfig::get_default_config_paths();
        let first_legacy = paths
            .iter()
            .position(|p| is_legacy(p))
            .expect("a legacy candidate");

        assert!(paths[..first_legacy]
            .iter()
            .all(|p| !is_legacy(p)));
        assert_eq!(
            paths[first_legacy - 1],
            PathBuf::from("/etc/freeswitch/fs_cli.yaml")
        );
    }

    #[test]
    fn only_the_two_names_c_fs_cli_reads_are_legacy() {
        assert!(is_legacy(Path::new("/etc/fs_cli.conf")));
        assert!(is_legacy(Path::new("/home/u/.fs_cli_conf")));
        assert!(!is_legacy(Path::new("/etc/freeswitch/fs_cli.yaml")));
        assert!(!is_legacy(Path::new("/srv/fs_cli")));
    }

    #[test]
    fn test_omitted_fields_take_the_defaults() {
        let yaml_content = r#"
fs_cli:
  sparse:
    host: fs.example.test
"#;
        let config: FsCliConfig = serde_yaml::from_str(yaml_content).unwrap();
        let app = config
            .get_profile("sparse")
            .unwrap()
            .into_app_config();
        let defaults = ProfileConfig::default();
        assert_eq!(app.host, "fs.example.test");
        assert_eq!(app.port, defaults.port);
        assert_eq!(app.timeout, defaults.timeout);
        assert_eq!(app.macros, defaults.macros);
        assert!(!app.retry);
    }

    /// A key left blank mid-edit is YAML null. It must read as "unset", not
    /// fail the whole file and take every other profile in it down.
    #[test]
    fn a_blank_value_falls_back_like_a_missing_key() {
        let yaml_content = r#"
fs_cli:
  blanks:
    host:
    port:
    password:
    timeout:
    retry:
    color:
    log_level:
    macros:
    originate_check:
"#;
        let config: FsCliConfig = serde_yaml::from_str(yaml_content).unwrap();
        let app = config
            .get_profile("blanks")
            .unwrap()
            .into_app_config();
        let defaults = ProfileConfig::default();
        assert_eq!(app.originate_check, defaults.originate_check);
        assert_eq!(app.host, defaults.host);
        assert_eq!(app.port, defaults.port);
        assert_eq!(app.password, defaults.password);
        assert_eq!(app.timeout, defaults.timeout);
        assert_eq!(app.color, defaults.color);
        assert_eq!(app.log_level, defaults.log_level);
        assert_eq!(app.macros, defaults.macros);
        assert!(!app.retry);
    }

    #[test]
    fn test_typed_field_parsing() {
        let yaml_content = r#"
fs_cli:
  p1:
    color: tag
    log_level: warn
    debug: 5
    history_file: /tmp/hist
"#;
        let config: FsCliConfig = serde_yaml::from_str(yaml_content).unwrap();
        let profile = config
            .get_profile("p1")
            .unwrap();
        assert_eq!(profile.color, ColorMode::Tag);
        assert_eq!(
            profile.log_level,
            LogSetting::Level(freeswitch_esl_tokio::LogLevel::Warning)
        );
        assert_eq!(profile.debug, crate::esl_debug::EslDebugLevel::Debug5);
        assert_eq!(
            profile.history_file,
            Some(std::path::PathBuf::from("/tmp/hist"))
        );

        let app = profile.into_app_config();
        assert_eq!(app.color, ColorMode::Tag);
        assert_eq!(
            app.log_level,
            LogSetting::Level(freeswitch_esl_tokio::LogLevel::Warning)
        );
        assert_eq!(app.debug, crate::esl_debug::EslDebugLevel::Debug5);
        assert_eq!(
            app.history_file,
            Some(std::path::PathBuf::from("/tmp/hist"))
        );
    }

    #[test]
    fn test_typed_fields_invalid_rejects_at_load() {
        let bad_color = r#"
fs_cli:
  p:
    color: rainbow
"#;
        let result: Result<FsCliConfig, _> = serde_yaml::from_str(bad_color);
        assert!(result.is_err());

        let bad_log = r#"
fs_cli:
  p:
    log_level: loudest
"#;
        let result: Result<FsCliConfig, _> = serde_yaml::from_str(bad_log);
        assert!(result.is_err());

        let bad_debug = r#"
fs_cli:
  p:
    debug: 99
"#;
        let result: Result<FsCliConfig, _> = serde_yaml::from_str(bad_debug);
        assert!(result.is_err());

        let bad_check = r#"
fs_cli:
  p:
    originate_check: maybe
"#;
        let result: Result<FsCliConfig, _> = serde_yaml::from_str(bad_check);
        assert!(result.is_err());
    }

    #[test]
    fn test_default_config_round_trips() {
        let default_cfg = FsCliConfig::default();
        let yaml = serde_yaml::to_string(&default_cfg).unwrap();
        let reparsed: FsCliConfig = serde_yaml::from_str(&yaml).unwrap();
        let profile = reparsed
            .get_profile("default")
            .unwrap();
        assert_eq!(profile.color, ColorMode::Line);
        assert_eq!(
            profile.log_level,
            LogSetting::Level(freeswitch_esl_tokio::LogLevel::Debug)
        );
        assert_eq!(profile.debug, crate::esl_debug::EslDebugLevel::None);
    }
}
