//! Worker spawn core: the low-level process launcher shared by the brief-2
//! CLI commands. Kept separate from `lease.rs` (a resource gate) — this module
//! only spawns a wrapper, drains its pipes with caps, and enforces deadlines.

pub mod cli;
pub mod spawn;
