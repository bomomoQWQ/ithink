//! Mechanism-only harness core. Plugins run in child processes.
//!
//! Native plugins are trusted programs, NOT sandboxed by this library.
//! Agent levels describe work roles, not scheduling or implicit permissions.
//!
//! Layout:
//! - [`kernel`]: mechanism only (identity, tasks, context, events, process runtime).
//! - [`userland`]: replaceable policy built on the public kernel API.

pub mod kernel;
pub mod userland;

// Stable public paths. Module re-exports keep `ithink::api`, `ithink::sdk`,
// `ithink::agent_loop`, ... working after the directory split.
pub use kernel::api::*;
pub use kernel::context::{Context, ContextEntry, ContextSnapshot};
pub use kernel::plugin::{PluginSpec, PluginToken};
pub use kernel::runtime::Runtime;
pub use kernel::{Kernel, api, context, plugin, protocol, runtime, sdk};
#[cfg(feature = "openai")]
pub use userland::openai;
pub use userland::{agent_loop, loop_plugin};
