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
//! | `type=assistant` block `text`                 | `Message{text}`                     |
//! | `type=assistant` block `thinking`             | `Thinking`                          |
//! | `type=assistant` block `tool_use`             | `ToolStarted{name, summary}`        |
//! | `type=assistant` `message.usage` present      | `Tokens{input, output}`             |
//! | `type=user` `tool_result`                     | `ToolResult{output, is_error}`      |
//! | `type=control_request, subtype=can_use_tool`  | `ApprovalRequested{..}`             |
//! | `type=control_response`                       | `ApprovalResolved{approved}`        |
//! | `type=result`                                 | `Tokens{final}` then `Finished{ok}` |
//! | unparseable / unknown                         | `Noise`                             |

#![forbid(unsafe_code)]

use awm_proto::{AgentEvent, AgentInfo, ApprovalCtx, EventSource, TokenUsage};
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
            Some("stream_event") => self.parse_stream_event(&value),
            Some("user") => self.parse_user(&value),
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
            // Full session metadata for the inspection card. `Info` maps to no
            // state transition, so emitting it alongside `Started` leaves the
            // collapsed state sequence unchanged.
            self.ready.push_back(AgentEvent::Info(AgentInfo {
                model: model.clone(),
                permission_mode: value
                    .get("permissionMode")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                tools: string_array(value, "tools"),
                skills: string_array(value, "skills"),
                plugins: string_array(value, "plugins"),
                slash_commands: string_array(value, "slash_commands"),
                agents: string_array(value, "agents"),
            }));
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
                    // Assistant text is shown in the agent's window; internal
                    // reasoning (`thinking`) is just a "working" signal.
                    Some("text") => {
                        let text = block
                            .get("text")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                        self.ready.push_back(AgentEvent::Message { text });
                    }
                    Some("thinking") => {
                        // Track C fills the reasoning text; empty for now.
                        let text = block
                            .get("thinking")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                        self.ready.push_back(AgentEvent::Thinking { text });
                    }
                    Some("tool_use") => {
                        let name = block
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                        let summary = summarize_tool_input(&name, block.get("input"));
                        self.ready
                            .push_back(AgentEvent::ToolStarted { name, summary });
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

    /// A `stream_event` frame wraps a raw Anthropic streaming event
    /// (`--include-partial-messages`). We surface text deltas as `MessageDelta`
    /// and thinking (reasoning) deltas as `Thinking`; other stream events (block
    /// start/stop, message deltas) carry no line.
    fn parse_stream_event(&mut self, value: &Value) {
        let inner = value.get("event");
        let inner_type = inner.and_then(|e| e.get("type")).and_then(Value::as_str);
        if inner_type == Some("content_block_delta") {
            let delta = inner.and_then(|e| e.get("delta"));
            match delta.and_then(|d| d.get("type")).and_then(Value::as_str) {
                Some("text_delta") => {
                    let text = delta
                        .and_then(|d| d.get("text"))
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    self.ready.push_back(AgentEvent::MessageDelta { text });
                    return;
                }
                Some("thinking_delta") => {
                    let text = delta
                        .and_then(|d| d.get("thinking"))
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    self.ready.push_back(AgentEvent::Thinking { text });
                    return;
                }
                _ => {}
            }
        }
        self.ready.push_back(AgentEvent::Noise);
    }

    /// A `user` frame carries tool results (the output of the agent's tools).
    fn parse_user(&mut self, value: &Value) {
        let Some(blocks) = value
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(Value::as_array)
        else {
            return;
        };
        for block in blocks {
            if block.get("type").and_then(Value::as_str) == Some("tool_result") {
                let is_error = block
                    .get("is_error")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let output = tool_result_text(block.get("content"));
                self.ready
                    .push_back(AgentEvent::ToolResult { output, is_error });
            }
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
        // Shape: response.response.behavior == "allow" | "deny". Control responses
        // to non-permission requests (e.g. the `initialize` handshake reply) carry
        // no `behavior` — those are not approval resolutions, so map them to Noise.
        match value
            .get("response")
            .and_then(|r| r.get("response"))
            .and_then(|r| r.get("behavior"))
            .and_then(Value::as_str)
        {
            Some(behavior) => self.ready.push_back(AgentEvent::ApprovalResolved {
                approved: behavior == "allow",
            }),
            None => self.ready.push_back(AgentEvent::Noise),
        }
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

/// Read a JSON array-of-strings field, dropping non-string entries. A missing
/// field (or a non-array value) yields an empty vec.
fn string_array(value: &Value, key: &str) -> Vec<String> {
    value
        .get(key)
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// A one-line preview of a tool's input, like Claude Code's `Tool(summary)`.
/// Picks the most meaningful field per tool, falling back to compact JSON.
fn summarize_tool_input(name: &str, input: Option<&Value>) -> String {
    let Some(input) = input else {
        return String::new();
    };
    let field = |k: &str| input.get(k).and_then(Value::as_str).unwrap_or("");
    let pick = match name {
        "Bash" => field("command"),
        "Read" | "Edit" | "Write" | "NotebookEdit" => field("file_path"),
        "Grep" | "Glob" => field("pattern"),
        "Task" => field("description"),
        "WebFetch" => field("url"),
        "WebSearch" => field("query"),
        _ => "",
    };
    if !pick.is_empty() {
        return one_line(pick, 120);
    }
    one_line(&input.to_string(), 120)
}

/// Flatten a tool_result `content` (a string, or an array of `{type,text}`
/// blocks) into plain text.
fn tool_result_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Collapse newlines and truncate to `max` chars (char-safe).
fn one_line(s: &str, max: usize) -> String {
    let flat = s.replace('\n', " ");
    if flat.chars().count() > max {
        let head: String = flat.chars().take(max).collect();
        format!("{head}…")
    } else {
        flat
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

    #[test]
    fn extracts_tool_summary_and_result() {
        let mut p = StreamParser::new();
        p.feed(br#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"ls -la"}}]}}
{"type":"user","message":{"content":[{"type":"tool_result","content":"total 4\nfile","is_error":false}]}}
"#);
        let events: Vec<_> = std::iter::from_fn(|| p.next_event()).collect();
        assert!(events.contains(&AgentEvent::ToolStarted {
            name: "Bash".into(),
            summary: "ls -la".into(),
        }));
        assert!(events.contains(&AgentEvent::ToolResult {
            output: "total 4\nfile".into(),
            is_error: false,
        }));
    }

    #[test]
    fn init_emits_info_with_metadata_then_started() {
        let mut p = StreamParser::new();
        p.feed(br#"{"type":"system","subtype":"init","cwd":"/home/dev/proj","model":"claude-opus-4-8","permissionMode":"acceptEdits","tools":["Bash","Read","Edit"],"skills":["deep-research","dataviz"],"plugins":["p1"],"slash_commands":["/review","/test"],"agents":["Explore","Plan"]}
"#);
        let events: Vec<_> = std::iter::from_fn(|| p.next_event()).collect();
        assert!(events.contains(&AgentEvent::Info(AgentInfo {
            model: "claude-opus-4-8".into(),
            permission_mode: "acceptEdits".into(),
            tools: vec!["Bash".into(), "Read".into(), "Edit".into()],
            skills: vec!["deep-research".into(), "dataviz".into()],
            plugins: vec!["p1".into()],
            slash_commands: vec!["/review".into(), "/test".into()],
            agents: vec!["Explore".into(), "Plan".into()],
        })));
        // Started is still emitted (Info does not replace it).
        assert!(events.contains(&AgentEvent::Started {
            model: "claude-opus-4-8".into(),
            cwd: "/home/dev/proj".into(),
        }));
    }

    #[test]
    fn init_missing_arrays_yield_empty_info() {
        let mut p = StreamParser::new();
        p.feed(br#"{"type":"system","subtype":"init","cwd":"/p","model":"m"}
"#);
        let events: Vec<_> = std::iter::from_fn(|| p.next_event()).collect();
        assert!(events.contains(&AgentEvent::Info(AgentInfo {
            model: "m".into(),
            permission_mode: String::new(),
            tools: vec![],
            skills: vec![],
            plugins: vec![],
            slash_commands: vec![],
            agents: vec![],
        })));
    }

    #[test]
    fn stream_event_text_delta_becomes_message_delta() {
        let mut p = StreamParser::new();
        p.feed(br#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hel"}}}
{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}}
"#);
        let events: Vec<_> = std::iter::from_fn(|| p.next_event()).collect();
        assert!(events.contains(&AgentEvent::MessageDelta { text: "Hel".into() }));
        // Non-text stream events are inert Noise.
        assert!(events.contains(&AgentEvent::Noise));
    }

    #[test]
    fn stream_event_thinking_delta_becomes_thinking() {
        let mut p = StreamParser::new();
        p.feed(br#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"Let me"}}}
"#);
        let events: Vec<_> = std::iter::from_fn(|| p.next_event()).collect();
        assert!(events.contains(&AgentEvent::Thinking {
            text: "Let me".into()
        }));
        // A thinking delta is not Noise (it carries the reasoning text).
        assert!(!events.contains(&AgentEvent::Noise));
    }

    #[test]
    fn tool_result_array_content_is_flattened() {
        assert_eq!(
            tool_result_text(Some(&serde_json::json!([
                {"type":"text","text":"a"},
                {"type":"text","text":"b"}
            ]))),
            "a\nb"
        );
    }

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
