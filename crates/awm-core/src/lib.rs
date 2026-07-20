//! Integration crate — the agent registry, the layout engine, and the runtime
//! that ties `awm-pty` (agent I/O), `awm-parser` (events), and `awm-proto` (the
//! contract) together. The `awm` binary drives an [`Engine`] and renders its
//! [`Registry`] via `awm-tui`.

#![forbid(unsafe_code)]

pub mod layout;
pub mod registry;
pub mod runtime;
pub mod session;

pub use layout::{plan_layout, LayoutMode};
pub use registry::{AgentRecord, Project, ProjectId, Registry};
pub use runtime::{CoreEvent, Engine};
pub use session::{AgentSnapshot, SessionState};
