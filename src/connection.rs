//! Connecting to FreeSWITCH, subscribing, and reporting connection failures.

use crate::config::AppConfig;
use anyhow::{Context, Result};
use freeswitch_esl_tokio::{
    AuthMethod, EslClient, EslConnectOptions, EslError, EslEventStream, EslEventType, EventFormat,
};
use tokio::time::Duration;
use tracing::{info, warn};

/// Connect to FreeSWITCH with timeout
pub(crate) async fn connect_to_freeswitch(
    config: &AppConfig,
) -> Result<(EslClient, EslEventStream)> {
    info!(
        "Connecting to FreeSWITCH at {}",
        format_host_port(&config.host, config.port)
    );

    let auth = match &config.user {
        Some(user) => {
            info!("Using user authentication: {}", user);
            AuthMethod::user(user, &config.password)
        }
        None => {
            info!("Using password authentication");
            AuthMethod::password(&config.password)
        }
    };
    let options =
        EslConnectOptions::default().with_connect_timeout(Duration::from_millis(config.timeout));

    let (client, events) = EslClient::connect_with_auth(&config.host, config.port, auth, options)
        .await
        .context("Failed to connect to FreeSWITCH")?;

    // Without this the library's own 5s default governs every api call, and
    // -T would silently mean "connect only".
    client.set_command_timeout(Duration::from_millis(config.timeout));

    Ok((client, events))
}

/// Retry connecting until it succeeds or the switch refuses these credentials.
pub(crate) async fn connect_retrying(config: &AppConfig) -> Result<(EslClient, EslEventStream)> {
    loop {
        match connect_to_freeswitch(config).await {
            Ok(pair) => return Ok(pair),
            Err(e) if is_refused(&e) => return Err(e),
            Err(e) => {
                warn!("Connection attempt failed: {:#}", e);
                info!("Retrying in {} ms...", config.timeout);
                tokio::time::sleep(Duration::from_millis(config.timeout)).await;
            }
        }
    }
}

pub(crate) async fn connect_to_freeswitch_with_retry(
    config: &AppConfig,
) -> Result<(EslClient, EslEventStream)> {
    if !config.retry {
        return connect_to_freeswitch(config).await;
    }
    info!(
        "Retry mode enabled - will retry every {} ms",
        config.timeout
    );
    connect_retrying(config).await
}

/// Auth rejection and ACL denial are configuration faults; retrying repeats them.
fn is_refused(error: &anyhow::Error) -> bool {
    // qual:allow(coupling, deh) reason: "classifying EslError is what this asks"
    matches!(
        error.downcast_ref::<EslError>(),
        Some(EslError::AuthenticationFailed { .. } | EslError::AccessDenied { .. })
    )
}

/// Check if error indicates connection loss
pub(crate) fn is_connection_error(error: &anyhow::Error) -> bool {
    // qual:allow(coupling, deh) reason: "classifying EslError is what this asks"
    error
        .downcast_ref::<EslError>()
        .is_some_and(|e| e.is_connection_error())
}

/// A lost connection or an unanswered request: the switch was not reachable.
pub(crate) fn is_unreachable(error: &anyhow::Error) -> bool {
    // qual:allow(coupling, deh) reason: "classifying EslError is what this asks"
    error
        .downcast_ref::<EslError>()
        .is_some_and(|e| e.is_connection_error() || matches!(e, EslError::Timeout { .. }))
}

/// Check if error is an ESL permission denial (e.g. an event the user is not
/// allowed to subscribe to). Used to gate the idle-liveness timer: a restricted
/// user who can't subscribe to HEARTBEAT has no idle traffic source.
pub(crate) fn is_permission_denied(error: &anyhow::Error) -> bool {
    // qual:allow(coupling, deh) reason: "classifying EslError is what this asks"
    error
        .downcast_ref::<EslError>()
        .is_some_and(|e| e.is_permission_denied())
}

/// Subscribe to events for monitoring
pub(crate) async fn subscribe_to_events(client: &EslClient) -> Result<()> {
    info!("Subscribing to events...");
    client
        .subscribe_events(
            EventFormat::Plain,
            &[
                EslEventType::ChannelCreate,
                EslEventType::ChannelAnswer,
                EslEventType::ChannelHangup,
                EslEventType::Heartbeat,
            ],
        )
        .await?;
    info!("Event monitoring enabled");
    Ok(())
}

/// Subscribe to heartbeat events only, to keep the liveness timer alive
pub(crate) async fn subscribe_heartbeat(client: &EslClient) -> Result<()> {
    client
        .subscribe_events(EventFormat::Plain, &[EslEventType::Heartbeat])
        .await?;
    Ok(())
}

