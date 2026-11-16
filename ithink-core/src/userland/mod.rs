//! Replaceable user-space policy built on the kernel's public API.
//!
//! These modules are bundled with the core package for convenience, but the
//! kernel region must never depend on them.

pub mod agent_loop;
pub mod loop_plugin;

#[cfg(feature = "openai")]
pub mod openai;
