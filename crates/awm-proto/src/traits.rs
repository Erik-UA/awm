//! The two seams between layers. Both are object-safe so the core can hold them
//! as `Box<dyn _>` and swap real implementations for fixtures/mocks in tests.

use crate::event::AgentEvent;
use crate::layout::LayoutCmd;
use crate::view::AgentView;

/// A source of normalized agent events (implemented by the parser over a PTY,
/// or by a fixture replayer in tests).
///
/// Pull-based and synchronous by design: proto stays free of any async runtime.
/// Implementations that read async I/O buffer internally and surface ready
/// events here. `None` means "no event available right now"; end-of-stream is
/// signalled in-band by [`AgentEvent::Finished`], not by `None`.
pub trait EventSource {
    /// Return the next ready event, or `None` if none is currently available.
    fn next_event(&mut self) -> Option<AgentEvent>;
}

/// A sink that draws the current world. Implemented by the TUI (Track C).
///
/// The renderer is a pure function of its inputs — it owns no layout policy.
pub trait Renderer {
    /// Draw `views` arranged per `layout`.
    fn render(&mut self, views: &[AgentView], layout: &LayoutCmd) -> std::io::Result<()>;
}
