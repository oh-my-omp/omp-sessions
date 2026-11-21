//! Request dispatch: one protocol op → one reply.

use std::path::PathBuf;
use std::time::UNIX_EPOCH;

use serde::Deserialize;

use crate::cache::Cache;
use crate::error::{EngineError, ErrorKind, Result};
use crate::filter::{self, FilteredRead};
use crate::json::{Obj, Value};
use crate::protocol::{Op, PROTOCOL, Reply, Request, Response};
use crate::routes::{self, Locator};
use crate::{handler, render, time};

/// The page size of an `events` request that names none.
const EVENTS_LIMIT: u64 = 500;

/// One answered frame: the response and what the log line names.
pub struct Answer {
	/// The op name, or `invalid` for a frame that is not a request.
	pub op: &'static str,
	pub id: u64,
	pub response: Response,
}

/// The fields every frame is read for before its shape is known.
#[derive(Deserialize)]
struct Envelope {
	id: Option<u64>,
	protocol: Option<u64>,
	op: Option<String>,
}

/// A frame of stdin read as a request: the request, or the answer to a frame that is not one.
pub enum Parsed {
	Request(Request),
	Invalid(Answer),
}

impl Parsed {
	/// Reads `frame` (one line of stdin, without its newline). The frame is
	/// parsed from its text, never through an intermediate map, so the key
	/// order of caller rows survives.
	pub fn of(frame: &str) -> Parsed {
		let invalid = |id: u64, error: EngineError| Parsed::Invalid(Answer { op: "invalid", id, response: Response::of(id, Err(error)) });
		let envelope: Envelope = match serde_json::from_str(frame) {
			Ok(envelope) => envelope,
			Err(error) => return invalid(0, EngineError::bad_request(format!("malformed request frame: {error}"))),
		};
		let id = envelope.id.unwrap_or(0);
		if let Some(protocol) = envelope.protocol
			&& protocol != u64::from(PROTOCOL)
		{
			return invalid(
				id,
				EngineError::new(ErrorKind::ProtocolMismatch, format!("engine protocol {PROTOCOL}, request protocol {protocol}")),
			);
		}
		match serde_json::from_str::<Request>(frame) {
			Ok(request) => Parsed::Request(request),
			Err(error) => invalid(id, EngineError::bad_request(format!("malformed request frame: {error}"))),
		}
	}

	/// Whether the frame is a query process `cancel` frame.
	pub fn is_cancel(frame: &str) -> bool {
		serde_json::from_str::<Envelope>(frame).is_ok_and(|envelope| envelope.op.as_deref() == Some("cancel"))
	}
}

/// Answers one request frame of the read process.
pub fn answer(frame: &str) -> Answer {
	match Parsed::of(frame) {
		Parsed::Invalid(answer) => answer,
		Parsed::Request(request) => Answer { op: request.op.name(), id: request.id, response: Response::of(request.id, handle(&request)) },
	}
}

/// Answers a read-process op; `sql` and `export` belong to the query process.
pub fn handle(request: &Request) -> Result<Reply> {
	let context = &request.context;
	match &request.op {
		Op::Sql { .. } | Op::Export { .. } => Err(EngineError::bad_request(format!(
			"{} runs in the query process (omp-sessions-engine query), not in a read process",
			request.op.name()
		))),
		Op::Resolve { url, skip_directory_listing } => handler::resolve(url, *skip_directory_listing, context),
		Op::ReadDirectory { url } => handler::read_directory(url, context),
		Op::Locate { url } => Ok(handler::locate(url, context)),
		Op::Agents { sid } => {
			let session = routes::locate_session(&Locator::Prefix(sid.clone()), context)?;
			Ok(Reply::Rows { rows: transcript_tree(&session)?.iter().map(TranscriptRow::to_value).collect() })
		}
		Op::Events { sid, agent, types, offset, limit } => {
			let session = routes::locate_session(&Locator::Prefix(sid.clone()), context)?;
			let agent = agent.as_deref().unwrap_or("");
			let file = if agent.is_empty() { session.file.clone() } else { routes::agent_file(&session, agent)? };
			let types = match types {
				Some(types) => filter::parse_type_filter(&[types.as_str()])?,
				None => None,
			};
			let cache = Cache::of(context)?;
			let (loaded, filter) =
				filter::load_filtered(cache.as_ref(), &file, &session.sid, agent, types, None, time::now_ms(), FilteredRead::Records)?;
			let records = render::event_records(&loaded, &transcript_url(&session.sid, agent), &filter);
			let total = records.len() as u64;
			let offset = offset.unwrap_or(0);
			let end = offset.saturating_add(limit.unwrap_or(EVENTS_LIMIT));
			let rows = records.into_iter().skip(usize::try_from(offset).unwrap_or(usize::MAX)).take(usize::try_from(end - offset).unwrap_or(usize::MAX)).collect();
			Ok(Reply::Page { rows, total, offset, next: (end < total).then_some(end) })
		}
	}
}

