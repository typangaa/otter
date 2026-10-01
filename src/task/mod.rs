//! `weir task run|show|list|clean` — one jailed worker run inside a fresh git
//! worktree, followed by jailed checks, a patch file and a ledger record.
//!
//! weir may only ever exec three kinds of program, all hard-coded:
//! the allow-listed worker wrappers (`pi-worker`, `agy-worker`), `agent-jail`
//! (for checks) and `git` (for worktree and diff bookkeeping).

pub mod check;
pub mod cli;
pub mod git;
pub mod id;
pub mod ledger;
pub mod run;
