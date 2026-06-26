//! Shop Pilot — buy groceries by chat.
//!
//! Layering (see the Inkdrop spec "Shop Pilot — Product & Architecture Spec"):
//!   channel adapter  -> normalizes a channel webhook into a CanonicalMessage
//!   engine           -> consumes CanonicalMessage, emits a CanonicalReply (intent)
//!   channel adapter  -> renders the CanonicalReply into channel-native UI
//!   store adapter    -> drives the user's own store session (Sixty60)
//!
//! The engine never knows which channel it is talking to, and never expresses
//! presentation (no "buttons") — only intent (`Reply::choice`).

pub mod protocol;
pub mod reply;
pub mod sixty60;
pub mod telegram;

// wasm-only: anything that touches the Workers runtime or the LLM agent.
#[cfg(target_arch = "wasm32")]
pub mod ai;
#[cfg(target_arch = "wasm32")]
pub mod session;
#[cfg(target_arch = "wasm32")]
mod worker_app;
