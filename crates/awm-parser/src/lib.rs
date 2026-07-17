//! Track B — stream-json → [`AgentEvent`]. **Stub for Phase 2.**
//!
//! [`StreamParser`] is line-oriented and robust to partial reads and garbage:
//! feed it arbitrary byte chunks, pull normalized events out via
//! [`awm_proto::EventSource`]. Bodies are `unimplemented!()` until Track B lands.

#![forbid(unsafe_code)]

use awm_proto::{AgentEvent, EventSource};
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

    /// Feed a chunk of raw output. Complete lines are parsed into events;
    /// unrecognized or malformed lines become [`AgentEvent::Noise`].
    pub fn feed(&mut self, _bytes: &[u8]) {
        // Silence dead-code warnings on the stub fields without doing work.
        let _ = (&self.partial, &self.ready);
        unimplemented!("Track B / Phase 2")
    }
}

impl EventSource for StreamParser {
    fn next_event(&mut self) -> Option<AgentEvent> {
        unimplemented!("Track B / Phase 2")
    }
}
