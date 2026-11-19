//! The wire protocol between the omp-sessions shell and this engine
//! (decision §4): newline-delimited UTF-8 JSON frames on stdin and stdout.
//! These types are the only definition; `src/engine/protocol.ts` in the
//! package is generated from them (`protocol-ts`).

use serde::{Deserialize, Serialize};

use crate::error::EngineError;
use crate::json::Value;

/// Bumped whenever a frame's shape changes. The shell and the engine come from
/// one commit, so a mismatch means a stale binary.
pub const PROTOCOL: u32 = 1;

/// The omp facts every request carries (decision §9). The engine never derives
/// omp's directory layout, its cwd-to-directory encoding or its cache root.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct Context {
	/// omp's sessions root (`getSessionsDir()`).
	pub sessions_dir: String,
	/// The caller's project session directory (`sessionDirForCwd(cwd)`), the target of `session://here/`.
	pub here_dir: String,
	/// The engine's cache directory under omp's agent cache root.
	pub cache_dir: String,
	/// The caller's working directory.
	pub cwd: String,
	/// The calling omp session; present on query-process requests.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	#[cfg_attr(feature = "ts", ts(optional))]
	pub session_id: Option<String>,
}

/// The parts of an omp `InternalUrl` the shell forwards unparsed.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct UrlParts {
	/// The whole URL as written.
	pub href: String,
	/// `rawHost`, else `hostname`.
	pub host: String,
	/// `rawPathname`, else `pathname`.
	pub pathname: String,
	/// The query string including its leading `?`, or empty.
	pub search: String,
}

/// One request frame.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct Request {
	pub id: u64,
	pub protocol: u32,
	pub context: Context,
	#[serde(flatten)]
	pub op: Op,
}

/// What a request asks for.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(tag = "op", rename_all = "camelCase")]
pub enum Op {
	/// `ProtocolHandler.resolve`: a rendered leaf or a directory listing.
	#[serde(rename_all = "camelCase")]
	Resolve { url: UrlParts, skip_directory_listing: bool },
	/// `ProtocolHandler.readDirectory`.
	ReadDirectory { url: UrlParts },
	/// `ProtocolHandler.locate`: the host file behind a file-backed leaf.
	Locate { url: UrlParts },
	/// `session_query` `agents`: the transcript tree of one session.
	Agents { sid: String },
	/// `session_query` `events`: one transcript's event records, paged.
	Events {
		sid: String,
		#[serde(default, skip_serializing_if = "Option::is_none")]
		#[cfg_attr(feature = "ts", ts(optional))]
		agent: Option<String>,
		#[serde(default, rename = "type", skip_serializing_if = "Option::is_none")]
		#[cfg_attr(feature = "ts", ts(optional))]
		types: Option<String>,
		#[serde(default, skip_serializing_if = "Option::is_none")]
		#[cfg_attr(feature = "ts", ts(optional))]
		offset: Option<u64>,
		#[serde(default, skip_serializing_if = "Option::is_none")]
		#[cfg_attr(feature = "ts", ts(optional))]
		limit: Option<u64>,
	},
}

impl Op {
	/// The op name as written on the wire and in the log line.
	pub fn name(&self) -> &'static str {
		match self {
			Op::Resolve { .. } => "resolve",
			Op::ReadDirectory { .. } => "readDirectory",
			Op::Locate { .. } => "locate",
			Op::Agents { .. } => "agents",
			Op::Events { .. } => "events",
		}
	}
}

/// One entry of a directory listing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct DirEntry {
	pub name: String,
	pub dir: bool,
}

/// A successful result.
#[derive(Clone, Debug, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Reply {
	/// A rendered leaf: its text.
	Resource { content: String },
	/// `resolve` on a directory: its listing as text (empty when the listing was skipped).
	Directory { content: String },
	/// `readDirectory`.
	Entries { entries: Vec<DirEntry> },
	/// `locate`: the host file, or null when the leaf is rendered.
	Located { file: Option<String> },
	/// `agents`.
	Rows {
		#[cfg_attr(feature = "ts", ts(type = "Array<Record<string, unknown>>"))]
		rows: Vec<Value>,
	},
	/// `events`: one page of records.
	Page {
		#[cfg_attr(feature = "ts", ts(type = "Array<Record<string, unknown>>"))]
		rows: Vec<Value>,
		total: u64,
		offset: u64,
		next: Option<u64>,
	},
}

/// A response frame: exactly one per request.
#[derive(Clone, Debug, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(untagged)]
pub enum Response {
	Ok {
		id: u64,
		#[cfg_attr(feature = "ts", ts(type = "true"))]
		ok: bool,
		result: Reply,
	},
	Err {
		id: u64,
		#[cfg_attr(feature = "ts", ts(type = "false"))]
		ok: bool,
		error: EngineError,
	},
}

impl Response {
	pub fn of(id: u64, outcome: crate::error::Result<Reply>) -> Self {
		match outcome {
			Ok(result) => Response::Ok { id, ok: true, result },
			Err(error) => Response::Err { id, ok: false, error },
		}
	}
}
