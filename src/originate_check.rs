//! What the switch will install for a typed `originate`.
//!
//! The switch consumes escapes in a dial string before installing anything, so a value written
//! with an apostrophe rarely arrives carrying one. This reads the line the way the switch does and
//! names the values the channel receives.

use freeswitch_esl_tokio::commands::{
    originate_split, BlockParse, DialStringCarrier, DialStringTarget, FlattenedDialString,
    OriginateError, Variables, VariablesType,
};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::ops::Range;

/// The switch's block-parser revision the check reads and renders for; the only one
/// freeswitch-types knows, and every vouched release runs it.
const BLOCK_PARSE: BlockParse = BlockParse::PairSplitCleans;

/// What fs_cli does about an `originate` whose dial string the switch may read differently.
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::EnumString, strum::Display, clap::ValueEnum)]
#[strum(serialize_all = "lowercase", ascii_case_insensitive)]
#[clap(rename_all = "lowercase")]
pub enum OriginateCheck {
    /// Send the command as typed.
    Off,
    /// Report the values the channel will receive.
    Warn,
    /// Rewrite a block whose values the switch would not deliver as typed, when the rewrite
    /// verifies; report and send the line unchanged when it does not.
    Fix,
}

impl Serialize for OriginateCheck {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for OriginateCheck {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        raw.parse()
            .map_err(|_| {
                serde::de::Error::custom(format!(
                    "Invalid originate check: {}. Valid options: off, warn, fix",
                    raw
                ))
            })
    }
}

/// What the switch will do with one command.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Checked {
    /// What to tell the operator before the command goes out.
    pub report: Option<String>,
    /// The line to send in place of the one typed.
    pub rewritten: Option<String>,
}

/// Read `command` as the switch reads it, under `policy`.
pub fn check(policy: OriginateCheck, command: &str) -> Checked {
    match policy {
        OriginateCheck::Off => Checked::default(),
        OriginateCheck::Warn => Checked {
            report: report(command),
            rewritten: None,
        },
        OriginateCheck::Fix => fix(command),
    }
}

/// Rewrite the blocks the switch would not deliver as typed, or say why the line stands.
fn fix(command: &str) -> Checked {
    let Some(arguments) = originate_arguments(command) else {
        return Checked::default();
    };
    let target = target_for(arguments);
    let dial_at = command.len() - arguments.len() + separator_len(arguments);
    let blocks: Vec<Range<usize>> = head_block_ranges(&command[dial_at..])
        .into_iter()
        .map(|block| (dial_at + block.start)..(dial_at + block.end))
        .filter(|block| correctable(&command[block.clone()]))
        .collect();
    if blocks.is_empty() {
        return Checked {
            report: report(command),
            rewritten: None,
        };
    }

    let mut rewritten = String::new();
    let mut cursor = 0;
    for block in blocks {
        match rewrite_block(&command[block.clone()], target) {
            Ok(text) => {
                rewritten.push_str(&command[cursor..block.start]);
                rewritten.push_str(&text);
                cursor = block.end;
            }
            Err(why) => {
                return Checked {
                    report: Some(format!("sent as typed: {why}")),
                    rewritten: None,
                }
            }
        }
    }
    rewritten.push_str(&command[cursor..]);
    Checked {
        report: None,
        rewritten: Some(rewritten),
    }
}

/// Whether a block's text names values this can read back with one meaning.
///
/// A backslash makes the reading ambiguous — it may belong to the value or protect what follows —
/// and a block carrying one is usually escaped on purpose, so it stands as written.
fn correctable(block: &str) -> bool {
    block.contains('\'') && !block.contains('\\')
}

/// A block rendered so the channel receives the values as its text reads them, verified by
/// reading the rendering back.
fn rewrite_block(block: &str, target: DialStringTarget) -> Result<String, String> {
    let scope = match block
        .as_bytes()
        .first()
    {
        Some(b'<') => VariablesType::Enterprise,
        Some(b'[') => VariablesType::Channel,
        _ => VariablesType::Default,
    };
    let inner = &block[1..block.len() - 1];
    if inner.starts_with("^^") {
        return Err("a block naming its own separator reads by that separator".into());
    }
    let mut variables = Variables::new(scope);
    let mut intended = Vec::new();
    for pair in inner.split(',') {
        let (key, value) = pair
            .split_once('=')
            .ok_or("a pair carries no =")?;
        variables
            .insert(key, value)
            .map_err(|e| format!("the switch would not deliver {key}: {e}"))?;
        intended.push((key.to_string(), value.to_string()));
    }

    let rendered = variables.render(target);
    let read_back = Variables::parse_for(&rendered, target)
        .map_err(|e| format!("the rewrite reads back as {e}"))?;
    let delivered: Vec<(String, String)> = read_back
        .iter()
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect();
    if delivered != intended {
        return Err("no escaping of this block delivers it as written".into());
    }
    Ok(rendered)
}

