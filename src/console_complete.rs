//! FreeSWITCH console_complete API integration

use crate::channel_info::ChannelProvider;
use crate::completion::{Completion, CompletionRequest, COMPLETION_TIMEOUT};
use freeswitch_esl_tokio::{CommandTicket, EslClient, EslError};
use std::collections::HashSet;
use tracing::{trace, warn};

/// The ticket of a lookup that timed out, so its late reply can be told apart
/// from one the operator is waiting for.
fn note_abandoned(error: &EslError, abandoned: &mut HashSet<CommandTicket>) {
    if let EslError::CommandTimeout { ticket, .. } = error {
        abandoned.insert(*ticket);
    }
}

/// Get console completions from FreeSWITCH using the console_complete API
pub async fn get_console_complete(
    client: &EslClient,
    request: &CompletionRequest,
    channel_provider: &ChannelProvider,
    abandoned: &mut HashSet<CommandTicket>,
) -> Vec<Completion> {
    let line = request
        .line
        .as_str();
    let pos = request.pos;

    let cmd = if pos > 0 && pos < line.len() {
        format!("console_complete c={};{}", pos, line)
    } else {
        format!("console_complete {}", line)
    };

    let is_uuid_command = line
        .trim_start()
        .starts_with("uuid_")
        && line.contains(' ');

    trace!("ESL API: {}", cmd);

    if is_uuid_command {
        match channel_provider
            .get_uuid_completions(client)
            .await
        {
            Ok(Some(enhanced_completions)) => {
                trace!(
                    "Using enhanced UUID completion with {} channels",
                    enhanced_completions.len()
                );
                return enhanced_completions;
            }
            Ok(None) => trace!("Falling back to default UUID completion"),
            Err(e) => {
                if let Some(esl) = e.downcast_ref::<EslError>() {
                    note_abandoned(esl, abandoned);
                }
                warn!("UUID channel lookup failed, falling back: {:#}", e);
            }
        }
    }

    match client
        .api_with_timeout(&cmd, COMPLETION_TIMEOUT)
        .await
    {
        Ok(response) => match response.api_result() {
            Ok(body) => {
                trace!("ESL Response body (escaped): {:?}", body);
                let parsed = parse_console_complete_response(body);
                trace!("Parsed completions: {:?}", parsed);
                parsed
            }
            Err(e) => {
                trace!("No completions for '{}': {}", cmd, e.display_chain());
                Vec::new()
            }
        },
        Err(e) => {
            note_abandoned(&e, abandoned);
            tracing::debug!("Failed to get console completions: {}", e.display_chain());
            Vec::new()
        }
    }
}

/// Parse the console_complete response from FreeSWITCH
pub fn parse_console_complete_response(body: &str) -> Vec<Completion> {
    let candidates: Vec<Completion> = body
        .lines()
        .flat_map(bracketed_candidates)
        .map(Completion::Candidate)
        .collect();

    if !candidates.is_empty() {
        return candidates;
    }

    write_directive(body)
        .into_iter()
        .collect()
}

/// An unterminated bracket takes the rest of the line: FreeSWITCH truncates
/// long candidate lists mid-entry.
fn bracketed_candidates(line: &str) -> Vec<String> {
    let mut candidates = Vec::new();
    let mut rest = line;

    while let Some((_, after)) = rest.split_once('[') {
        let (content, tail) = after
            .split_once(']')
            .unwrap_or((after, ""));
        let text = content.trim();
        if !text.is_empty() {
            candidates.push(text.to_string());
        }
        rest = tail;
    }

    candidates
}

fn write_directive(body: &str) -> Option<Completion> {
    let (_, directive) = body.split_once("write=")?;
    let (_, replacement) = directive.split_once(':')?;
    let replacement = replacement.trim_end();

    (!replacement.is_empty()).then(|| Completion::Write(replacement.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bracket_list_yields_one_candidate_per_bracket() {
        let completions = parse_console_complete_response("[status]\t[reload]\t[shutdown]");
        assert_eq!(completions.len(), 3);
        assert!(matches!(&completions[0], Completion::Candidate(s) if s == "status"));
        assert!(matches!(&completions[1], Completion::Candidate(s) if s == "reload"));
        assert!(matches!(&completions[2], Completion::Candidate(s) if s == "shutdown"));
    }

    #[test]
    fn write_fallback_yields_write() {
        let completions = parse_console_complete_response("write=5:status profile");
        assert_eq!(completions.len(), 1);
        assert!(matches!(&completions[0], Completion::Write(s) if s == "status profile"));
    }

    #[test]
    fn an_unterminated_bracket_takes_the_rest_of_the_line() {
        let completions = parse_console_complete_response("[status]\t[reloa");
        assert_eq!(completions.len(), 2);
        assert!(matches!(&completions[1], Completion::Candidate(s) if s == "reloa"));
    }

    #[test]
    fn write_payload_keeps_inner_spaces_and_drops_the_trailing_newline() {
        let completions = parse_console_complete_response("write=5:status profile \n");
        assert!(matches!(&completions[0], Completion::Write(s) if s == "status profile"));
    }

    #[test]
    fn empty_body_yields_nothing() {
        assert!(parse_console_complete_response("").is_empty());
    }

    #[test]
    fn brackets_win_over_write_in_same_body() {
        let completions = parse_console_complete_response("[status]\nwrite=5:status profile");
        assert_eq!(completions.len(), 1);
        assert!(matches!(&completions[0], Completion::Candidate(s) if s == "status"));
    }
}
