//! CLI layer — subcommand runner modules.
//!
//! Each submodule exposes plain functions; the main [`clap`] dispatch
//! (in `src/main.rs`) calls them directly.  All public functions follow
//! these conventions:
//!
//! * Accept a `path: &std::path::Path` pointing at `weir.toml`.
//! * Return `crate::error::Result<()>` (or `()` when infallible).
//! * Print machine-readable JSON to **stdout** when `json: bool` is `true`.
//! * Print human-readable text to **stdout** otherwise.
//! * Print error messages to **stderr** via [`eprintln!`].
//! * Exit codes are set by the caller (`main.rs`): 0 = success, 1 = user
//!   error, 2 = system / unexpected error.

pub mod backend;
pub mod deep;
pub mod status;
pub mod validate;
pub mod workflow;