fn report(command: &str) -> Option<String> {
    let arguments = originate_arguments(command)?;
    let target = target_for(arguments);
    let tokens = match originate_split(arguments, ' ') {
        Ok(tokens) => tokens,
        Err(e) => return Some(format!("originate's argument split refuses this line: {e}")),
    };
    let dial = tokens.first()?;
    let asked = depth_sensitive(dial) || opens_unclosed_block(dial);
    let list = match FlattenedDialString::parse_for(dial, target) {
        Ok(list) => list,
        Err(e) => return asked.then(|| format!("the switch reads this dial string as {e}")),
    };

    let mut warnings: Vec<String> = list
        .warnings()
        .iter()
        .map(ToString::to_string)
        .collect();
    for leg in list.legs() {
        warnings.extend(
            leg.warnings()
                .iter()
                .map(ToString::to_string),
        );
    }
    let (mut values, mut faults) = delivered(dial, target);
    for leg in list.legs() {
        let (leg_values, leg_faults) = delivered(leg.raw(), target);
        values.extend(leg_values);
        faults.extend(leg_faults);
    }
    if !asked && faults.is_empty() && warnings.is_empty() {
        return None;
    }

    let mut parts = Vec::new();
    if !values.is_empty() {
        parts.push(format!("the switch will set {}", values.join(", ")));
    }
    parts.extend(faults);
    parts.extend(warnings);
    (!parts.is_empty()).then(|| parts.join("; "))
}

/// The pairs each bracket block at the head of `text` installs, as the switch installs them, and
/// whether a block installs something its text does not describe.
fn delivered(text: &str, target: DialStringTarget) -> (Vec<String>, Vec<String>) {
    let mut values = Vec::new();
    let mut faults = Vec::new();
    for block in head_block_ranges(text) {
        match Variables::parse_for(&text[block], target) {
            Ok(variables) => values.extend(
                variables
                    .iter()
                    .map(|(key, value)| format!("{key}={value:?}")),
            ),
            // The variant's own prefix names a parse, which says nothing to an operator who
            // typed a dial string rather than asked for one to be read.
            Err(OriginateError::ParseError(text)) => faults.push(text),
            Err(e) => faults.push(e.to_string()),
        }
    }
    (values, faults)
}

/// The `originate` arguments of `command`, past an `api` or `bgapi` prefix.
fn originate_arguments(command: &str) -> Option<&str> {
    let rest = command.trim_start();
    let rest = strip_word(rest, "bgapi")
        .or_else(|| strip_word(rest, "api"))
        .unwrap_or(rest);
    strip_word(rest, "originate")
}

/// `text` past a leading `word` and the blank after it.
fn strip_word<'a>(text: &'a str, word: &str) -> Option<&'a str> {
    let tail = text
        .get(..word.len())
        .filter(|head| head.eq_ignore_ascii_case(word))
        .map(|_| &text[word.len()..])?;
    tail.starts_with(|c: char| c.is_ascii_whitespace())
        .then(|| tail.trim_start())
}

/// Whether the dial string carries anything whose delivered value depends on escaping depth.
fn depth_sensitive(dial: &str) -> bool {
    dial.contains('\'') || dial.contains('\\') || dial.contains(":_:")
}

/// A block the argument split cut short, which an unescaped space in a value produces.
fn opens_unclosed_block(dial: &str) -> bool {
    dial.starts_with(['{', '<', '[']) && head_block_end(dial).is_none()
}

/// The bytes a leading `^^` separator takes before the dial string.
fn separator_len(arguments: &str) -> usize {
    arguments
        .strip_prefix("^^")
        .and_then(|rest| {
            rest.chars()
                .next()
        })
        .map_or(0, |separator| 2 + separator.len_utf8())
}

/// A line opening `^^` names the separator its arguments split on.
fn target_for(arguments: &str) -> DialStringTarget {
    let base = DialStringTarget::new(DialStringCarrier::EslApi, BLOCK_PARSE);
    arguments
        .strip_prefix("^^")
        .and_then(|rest| {
            rest.chars()
                .next()
        })
        .and_then(|separator| {
            base.with_argv_separator(separator)
                .ok()
        })
        .unwrap_or(base)
}

/// The bracket blocks opening `text`, read as `switch_find_end_paren` reads them: depth counted,
/// no escape honoured.
fn head_block_ranges(text: &str) -> Vec<Range<usize>> {
    let mut ranges = Vec::new();
    let mut offset = 0;
    while let Some(end) = head_block_end(&text[offset..]) {
        ranges.push(offset..offset + end);
        offset += end;
    }
    ranges
}

