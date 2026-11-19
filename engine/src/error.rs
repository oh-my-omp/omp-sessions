//! Engine error kinds (decision §15): what the engine answers instead of a result.

use std::fmt;
use std::path::Path;

use serde::Serialize;

/// The error kinds the engine produces. The shell adds `engine_unavailable` and
/// `engine_exited`, which only it can observe.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
	/// A URL or parameter does not follow the grammar; the message names the field or path segment.
	BadRequest,
	/// No such session, transcript, entry or agent.
	NotFound,
	/// An id prefix matches several sessions; the message lists them.
	Ambiguous,
	/// A statement type is not admitted, or an export path lies inside the sessions directory.
	Refused,
	/// DuckDB reported an error; the message is its text.
	QueryFailed,
	/// A file could not be read; the message carries the path and the system error.
	Io,
	/// The request's `protocol` differs from the engine's.
	ProtocolMismatch,
	/// The request was cancelled.
	Aborted,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct EngineError {
	pub kind: ErrorKind,
	pub message: String,
}

impl EngineError {
	pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
		Self { kind, message: message.into() }
	}

	pub fn bad_request(message: impl Into<String>) -> Self {
		Self::new(ErrorKind::BadRequest, message)
	}

	pub fn not_found(message: impl Into<String>) -> Self {
		Self::new(ErrorKind::NotFound, message)
	}

	/// A failed read of `path`.
	pub fn io(path: &Path, error: &std::io::Error) -> Self {
		Self::new(ErrorKind::Io, format!("{}: {error}", path.display()))
	}
}

impl fmt::Display for EngineError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(f, "{:?}: {}", self.kind, self.message)
	}
}

impl std::error::Error for EngineError {}

pub type Result<T, E = EngineError> = std::result::Result<T, E>;
