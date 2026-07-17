//! Track C — Ratatui rendering and keymap. **Stub for Phase 2.**
//!
//! [`AwmTui`] wraps a ratatui [`Terminal`] over any backend, so tests can drive
//! it with `TestBackend` for snapshotting. Render body is `unimplemented!()`
//! until Track C lands.

#![forbid(unsafe_code)]

use awm_proto::{AgentView, LayoutCmd, Renderer};
use ratatui::backend::Backend;
use ratatui::Terminal;

pub mod keymap;

/// The TUI renderer, generic over a ratatui backend.
pub struct AwmTui<B: Backend> {
    #[allow(dead_code)]
    terminal: Terminal<B>,
}

impl<B: Backend> AwmTui<B> {
    /// Build a TUI over `backend` (e.g. `TestBackend` in tests, a crossterm
    /// backend in the real app).
    pub fn new(backend: B) -> std::io::Result<Self> {
        Ok(AwmTui {
            terminal: Terminal::new(backend)?,
        })
    }

    /// Borrow the underlying backend — lets tests snapshot a `TestBackend`.
    pub fn backend(&self) -> &B {
        self.terminal.backend()
    }
}

impl<B: Backend> Renderer for AwmTui<B> {
    fn render(&mut self, _views: &[AgentView], _layout: &LayoutCmd) -> std::io::Result<()> {
        unimplemented!("Track C / Phase 2")
    }
}
