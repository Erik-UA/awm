//! Track B — stream-json → [`AgentEvent`].
//!
//! [`StreamParser`] is line-oriented and robust to partial reads and garbage:
//! feed it arbitrary byte chunks, pull normalized events out via
//! [`awm_proto::EventSource`]. Complete newline-delimited JSON lines are mapped
//! per `fixtures/README.md`; unparseable or unknown lines become
//! [`AgentEvent::Noise`] rather than panicking.
//!
//! The mapping this crate implements (from `fixtures/README.md`):
//!
//! | stream-json line                              | AgentEvent(s)                       |
//! |-----------------------------------------------|-------------------------------------|
//! | `type=system, subtype=init`                   | `Started{model, cwd}`               |
//! | `type=assistant` block `text`/`thinking`      | `Thinking`                          |
//! | `type=assistant` block `tool_use`             | `ToolStarted{name}`                 |
//! | `type=assistant` `message.usage` present      | `Tokens{input, output}`             |
//! | `type=user` `tool_result`                     | *(no event)*                        |
//! | `type=control_request, subtype=can_use_tool`  | `ApprovalRequested{..}`             |
//! | `type=control_response`                       | `ApprovalResolved{approved}`        |
//! | `type=result`                                 | `Tokens{final}` then `Finished{ok}` |
//! | unparseable / unknown                         | `Noise`                             |

#![forbid(unsafe_code)]

use awm_proto::{AgentEvent, ApprovalCtx, EventSource, TokenUsage};
use serde_json::Value;
use std::collections::VecDeque;

/// Incremental parser turning raw stream-json bytes into [`AgentEvent`]s.
#[derive(Default)]
pub struct StreamParser {
    /// Bytes of an as-yet-incomplete trailing line.
    partial: Vec<u8>,
    /// Parsed-but-not-yet-consumed events.
    ready: VecDeque<AgentEvent>,
}

