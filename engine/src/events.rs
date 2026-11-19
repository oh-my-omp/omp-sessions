//! Session JSONL → normalized timeline events.
//!
//! The index pass projects every record with a skeleton policy (ids, roles,
//! tool names, timestamps; texts measured, never captured): it picks the
//! active branch by omp's own leaf replay, numbers its turns, records each
//! branch entry's line span, and completes tool calls from their results and
//! `tool_execution_start` records wherever they sit in the file. The build
//! pass then reads only the branch lines a caller asks for, projected in full
//! (timeline) or light (structure and measures only), into events. Every text
//! has its artifact://, agent://, history:// and local:// references rewritten
//! to the owning session.

use std::path::{Path, PathBuf};

use crate::error::Result;
use crate::json::{Clipped, Value};
use crate::scan::ScanStats;

/// A free text: whole, or clipped at its capture cap.
#[derive(Clone, Debug, PartialEq)]
pub enum Text {
	Plain(String),
	Clipped(Clipped),
}

impl Text {
	/// The text, or the captured prefix of a clipped one.
	pub fn shown(&self) -> &str {
		match self {
			Text::Plain(text) => text,
			Text::Clipped(clipped) => &clipped.text,
		}
	}
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CallStatus {
	Pending,
	Ok,
	Error,
}

impl CallStatus {
	pub fn name(self) -> &'static str {
		match self {
			CallStatus::Pending => "pending",
			CallStatus::Ok => "ok",
			CallStatus::Error => "error",
		}
	}
}

/// A tool call completed from its result and start records.
#[derive(Clone, Debug, PartialEq)]
pub struct Call {
	pub id: String,
	pub tool: String,
	pub args: Value,
	pub intent: Option<String>,
	pub status: CallStatus,
	pub result_entry: Option<String>,
	pub result_chars: Option<u64>,
	pub dur_ms: Option<i64>,
}

/// One child of a dispatch (`task` / `fork_task`).
#[derive(Clone, Debug, PartialEq)]
pub struct ChildTask {
	pub name: String,
	pub agent: String,
	pub task: Text,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExecLang {
	Bash,
	Python,
}

impl ExecLang {
	pub fn name(self) -> &'static str {
		match self {
			ExecLang::Bash => "bash",
			ExecLang::Python => "python",
		}
	}
}

/// What every event carries: its entry, content block, timestamp, physical line and turn.
#[derive(Clone, Debug, PartialEq)]
pub struct EventBase {
	pub entry: String,
	pub block: Option<usize>,
	pub t: String,
	pub line: u64,
	/// The turn its entry belongs to (0: before the first prompt).
	pub turn: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub enum EventKind {
	User { text: Text, injected: bool },
	Developer { text: Text },
	Thinking { text: Text },
	Say { text: Text },
	Call { call: Call },
	IrcOut { to: String, text: Text, call: Call },
	Dispatch { call: Call, context: Option<Text>, children: Vec<ChildTask> },
	IrcIn { from: String, to: String, text: Text },
	Subresult { id: String, agent: String, status: String, duration: String, text: Text },
	Exec { lang: ExecLang, source: Text, status: String },
	Marker { what: String, detail: String },
}

impl EventKind {
	/// The kind name as events.jsonl and the type filter write it.
	pub fn name(&self) -> &'static str {
		match self {
			EventKind::User { .. } => "user",
			EventKind::Developer { .. } => "developer",
			EventKind::Thinking { .. } => "thinking",
			EventKind::Say { .. } => "say",
			EventKind::Call { .. } => "call",
			EventKind::IrcOut { .. } => "irc_out",
			EventKind::Dispatch { .. } => "dispatch",
			EventKind::IrcIn { .. } => "irc_in",
			EventKind::Subresult { .. } => "subresult",
			EventKind::Exec { .. } => "exec",
			EventKind::Marker { .. } => "marker",
		}
	}

	/// The tool call an event carries: plain calls, outgoing IRC and dispatches.
	pub fn call(&self) -> Option<&Call> {
		match self {
			EventKind::Call { call } | EventKind::IrcOut { call, .. } | EventKind::Dispatch { call, .. } => Some(call),
			_ => None,
		}
	}
}

#[derive(Clone, Debug, PartialEq)]
pub struct SessionEvent {
	pub base: EventBase,
	pub kind: EventKind,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SessionMeta {
	/// The root session's full id.
	pub sid: String,
	/// The subagent id, or "" for the root session.
	pub agent: String,
	pub file: PathBuf,
	pub cwd: String,
	pub title: Option<String>,
	pub created: String,
	pub parent_session: Option<String>,
	pub leaf: Option<String>,
	pub entries: usize,
	pub on_branch: usize,
	pub stats: ScanStats,
}

#[derive(Clone, Debug, PartialEq)]
pub struct LoadedSession {
	pub meta: SessionMeta,
	pub events: Vec<SessionEvent>,
}

/// One active-branch entry, as the index pass sees it.
#[derive(Clone, Debug, PartialEq)]
pub struct BranchEntry {
	pub entry: String,
	pub line: u64,
	pub t: String,
	/// The turn its events belong to (0: before the first prompt).
	pub turn: u32,
}

/// How the build pass projects one branch entry: `Full` builds its events as
/// [`load_session`] returns them; `Light` builds the same events with what a
/// filter or a length-based caller reads kept exact (kind, entry, block, t,
/// line, turn, a call's id, tool and completed-call facts, IRC ends, marker
/// names, exec status, prompt attribution, a dispatch's children count, each
/// child's name and agent) and every free text measured only; `Skip` builds none.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Build {
	Full,
	Light,
	Skip,
}

/// The index pass over one session file: metadata, the active branch root
/// first, and what the build pass needs to complete calls and read lines.
#[derive(Debug)]
pub struct SessionIndex {
	pub meta: SessionMeta,
	pub branch: Vec<BranchEntry>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EntrySelector {
	Id(String),
	/// A physical line number, 1-based.
	Line(u64),
}

/// One entry in detail (any branch).
#[derive(Clone, Debug, PartialEq)]
pub struct EntryDetail {
	pub line: u64,
	pub record: Value,
	/// For a tool result: the assistant's toolCall block.
	pub call: Option<Value>,
	/// For a tool result: the `tool_execution_start` record.
	pub start: Option<Value>,
}

/// The index pass over one session file (`agent` "" for the root session,
/// else the subagent id; `sid` the root session's full id). No text is captured.
pub fn index_session(file: &Path, sid: &str, agent: &str) -> Result<SessionIndex> {
	let _ = (file, sid, agent);
	todo!("slice: events")
}

/// The build pass: events of the branch entries `build` asks for, in branch order.
pub fn build_events(index: &SessionIndex, build: &dyn Fn(&BranchEntry) -> Build) -> Result<Vec<SessionEvent>> {
	let _ = (index, build);
	todo!("slice: events")
}

/// Loads one session file as its full active-branch timeline.
pub fn load_session(file: &Path, sid: &str, agent: &str) -> Result<LoadedSession> {
	let index = index_session(file, sid, agent)?;
	let events = build_events(&index, &|_| Build::Full)?;
	Ok(LoadedSession { meta: index.meta, events })
}

/// Finds one entry anywhere in the file and projects it in detail; None when absent.
pub fn load_entry(file: &Path, selector: &EntrySelector) -> Result<Option<EntryDetail>> {
	let _ = (file, selector);
	todo!("slice: events")
}

/// Rewrites references to the owning session's resources into session:// URLs.
pub fn rewrite_refs(text: &str, sid: &str) -> String {
	let _ = (text, sid);
	todo!("slice: events")
}
