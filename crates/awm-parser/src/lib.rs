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

/// A parsed [`AgentEvent`] plus sub-agent correlation that can't ride on the
/// frozen `AgentEvent` types. `parent_tool_use_id` is the sub-agent tool's
/// (`Agent`/`Task`) `tool_use.id` whose sub-agent produced this event (`None`
/// for the root agent's own output). `spawn` is set only on the `ToolStarted`
/// event of such a call and carries the freshly-spawned sub-agent's tool id +
/// description. Consumers that don't care about sub-agents keep using
/// [`EventSource::next_event`].
#[derive(Clone, Debug, PartialEq)]
pub struct Routed {
    pub event: AgentEvent,
    pub parent_tool_use_id: Option<String>,
    /// The `tool_use.id` of a `ToolStarted` event (`None` otherwise). Lets the
    /// core map a later `can_use_tool` gate (which references this id) back to
    /// the pane that owns the tool.
    pub tool_use_id: Option<String>,
    pub spawn: Option<Spawn>,
}

/// An `Agent`/`Task` tool call that spawns a sub-agent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Spawn {
    /// The `tool_use.id` of the spawning call; later nested messages reference it
    /// via `parent_tool_use_id`.
    pub tool_use_id: String,
    /// The sub-agent's task description (from the `Task` input), for its label.
    pub description: String,
}

/// Incremental parser turning raw stream-json bytes into [`AgentEvent`]s.
#[derive(Default)]
pub struct StreamParser {
    /// Bytes of an as-yet-incomplete trailing line.
    partial: Vec<u8>,
    /// Parsed-but-not-yet-consumed events, each tagged with sub-agent routing.
    ready: VecDeque<Routed>,
    /// `parent_tool_use_id` of the line currently being parsed, stamped onto
    /// every event it emits. Reset per line.
    cur_parent: Option<String>,
}

