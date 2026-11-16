//! Mechanism-only kernel region.
//!
//! Everything here stays independent of model protocols, conversations,
//! network clients, and user-space orchestration.

pub mod api;
pub mod context;
pub mod plugin;
pub mod protocol;
pub mod runtime;
pub mod sdk;

mod process_tree;
mod state;

pub use state::Kernel;
