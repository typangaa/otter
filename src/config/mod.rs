//! Configuration model and loading.
//!
//! The TOML file (`weir.toml`) must carry `version = 2`; the schema lives in
//! [`v2`]. [`resolve`] finds the file on disk.

pub mod resolve;
pub mod v2;