impl StreamParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Enqueue an event, tagged with the current line's `parent_tool_use_id`
    /// plus optional per-tool routing (`tool_use_id`, sub-agent `spawn`).
    fn emit_routed(
        &mut self,
        event: AgentEvent,
        tool_use_id: Option<String>,
        spawn: Option<Spawn>,
    ) {
        let parent_tool_use_id = self.cur_parent.clone();
        self.ready.push_back(Routed {
            event,
            parent_tool_use_id,
            tool_use_id,
            spawn,
        });
    }

    /// Enqueue a plain event (no per-tool routing).
    fn emit(&mut self, event: AgentEvent) {
        self.emit_routed(event, None, None);
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
        // Each line stands alone; clear any previous line's routing context.
        self.cur_parent = None;

        // Tolerate CRLF and surrounding whitespace.
        let line = trim_ascii(line);
        if line.is_empty() {
            return;
        }

        let value: Value = match serde_json::from_slice(line) {
            Ok(v) => v,
            Err(_) => {
                self.emit(AgentEvent::Noise);
                return;
            }
        };

        // Sub-agent output carries `parent_tool_use_id` (inside `message` for
        // assistant frames; on the envelope for raw stream events). Stamp it on
        // every event this line produces so the core can route to a child pane.
        self.cur_parent = value
            .get("message")
            .and_then(|m| m.get("parent_tool_use_id"))
            .or_else(|| value.get("parent_tool_use_id"))
            .and_then(Value::as_str)
            .map(str::to_string);

        match value.get("type").and_then(Value::as_str) {
            Some("system") => self.parse_system(&value),
            Some("assistant") => self.parse_assistant(&value),
            Some("stream_event") => self.parse_stream_event(&value),
            Some("user") => self.parse_user(&value),
            Some("control_request") => self.parse_control_request(&value),
            Some("control_response") => self.parse_control_response(&value),
            Some("result") => self.parse_result(&value),
            // Unknown / future / missing `type` → Noise.
            _ => self.emit(AgentEvent::Noise),
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
            self.emit(AgentEvent::Info(AgentInfo {
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
                session_id: value
                    .get("session_id")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            }));
            self.emit(AgentEvent::Started { model, cwd });
        } else {
            // Unknown system subtype: treat as noise rather than guess.
            self.emit(AgentEvent::Noise);
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
                        self.emit(AgentEvent::Message { text });
                    }
                    Some("thinking") => {
                        // Track C fills the reasoning text; empty for now.
                        let text = block
                            .get("thinking")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                        self.emit(AgentEvent::Thinking { text });
                    }
                    Some("tool_use") => {
                        let name = block
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                        let summary = summarize_tool_input(&name, block.get("input"));
                        let tid = block.get("id").and_then(Value::as_str).map(str::to_string);
                        // A sub-agent-spawning tool_use (`Agent`/`Task`) carries its
                        // id + description out-of-band so the core can mint a child
                        // pane. Every tool_use also exposes its `tool_use_id` so the
                        // core can later route the tool's approval gate.
                        let spawn = is_subagent_tool(&name, block.get("input"))
                            .then(|| tid.clone())
                            .flatten()
                            .map(|tool_use_id| Spawn {
                                tool_use_id,
                                description: block
                                    .get("input")
                                    .and_then(|i| i.get("description"))
                                    .and_then(Value::as_str)
                                    .filter(|s| !s.is_empty())
                                    .unwrap_or("subagent")
                                    .to_string(),
                            });
                        // Keep the raw input for edit-family tools so the core
                        // can render a diff; other tools don't need it (avoids
                        // cloning large Bash/Read payloads).
                        let input = matches!(
                            name.as_str(),
                            "Edit" | "Write" | "MultiEdit" | "NotebookEdit"
                        )
                        .then(|| block.get("input").cloned())
                        .flatten();
                        self.emit_routed(
                            AgentEvent::ToolStarted { name, summary, input },
                            tid,
                            spawn,
                        );
                    }
                    // Other block kinds (e.g. tool_result echoes) carry no event.
                    _ => {}
                }
            }
        }

        // Token accounting, if the assistant frame reports usage.
        if let Some(usage) = message.and_then(|m| m.get("usage")) {
            self.emit(AgentEvent::Tokens(token_usage(usage)));
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
                    self.emit(AgentEvent::MessageDelta { text });
                    return;
                }
                Some("thinking_delta") => {
                    let text = delta
                        .and_then(|d| d.get("thinking"))
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    self.emit(AgentEvent::Thinking { text });
                    return;
                }
                _ => {}
            }
        }
        self.emit(AgentEvent::Noise);
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
                self.emit(AgentEvent::ToolResult { output, is_error });
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
            self.emit(AgentEvent::Noise);
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
        self.emit(AgentEvent::ApprovalRequested(ctx));
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
            Some(behavior) => self.emit(AgentEvent::ApprovalResolved {
                approved: behavior == "allow",
            }),
            None => self.emit(AgentEvent::Noise),
        }
    }

    fn parse_result(&mut self, value: &Value) {
        if let Some(usage) = value.get("usage") {
            self.emit(AgentEvent::Tokens(token_usage(usage)));
        }
        let ok = !value
            .get("is_error")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        self.emit(AgentEvent::Finished { ok });
    }
}

impl StreamParser {
    /// Like [`EventSource::next_event`], but also yields sub-agent routing info
    /// ([`Routed`]) that can't travel on the frozen `AgentEvent` types.
    pub fn next_routed(&mut self) -> Option<Routed> {
        self.ready.pop_front()
    }
}

