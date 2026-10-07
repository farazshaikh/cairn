//! Catalog, query planner and executor for cairn.
//!
//! [`Database`] opens a cairn file and runs SQL. `cairn-sql` parses the
//! text; the binder (`bind.rs`) resolves names and checks types statically;
//! the planner (`plan.rs`, `select.rs`) picks a primary-key, index or
//! full-scan access path; and the executor (`plan.rs`, `dml.rs`, `ddl.rs`)
//! reads and writes `cairn-storage` B-trees. Every write statement
//! validates its whole effect before the first write, so a failing
//! statement changes nothing.
//!
//! On-disk formats are documented where they are encoded:
//! `codec/record.rs` (row records, version 1), `codec/key.rs` (row and
//! index keys) and `catalog.rs` (catalog entries, version 1). The README
//! "Querying" section describes the SQL semantics.

#![cfg_attr(
    not(test),
    deny(
        clippy::indexing_slicing,
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic
    )
)]

mod bind;
mod catalog;
mod codec;
mod database;
mod ddl;
mod dml;
mod error;
mod eval;
mod explain;
mod functions;
mod like;
mod plan;
mod select;
mod table;
mod value;

pub use database::{Database, QueryResult};
pub use error::{ErrorKind, ExecError};
pub use value::Value;
