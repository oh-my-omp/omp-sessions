//! Header-level session metadata from a session file's head and tail windows,
//! by omp's own listing rules (`session-listing.ts` `scanSessionFile`,
//! `deriveSessionStatus`): id, cwd, title, parent, timestamps, turn counts,
//! first prompt and lifecycle status.

use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionStatus {
	Complete,
	Error,
	Aborted,
	Interrupted,
	Pending,
	Unknown,
}

impl SessionStatus {
	pub fn name(self) -> &'static str {
		match self {
			SessionStatus::Complete => "complete",
			SessionStatus::Error => "error",
			SessionStatus::Aborted => "aborted",
			SessionStatus::Interrupted => "interrupted",
			SessionStatus::Pending => "pending",
			SessionStatus::Unknown => "unknown",
		}
	}
}

#[derive(Clone, Debug, PartialEq)]
pub struct SessionInfo {
	pub path: PathBuf,
	pub id: String,
	pub cwd: String,
	pub title: Option<String>,
	/// The header's `parentSession` as written.
	pub parent_session_path: Option<String>,
	pub continuation_of: Option<String>,
	/// The header timestamp; None when it is missing or not a valid time.
	pub created_ms: Option<i64>,
	pub modified_ms: i64,
	pub message_count: u64,
	pub assistant_turns: u64,
	pub size: u64,
	/// The first prompt's text, or `(no messages)`.
	pub first_message: String,
	/// Present when read with status.
	pub status: Option<SessionStatus>,
}

/// The metadata of one session file; None when unreadable or not a session.
pub fn read_session_info(file: &Path, with_status: bool) -> Option<SessionInfo> {
	let _ = (file, with_status);
	todo!("slice: routes")
}
