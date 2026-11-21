//! omp-sessions engine: the only reader of omp session files for the
//! omp-sessions plugin (decision §1). It answers protocol frames on stdin with
//! frames on stdout (src/protocol.rs); stderr carries one log line per request.
//! A read process answers `session://` and row requests; the query process
//! (`query`) answers `sql` and `export` on its DuckDB database.

pub mod duck;
pub mod error;
pub mod events;
pub mod filter;
pub mod handler;
pub mod info;
pub mod jq;
pub mod json;
pub mod metrics;
pub mod ops;
pub mod protocol;
pub mod query;
pub mod render;
pub mod routes;
pub mod scan;
pub mod time;
pub mod window;