/// Enable logging at the specified level
pub(crate) async fn enable_logging(
    client: &EslClient,
    log_level: crate::log_level::LogSetting,
) -> Result<()> {
    info!("Enabling logging at level: {}", log_level);
    if let Some(reply) = crate::log_level::set_log_level(client, log_level).await? {
        warn!("Failed to set log level: {}", reply);
    }
    Ok(())
}
fn format_host_port(host: &str, port: u16) -> String {
    if host.contains(':') {
        format!("[{}]:{}", host, port)
    } else {
        format!("{}:{}", host, port)
    }
}

fn print_io_hint(io_err: &std::io::Error, config: &AppConfig) {
    match io_err.kind() {
        std::io::ErrorKind::ConnectionRefused => {
            eprintln!(
                "Connection refused - is FreeSWITCH running and listening on port {}?",
                config.port
            );
        }
        std::io::ErrorKind::TimedOut => {
            eprintln!("Connection timed out after {} ms", config.timeout);
        }
        _ => {
            eprintln!("IO error: {}", io_err);
        }
    }
}

/// Turning a connection failure into advice the user can act on is this
/// module's job, so it inspects the error itself rather than passing it up.
pub(crate) fn print_connect_error(e: &anyhow::Error, config: &AppConfig) {
    // qual:allow(coupling, deh) reason: "the failure hint is built from the error"
    if let Some(EslError::AuthenticationFailed { reason }) = e.downcast_ref::<EslError>() {
        eprintln!("Authentication failed: {}", reason);
        return;
    }

    eprintln!(
        "Failed to connect to FreeSWITCH at {}",
        format_host_port(&config.host, config.port)
    );

    // qual:allow(coupling, deh) reason: "the failure hint is built from the error"
    if let Some(esl_err) = e.downcast_ref::<EslError>() {
        match esl_err {
            EslError::Io(io_err) => print_io_hint(io_err, config),
            EslError::Timeout { timeout_ms } => {
                eprintln!("Connection timed out after {} ms", timeout_ms)
            }
            _ => eprintln!("Error: {}", esl_err),
        }
    // qual:allow(coupling, deh) reason: "the failure hint is built from the error"
    } else if let Some(io_err) = e.downcast_ref::<std::io::Error>() {
        print_io_hint(io_err, config);
    } else {
        eprintln!("Error: {}", e);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_connection_error_with_esl_errors() {
        let err: anyhow::Error = EslError::ConnectionClosed.into();
        assert!(is_connection_error(&err));

        let err: anyhow::Error = EslError::NotConnected.into();
        assert!(is_connection_error(&err));

        let io_err = std::io::Error::new(std::io::ErrorKind::ConnectionReset, "reset");
        let err: anyhow::Error = EslError::from(io_err).into();
        assert!(is_connection_error(&err));

        let err: anyhow::Error = EslError::Timeout { timeout_ms: 1000 }.into();
        assert!(!is_connection_error(&err));

        let err: anyhow::Error = EslError::auth_failed("bad password").into();
        assert!(!is_connection_error(&err));
    }

    #[test]
    fn test_is_connection_error_with_io_errors() {
        // IO errors wrapped in EslError are connection errors
        let io_err = std::io::Error::new(std::io::ErrorKind::ConnectionReset, "reset");
        let err: anyhow::Error = EslError::from(io_err).into();
        assert!(is_connection_error(&err));

        // Bare io::Error not wrapped in EslError is not recognized
        let io_err = std::io::Error::new(std::io::ErrorKind::ConnectionReset, "reset");
        let err: anyhow::Error = io_err.into();
        assert!(!is_connection_error(&err));
    }

    #[test]
    fn retry_stops_only_on_refused_credentials() {
        let err: anyhow::Error = EslError::auth_failed("-ERR invalid").into();
        assert!(is_refused(&err.context("Failed to connect to FreeSWITCH")));

        let err: anyhow::Error = EslError::AccessDenied {
            reason: "rude-rejection".into(),
        }
        .into();
        assert!(is_refused(&err));

        let io_err = std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "refused");
        let err: anyhow::Error = EslError::from(io_err).into();
        assert!(!is_refused(&err));

        let err: anyhow::Error = EslError::Timeout { timeout_ms: 1000 }.into();
        assert!(!is_refused(&err));
    }

    #[test]
    fn test_is_connection_error_with_other_errors() {
        let err = anyhow::anyhow!("Some random error");
        assert!(!is_connection_error(&err));
    }
}
