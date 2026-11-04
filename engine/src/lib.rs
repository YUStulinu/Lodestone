//! Lodestone: a SQL database written from scratch.
//!
//! * [`storage`]: pages, a write-ahead log with crash recovery, and a B+tree.
//! * [`mvcc`]: snapshot-isolated transactions over the tree.
//! * [`sql`]: parser, planner and optimiser, executor.

#![allow(clippy::type_complexity)]

pub mod keycode;
pub mod mvcc;
pub mod slt;
pub mod sql;
pub mod storage;

pub use sql::{Database, Output, Session, Value};
