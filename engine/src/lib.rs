//! omp-sessions engine: the only reader of omp session files for the
//! omp-sessions plugin (decision §1). It answers protocol frames on stdin with
//! frames on stdout (src/protocol.rs); stderr carries one log line per request.

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
pub mod render;
pub mod routes;
pub mod scan;
pub mod time;
pub mod window;