/// One past the bracket closing the block `text` opens with.
fn head_block_end(text: &str) -> Option<usize> {
    let (open, close) = match text
        .as_bytes()
        .first()?
    {
        b'{' => (b'{', b'}'),
        b'<' => (b'<', b'>'),
        b'[' => (b'[', b']'),
        _ => return None,
    };
    let mut depth = 0usize;
    for (index, byte) in text
        .bytes()
        .enumerate()
    {
        if byte == open {
            depth += 1;
        } else if byte == close {
            depth -= 1;
            if depth == 0 {
                return Some(index + 1);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn warned(command: &str) -> Option<String> {
        check(OriginateCheck::Warn, command).report
    }

    #[test]
    fn a_bare_quote_runs_the_rest_of_the_line_into_one_argument() {
        let report = warned("originate {cid_name=O'Brien}sofia/gw/x &park()").unwrap();
        assert!(report.contains("unclosed quote"), "{report}");
    }

    #[test]
    fn two_quotes_in_one_value_are_both_stripped() {
        let report = warned(r"originate {p1=l\'a\'b}sofia/gw/x &park()").unwrap();
        assert!(report.contains(r#"p1="lab""#), "{report}");
    }

    #[test]
    fn an_unescaped_space_truncates_the_dial_string() {
        let report = warned("originate {cid_name=O Brien}sofia/gw/x &park()").unwrap();
        assert!(report.contains("dial string"), "{report}");
    }

    #[test]
    fn an_empty_value_is_reported_though_nothing_is_quoted() {
        let report = warned("originate {p1=,p2=SENTINEL}sofia/gw/x &park()").unwrap();
        assert!(report.contains("p1 has an empty value"), "{report}");
    }

    #[test]
    fn a_bgapi_prefix_is_read_past() {
        assert!(warned(r"bgapi originate {p1=it\'s}sofia/gw/x &park()").is_some());
    }

    #[test]
    fn an_argument_separator_is_honoured() {
        let report = warned(r"originate ^^~{p1=it\'s~a b}loopback/9199/test~&park()");
        assert!(report.is_some());
    }

    #[test]
    fn an_unmodelled_endpoint_module_is_still_read() {
        let report = warned(r"originate {p1=it\'s}freetdm/1/0/5551212 &park()").unwrap();
        assert!(report.contains("p1="), "{report}");
    }

    #[test]
    fn a_dial_string_with_nothing_depth_sensitive_is_quiet() {
        assert_eq!(
            warned("originate {cid_name=Tremblay}sofia/gw/x &park()"),
            None
        );
    }

    #[test]
    fn a_command_that_is_not_an_originate_is_quiet() {
        assert_eq!(warned(r"uuid_setvar abc cid_name O\'Brien"), None);
        assert_eq!(warned("status"), None);
    }

    fn fixed(command: &str) -> Checked {
        check(OriginateCheck::Fix, command)
    }

    #[test]
    fn a_quoted_value_is_rewritten_to_arrive_as_typed() {
        let rewritten = fixed("originate {cid_name=O'Brien}sofia/gw/x &park()")
            .rewritten
            .unwrap();
        let arguments = originate_arguments(&rewritten).unwrap();
        let dial = originate_split(arguments, ' ')
            .unwrap()
            .remove(0);
        let variables = Variables::parse_for(
            &dial[..head_block_end(&dial).unwrap()],
            DialStringTarget::new(DialStringCarrier::EslApi, BLOCK_PARSE),
        )
        .unwrap();
        assert_eq!(variables.get("cid_name"), Some("O'Brien"));
    }

    #[test]
    fn two_quoted_values_in_one_block_both_arrive() {
        let rewritten = fixed("originate {p1=O'Brien,p2=D'Arcy}sofia/gw/x &park()")
            .rewritten
            .unwrap();
        let arguments = originate_arguments(&rewritten).unwrap();
        let dial = originate_split(arguments, ' ')
            .unwrap()
            .remove(0);
        let variables = Variables::parse_for(
            &dial[..head_block_end(&dial).unwrap()],
            DialStringTarget::new(DialStringCarrier::EslApi, BLOCK_PARSE),
        )
        .unwrap();
        assert_eq!(variables.get("p1"), Some("O'Brien"));
        assert_eq!(variables.get("p2"), Some("D'Arcy"));
    }

    #[test]
    fn a_block_carrying_a_backslash_stands_as_written() {
        let checked = fixed(r"originate {p1=it\'s}sofia/gw/x &park()");
        assert_eq!(checked.rewritten, None);
    }

    #[test]
    fn a_block_with_nothing_to_correct_is_left_alone() {
        let checked = fixed("originate {cid_name=Tremblay}sofia/gw/x &park()");
        assert_eq!(checked.rewritten, None);
        assert_eq!(checked.report, None);
    }

    #[test]
    fn the_policy_switches_the_check_off() {
        assert_eq!(
            check(
                OriginateCheck::Off,
                r"originate {p1=it\'s,p2=SENTINEL}sofia/gw/x &park()"
            )
            .report,
            None
        );
    }
}
