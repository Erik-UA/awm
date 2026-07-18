//! # awm-proto — frozen contracts
//!
//! The single source of truth for `awm`. Every other crate depends on these
//! types and traits and **must not** redefine them. Frozen at the end of
//! Phase 1; changes flow only through the orchestrator thereafter.
//!
//! - [`AgentEvent`] / [`AgentState`] — the lifecycle vocabulary and its state
//!   machine ([`AgentState::apply`]).
//! - [`AgentMeta`] / [`Tags`] / [`AgentId`] — identity and tagging.
//! - [`AgentView`] — the render DTO the TUI consumes.
//! - [`LayoutCmd`] — layout directives from core to TUI.
//! - [`EventSource`] / [`Renderer`] — the layer seams.

#![forbid(unsafe_code)]

pub mod event;
pub mod layout;
pub mod meta;
pub mod state;
pub mod traits;
pub mod view;

pub use event::{AgentEvent, AgentInfo, ApprovalCtx, TokenUsage};
pub use layout::LayoutCmd;
pub use meta::{AgentId, AgentMeta, Tags};
pub use state::AgentState;
pub use traits::{EventSource, Renderer};
pub use view::{AgentView, LineKind, TranscriptLine};