/// Canonical session:// URL of a transcript.
pub fn transcript_url(sid: &str, agent: &str) -> String {
	if agent.is_empty() { format!("session://{sid}") } else { format!("session://{sid}/agents/{agent}") }
}

/// One transcript of a session: the root (`agent` empty) or a subagent, with
/// its size and mtime when it was listed.
pub struct TranscriptRow {
	pub sid: String,
	pub agent: String,
	/// The parent transcript's agent id (empty for the root); None for the root itself.
	pub parent: Option<String>,
	pub file: PathBuf,
	pub bytes: u64,
	/// ISO-8601 UTC mtime.
	pub modified: String,
}

impl TranscriptRow {
	fn of(sid: &str, agent: &str, parent: Option<&str>, file: PathBuf) -> Result<Self> {
		let stat = std::fs::metadata(&file).map_err(|error| EngineError::io(&file, &error))?;
		let modified = stat
			.modified()
			.ok()
			.and_then(|at| at.duration_since(UNIX_EPOCH).ok())
			.map_or(0, |since| i64::try_from(since.as_millis()).unwrap_or(i64::MAX));
		Ok(Self {
			sid: sid.to_owned(),
			agent: agent.to_owned(),
			parent: parent.map(str::to_owned),
			file,
			bytes: stat.len(),
			modified: time::iso_utc(modified),
		})
	}

	/// The `agents` row `{sid, agent, parent, file, bytes, modified}`.
	pub fn to_value(&self) -> Value {
		let mut row = Obj::new();
		row.insert("sid", Value::Str(self.sid.clone()));
		row.insert("agent", Value::Str(self.agent.clone()));
		row.insert("parent", self.parent.as_ref().map_or(Value::Null, |parent| Value::Str(parent.clone())));
		row.insert("file", Value::Str(self.file.to_string_lossy().into_owned()));
		row.insert("bytes", Value::Num(self.bytes as f64));
		row.insert("modified", Value::Str(self.modified.clone()));
		Value::Obj(row)
	}
}

/// The root transcript and every subagent below it, parents before children.
pub fn transcript_tree(session: &routes::SessionRef) -> Result<Vec<TranscriptRow>> {
	let mut rows = vec![TranscriptRow::of(&session.sid, "", None, session.file.clone())?];
	let mut index = 0;
	while index < rows.len() {
		let (parent, file) = (rows[index].agent.clone(), rows[index].file.clone());
		index += 1;
		for agent in routes::child_agents(&file) {
			let child = routes::child_dir(&file).join(format!("{agent}.jsonl"));
			rows.push(TranscriptRow::of(&session.sid, &agent, Some(&parent), child)?);
		}
	}
	Ok(rows)
}

#[cfg(test)]
mod tests {
	use super::*;

	fn error_of(frame: &str) -> (u64, ErrorKind, String) {
		let answer = answer(frame);
		match answer.response {
			Response::Err { id, error, .. } => (id, error.kind, error.message),
			Response::Ok { .. } => panic!("{frame} answered ok"),
		}
	}

	const CONTEXT: &str = r#""context":{"sessionsDir":"/nonexistent","hereDir":"/nonexistent/-w","cacheDir":"","cwd":"/"}"#;

	#[test]
	fn frame_errors() {
		let (id, kind, _) = error_of("{not json");
		assert_eq!((id, kind), (0, ErrorKind::BadRequest));
		let (id, kind, message) = error_of(&format!(r#"{{"id":7,"protocol":9,"op":"agents","sid":"x",{CONTEXT}}}"#));
		assert_eq!((id, kind, message.as_str()), (7, ErrorKind::ProtocolMismatch, "engine protocol 1, request protocol 9"));
		let (id, kind, message) = error_of(&format!(r#"{{"id":8,"protocol":1,"op":"sql",{CONTEXT}}}"#));
		assert_eq!((id, kind), (8, ErrorKind::BadRequest));
		assert!(message.starts_with("malformed request frame: "), "{message}");
		let (id, kind, _) = error_of(&format!(r#"{{"id":9,"protocol":1,"op":"agents","sid":"abc",{CONTEXT}}}"#));
		assert_eq!((id, kind), (9, ErrorKind::NotFound));
	}
}
