//! Channel information management for enhanced UUID completion

use crate::completion::{Completion, COMPLETION_TIMEOUT};
use crate::log_display::format_caller_id;
use anyhow::{Context, Result};
use freeswitch_esl_tokio::EslClient;
use serde::Deserialize;

/// Channel information from FreeSWITCH JSON output
#[derive(Debug, Clone, Deserialize)]
pub struct ChannelInfo {
    pub uuid: String,
    pub created: String,
    pub created_epoch: String,
    pub name: String,
    pub state: String,
    #[serde(default)]
    pub cid_name: String,
    #[serde(default)]
    pub cid_num: String,
}

/// Wrapper for FreeSWITCH JSON response
#[derive(Debug, Deserialize)]
pub struct ChannelsResponse {
    pub row_count: u32,
    /// mod_commands answers an empty result with `{"row_count": 0}` and no
    /// `rows` key at all.
    #[serde(default)]
    pub rows: Vec<ChannelInfo>,
}

/// Channel information provider with smart fetching
pub struct ChannelProvider {
    max_channels: u32,
}

impl ChannelProvider {
    /// Create new channel provider with configurable limit
    pub fn new(max_channels: u32) -> Self {
        Self { max_channels }
    }

    /// Get enhanced UUID completions with channel info.
    ///
    /// Returns `None` if the channel count exceeds the configured limit (fall back
    /// to default console_complete). Each `Completion::Uuid` carries the full
    /// channel line as `display` and the bare UUID as `replacement`.
    pub async fn get_uuid_completions(
        &self,
        client: &EslClient,
    ) -> Result<Option<Vec<Completion>>> {
        let response = self
            .fetch_channels_json(client)
            .await?;

        if response.row_count == 0 {
            return Ok(Some(Vec::new()));
        }

        // The count check gates the display list, not the transfer: this
        // fetch already paid for the full row set before we know the count.
        if response.row_count > self.max_channels {
            tracing::debug!(
                "Too many channels ({}) for enhanced completion, limit is {}. Falling back to default.",
                response.row_count, self.max_channels
            );
            return Ok(None);
        }

        let mut channels = response.rows;
        channels.sort_by(|a, b| {
            let a_epoch: u64 = a
                .created_epoch
                .parse()
                .unwrap_or(0);
            let b_epoch: u64 = b
                .created_epoch
                .parse()
                .unwrap_or(0);
            b_epoch.cmp(&a_epoch)
        });

        let completions = channels
            .into_iter()
            .map(|ch| {
                let head = format!("{} {} {} ({})", ch.uuid, ch.created, ch.name, ch.state);
                let display = match format_caller_id(&ch.cid_num, &ch.cid_name) {
                    Some(caller_id) => format!("{} {}", head, caller_id),
                    None => head,
                };
                Completion::Uuid {
                    uuid: ch.uuid,
                    display,
                }
            })
            .collect();

        Ok(Some(completions))
    }

    // qual:allow(srp, slm) reason: "small helper pairing the ESL call with JSON parsing; another layer would not earn its place"
    async fn fetch_channels_json(&self, client: &EslClient) -> Result<ChannelsResponse> {
        const COMMAND: &str = "show channels as json";
        let response = client
            .api_with_timeout(COMMAND, COMPLETION_TIMEOUT)
            .await
            .with_context(|| format!("ESL API call '{}' failed", COMMAND))?;

        let body = response
            .api_result()
            .with_context(|| format!("ESL command '{}' failed", COMMAND))?;
        serde_json::from_str::<ChannelsResponse>(body)
            .with_context(|| format!("Failed to parse JSON response for '{}'", COMMAND))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_result_carries_no_rows_key() {
        let parsed: ChannelsResponse = serde_json::from_str(r#"{"row_count": 0}"#).unwrap();
        assert_eq!(parsed.row_count, 0);
        assert!(parsed
            .rows
            .is_empty());
    }

    #[test]
    fn rows_deserialize_with_the_optional_caller_id_absent() {
        let body = r#"{"row_count":1,"rows":[{"uuid":"u","created":"c","created_epoch":"1",
                       "name":"sofia/n","state":"CS_EXECUTE"}]}"#;
        let parsed: ChannelsResponse = serde_json::from_str(body).unwrap();
        assert_eq!(parsed.rows[0].cid_num, "");
    }
}