impl StreamParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed a chunk of raw output. Complete (newline-terminated) lines are
    /// parsed into events; any partial trailing line is buffered until the rest
    /// of it arrives in a later chunk. Unrecognized or malformed lines become
    /// [`AgentEvent::Noise`]. Never panics on arbitrary input.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.partial.extend_from_slice(bytes);
        while let Some(nl) = self.partial.iter().position(|&b| b == b'\n') {
            // Take the line (without its trailing '\n') out of the buffer.
            let line: Vec<u8> = self.partial.drain(..=nl).collect();
            self.parse_line(&line[..nl]);
        }
    }

    /// Parse a single complete line (newline already stripped) and enqueue the
    /// resulting event(s). Blank/whitespace-only lines are ignored.
    fn parse_line(&mut self, line: &[u8]) {
        // Tolerate CRLF and surrounding whitespace.
        let line = trim_ascii(line);
        if line.is_empty() {
            return;
        }

        let value: Value = match serde_json::from_slice(line) {
            Ok(v) => v,
            Err(_) => {
                self.ready.push_back(AgentEvent::Noise);
                return;
            }
        };

        match value.get("type").and_then(Value::as_str) {
            Some("system") => self.parse_system(&value),
            Some("assistant") => self.parse_assistant(&value),
            // Tool results / other user turns are PTY-buffer output, not events.
            Some("user") => {}
            Some("control_request") => self.parse_control_request(&value),
            Some("control_response") => self.parse_control_response(&value),
            Some("result") => self.parse_result(&value),
            // Unknown / future / missing `type` → Noise.
            _ => self.ready.push_back(AgentEvent::Noise),
        }
    }

    fn parse_system(&mut self, value: &Value) {
        if value.get("subtype").and_then(Value::as_str) == Some("init") {
            let model = value
                .get("model")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let cwd = value
                .get("cwd")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .into();
            self.ready.push_back(AgentEvent::Started { model, cwd });
        } else {
            // Unknown system subtype: treat as noise rather than guess.
            self.ready.push_back(AgentEvent::Noise);
        }
    }

    fn parse_assistant(&mut self, value: &Value) {
        let message = value.get("message");

        // One event per recognized content block, in order.
        if let Some(content) = message
            .and_then(|m| m.get("content"))
            .and_then(Value::as_array)
        {
            for block in content {
                match block.get("type").and_then(Value::as_str) {
                    Some("text") | Some("thinking") => {
                        self.ready.push_back(AgentEvent::Thinking);
                    }
                    Some("tool_use") => {
                        let name = block
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                        self.ready.push_back(AgentEvent::ToolStarted { name });
                    }
                    // Other block kinds (e.g. tool_result echoes) carry no event.
                    _ => {}
                }
            }
        }

        // Token accounting, if the assistant frame reports usage.
        if let Some(usage) = message.and_then(|m| m.get("usage")) {
            self.ready
                .push_back(AgentEvent::Tokens(token_usage(usage)));
        }
    }

    fn parse_control_request(&mut self, value: &Value) {
        let request = value.get("request");
        let is_can_use_tool = request
            .and_then(|r| r.get("subtype"))
            .and_then(Value::as_str)
            == Some("can_use_tool");

        if !is_can_use_tool {
            self.ready.push_back(AgentEvent::Noise);
            return;
        }
        let request = request.unwrap();

        let ctx = ApprovalCtx {
            tool: request
                .get("tool_name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            input: request.get("input").cloned().unwrap_or(Value::Null),
            // Correlation id lives on the envelope, not inside `request`.
            request_id: value
                .get("request_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            tool_use_id: request
                .get("tool_use_id")
                .and_then(Value::as_str)
                .map(str::to_string),
            description: request
                .get("description")
                .and_then(Value::as_str)
                .map(str::to_string),
            decision_reason: request
                .get("decision_reason")
                .and_then(Value::as_str)
                .map(str::to_string),
            diff: None,
        };
        self.ready.push_back(AgentEvent::ApprovalRequested(ctx));
    }

    fn parse_control_response(&mut self, value: &Value) {
        // Shape: response.response.behavior == "allow" | "deny".
        let behavior = value
            .get("response")
            .and_then(|r| r.get("response"))
            .and_then(|r| r.get("behavior"))
            .and_then(Value::as_str);
        let approved = behavior == Some("allow");
        self.ready
            .push_back(AgentEvent::ApprovalResolved { approved });
    }

    fn parse_result(&mut self, value: &Value) {
        if let Some(usage) = value.get("usage") {
            self.ready
                .push_back(AgentEvent::Tokens(token_usage(usage)));
        }
        let ok = !value
            .get("is_error")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        self.ready.push_back(AgentEvent::Finished { ok });
    }
}

impl EventSource for StreamParser {
    fn next_event(&mut self) -> Option<AgentEvent> {
        self.ready.pop_front()
    }
}

/// Extract `{input_tokens, output_tokens}` from a `usage` object, defaulting
/// missing fields to zero.
fn token_usage(usage: &Value) -> TokenUsage {
    let field = |k: &str| usage.get(k).and_then(Value::as_u64).unwrap_or(0);
    TokenUsage {
        input: field("input_tokens"),
        output: field("output_tokens"),
    }
}

/// Trim leading/trailing ASCII whitespace (incl. `\r`) from a byte slice.
/// Equivalent to the unstable `[u8]::trim_ascii`, inlined for MSRV 1.75.
fn trim_ascii(mut bytes: &[u8]) -> &[u8] {
    while let [first, rest @ ..] = bytes {
        if first.is_ascii_whitespace() {
            bytes = rest;
        } else {
            break;
        }
    }
    while let [rest @ .., last] = bytes {
        if last.is_ascii_whitespace() {
            bytes = rest;
        } else {
            break;
        }
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feeding a fixture in one shot and feeding it split at arbitrary byte
    /// offsets must yield the exact same event sequence — proving the parser is
    /// robust across chunk boundaries (partial trailing lines are buffered).
    #[test]
    fn chunk_boundaries_are_transparent() {
        let bytes: &[u8] = include_bytes!("../../../fixtures/approval.jsonl");

        // Baseline: feed the whole thing at once.
        let whole = drain(feed_all(bytes, &[bytes.len()]));

        // A spread of awkward split points, including inside JSON tokens and
        // mid-multibyte-adjacent positions.
        for splits in [
            vec![1],
            vec![7, 40, 200],
            vec![1, 2, 3, 5, 8, 13, 21, 34, 55, 89, 144],
            vec![bytes.len() / 3, 2 * bytes.len() / 3],
            (0..bytes.len()).step_by(3).collect::<Vec<_>>(),
            (0..bytes.len()).step_by(1).collect::<Vec<_>>(),
        ] {
            let chunked = drain(feed_all(bytes, &splits));
            assert_eq!(
                chunked, whole,
                "event sequence diverged for split points {splits:?}"
            );
        }
    }

    /// Feed `bytes` to a fresh parser, breaking it at the given cut offsets.
    fn feed_all(bytes: &[u8], cuts: &[usize]) -> StreamParser {
        let mut parser = StreamParser::new();
        let mut start = 0;
        for &cut in cuts {
            let end = cut.min(bytes.len());
            if end > start {
                parser.feed(&bytes[start..end]);
                start = end;
            }
        }
        if start < bytes.len() {
            parser.feed(&bytes[start..]);
        }
        parser
    }

    fn drain(mut parser: StreamParser) -> Vec<AgentEvent> {
        let mut out = Vec::new();
        while let Some(ev) = parser.next_event() {
            out.push(ev);
        }
        out
    }

    #[test]
    fn garbage_never_panics_and_maps_to_noise() {
        let mut parser = StreamParser::new();
        parser.feed(b"not json at all\n");
        parser.feed(b"{\"type\":\"mystery_event\"}\n");
        parser.feed(b"{ truncated \n");
        assert_eq!(parser.next_event(), Some(AgentEvent::Noise));
        assert_eq!(parser.next_event(), Some(AgentEvent::Noise));
        assert_eq!(parser.next_event(), Some(AgentEvent::Noise));
        assert_eq!(parser.next_event(), None);
    }
}