impl EventSource for StreamParser {
    fn next_event(&mut self) -> Option<AgentEvent> {
        self.ready.pop_front().map(|r| r.event)
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

/// Whether a `tool_use` spawns a sub-agent. The tool is named `Agent` in current
/// `claude` and `Task` in older builds; as a rename-proof fallback, any tool whose
/// input carries a `subagent_type` field counts (only the sub-agent tool has it).
fn is_subagent_tool(name: &str, input: Option<&Value>) -> bool {
    matches!(name, "Task" | "Agent")
        || input.and_then(|i| i.get("subagent_type")).is_some()
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
        "Task" | "Agent" => field("description"),
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
            input: None,
        }));
        assert!(events.contains(&AgentEvent::ToolResult {
            output: "total 4\nfile".into(),
            is_error: false,
        }));
    }

    #[test]
    fn edit_tool_carries_input_for_diff_rendering() {
        let mut p = StreamParser::new();
        p.feed(br#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Edit","input":{"file_path":"f.rs","old_string":"a","new_string":"b"}}]}}
"#);
        let events: Vec<_> = std::iter::from_fn(|| p.next_event()).collect();
        let input = events
            .iter()
            .find_map(|e| match e {
                AgentEvent::ToolStarted { name, input, .. } if name == "Edit" => Some(input),
                _ => None,
            })
            .expect("Edit ToolStarted present")
            .as_ref()
            .expect("Edit carries its input for diffing");
        assert_eq!(input["old_string"], "a");
        assert_eq!(input["new_string"], "b");
        // The one-line summary is still just the path.
        assert!(events.iter().any(|e| matches!(e,
            AgentEvent::ToolStarted { name, summary, .. } if name == "Edit" && summary == "f.rs")));
    }

    #[test]
    fn init_emits_info_with_metadata_then_started() {
        let mut p = StreamParser::new();
        p.feed(br#"{"type":"system","subtype":"init","cwd":"/home/dev/proj","session_id":"sess-abc","model":"claude-opus-4-8","permissionMode":"acceptEdits","tools":["Bash","Read","Edit"],"skills":["deep-research","dataviz"],"plugins":["p1"],"slash_commands":["/review","/test"],"agents":["Explore","Plan"]}
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
            session_id: Some("sess-abc".into()),
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
            session_id: None,
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
    fn task_spawn_and_nested_message_carry_routing() {
        let bytes: &[u8] = include_bytes!("../../../fixtures/subagents.jsonl");
        let mut p = StreamParser::new();
        p.feed(bytes);
        let routed: Vec<Routed> = std::iter::from_fn(|| p.next_routed()).collect();

        // The Task tool_use spawns a sub-agent, carrying its id + description.
        let spawn = routed
            .iter()
            .find_map(|r| r.spawn.clone())
            .expect("Task should carry a Spawn");
        assert_eq!(spawn.tool_use_id, "toolu_1");
        assert_eq!(spawn.description, "find handlers");
        // The Task call itself is the root's own output (no parent).
        let task = routed
            .iter()
            .find(|r| r.spawn.is_some())
            .unwrap();
        assert_eq!(task.parent_tool_use_id, None);

        // The nested sub-agent message is attributed to the Task's tool id.
        let nested = routed
            .iter()
            .find(|r| r.event == AgentEvent::Message {
                text: "Searching for handler definitions...".into(),
            })
            .expect("nested sub-agent message present");
        assert_eq!(nested.parent_tool_use_id.as_deref(), Some("toolu_1"));

        // The root's own follow-up message has no parent.
        let root_msg = routed
            .iter()
            .find(|r| r.event == AgentEvent::Message {
                text: "The subagent found 4 handlers in src/http/.".into(),
            })
            .expect("root follow-up message present");
        assert_eq!(root_msg.parent_tool_use_id, None);
    }

    #[test]
    fn agent_tool_spawns_and_nested_output_routes() {
        // The real `claude` names the sub-agent tool `Agent` (with a
        // `subagent_type` input) and puts `parent_tool_use_id` at the top level.
        let mut p = StreamParser::new();
        p.feed(br#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_9","name":"Agent","input":{"description":"run parser tests","prompt":"cargo test -p awm-parser","subagent_type":"general-purpose"}}]}}
{"type":"assistant","parent_tool_use_id":"toolu_9","message":{"content":[{"type":"text","text":"Running the parser tests..."}]}}
"#);
        let routed: Vec<Routed> = std::iter::from_fn(|| p.next_routed()).collect();

        let spawn = routed
            .iter()
            .find_map(|r| r.spawn.clone())
            .expect("Agent tool should carry a Spawn");
        assert_eq!(spawn.tool_use_id, "toolu_9");
        assert_eq!(spawn.description, "run parser tests");

        // The summary is the description, not raw JSON.
        assert!(routed.iter().any(|r| r.event
            == AgentEvent::ToolStarted {
                name: "Agent".into(),
                summary: "run parser tests".into(),
                input: None,
            }));

        // Nested sub-agent output (top-level parent_tool_use_id) is attributed.
        let nested = routed
            .iter()
            .find(|r| r.event == AgentEvent::Message {
                text: "Running the parser tests...".into(),
            })
            .expect("nested message present");
        assert_eq!(nested.parent_tool_use_id.as_deref(), Some("toolu_9"));
    }

    #[test]
    fn tool_use_exposes_its_id_for_gate_correlation() {
        let mut p = StreamParser::new();
        p.feed(br#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_bash","name":"Bash","input":{"command":"cargo test"}}]}}
"#);
        let routed: Vec<Routed> = std::iter::from_fn(|| p.next_routed()).collect();
        let ts = routed
            .iter()
            .find(|r| matches!(r.event, AgentEvent::ToolStarted { .. }))
            .expect("ToolStarted present");
        assert_eq!(ts.tool_use_id.as_deref(), Some("toolu_bash"));
        assert!(ts.spawn.is_none());
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
