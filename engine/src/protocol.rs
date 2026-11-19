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

/// The sessions whose relations fill schema `src` for one `sql` or `export` call.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct Selection {
	/// Session ids or unique id prefixes.
	pub sids: Vec<String>,
	/// Whether the subagent transcripts are read too; omitted: true.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	#[cfg_attr(feature = "ts", ts(optional))]
	pub agents: Option<bool>,
}

/// The file format `export` writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(rename_all = "lowercase")]
pub enum ExportFormat {
	Parquet,
	Csv,
	Json,
}

impl ExportFormat {
	/// The name DuckDB's `COPY … (FORMAT …)` takes.
	pub fn name(self) -> &'static str {
		match self {
			ExportFormat::Parquet => "parquet",
			ExportFormat::Csv => "csv",
			ExportFormat::Json => "json",
		}
	}
}

/// The `cancel` frame of the query process: interrupts request `id`, which
/// then answers `aborted`. It has no response of its own.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct Cancel {
	pub op: CancelOp,
	pub id: u64,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub enum CancelOp {
	#[serde(rename = "cancel")]
	Cancel,
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
	/// `session_query` `sql` (query process): runs `query` with this call's `src`.
	Sql {
		query: String,
		#[serde(default, skip_serializing_if = "Option::is_none")]
		#[cfg_attr(feature = "ts", ts(optional))]
		from: Option<Selection>,
		/// Caller rows that become `src.<name>`.
		#[serde(default, skip_serializing_if = "Option::is_none")]
		#[cfg_attr(feature = "ts", ts(optional, type = "Record<string, Array<Record<string, unknown>>>"))]
		tables: Option<Value>,
		/// Bindings of the only statement: positional or by name.
		#[serde(default, skip_serializing_if = "Option::is_none")]
		#[cfg_attr(
			feature = "ts",
			ts(optional, type = "Array<string | number | boolean | null> | Record<string, string | number | boolean | null>")
		)]
		params: Option<Value>,
		/// Most rows returned; default 1000.
		#[serde(default, skip_serializing_if = "Option::is_none")]
		#[cfg_attr(feature = "ts", ts(optional))]
		limit: Option<u64>,
	},
	/// `session_query` `export` (query process): writes `query`'s whole result to `path`.
	Export {
		query: String,
		path: String,
		format: ExportFormat,
		#[serde(default, skip_serializing_if = "Option::is_none")]
		#[cfg_attr(feature = "ts", ts(optional))]
		from: Option<Selection>,
		#[serde(default, skip_serializing_if = "Option::is_none")]
		#[cfg_attr(feature = "ts", ts(optional, type = "Record<string, Array<Record<string, unknown>>>"))]
		tables: Option<Value>,
		#[serde(default, skip_serializing_if = "Option::is_none")]
		#[cfg_attr(
			feature = "ts",
			ts(optional, type = "Array<string | number | boolean | null> | Record<string, string | number | boolean | null>")
		)]
		params: Option<Value>,
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
			Op::Sql { .. } => "sql",
			Op::Export { .. } => "export",
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
	/// `sql` whose last statement returns rows: at most `limit` of them, within
	/// the result's character budget; `truncated` when a row was left out.
	SqlRows {
		columns: Vec<String>,
		#[cfg_attr(feature = "ts", ts(type = "Array<Record<string, unknown>>"))]
		rows: Vec<Value>,
		truncated: bool,
	},
	/// `sql` whose last statement returns no rows: the rows it changed.
	Changes { changes: u64 },
	/// `export`: the file written and what it holds.
	Exported { path: String, format: ExportFormat, rows: u64, columns: Vec<String> },
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
