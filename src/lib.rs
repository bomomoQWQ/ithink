//! Mechanism-only harness kernel. Plugins run in child processes.
//!
//! Native plugins are trusted programs, NOT sandboxed by this library.
//! Agent levels describe work roles, not scheduling or implicit permissions.

pub mod agent_loop;
pub mod api;
pub mod context;
pub mod kernel;
pub mod loop_plugin;
#[cfg(feature = "openai")]
pub mod openai;
pub mod plugin;
mod process_tree;
pub mod protocol;
pub mod runtime;
pub mod sdk;

pub use api::*;
pub use context::{Context, ContextEntry, ContextSnapshot};
pub use kernel::Kernel;
pub use plugin::{PluginSpec, PluginToken};
pub use runtime::Runtime;
