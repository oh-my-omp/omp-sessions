//! Request dispatch: one protocol op → one reply.

use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

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

/// Answers one request frame (one line of stdin, without its newline).
pub fn answer(frame: &str) -> Answer {
	let invalid = |id: u64, error: EngineError| Answer { op: "invalid", id, response: Response::of(id, Err(error)) };
	let value: serde_json::Value = match serde_json::from_str(frame) {
		Ok(value) => value,
		Err(error) => return invalid(0, EngineError::bad_request(format!("malformed request frame: {error}"))),
	};
	let id = value.get("id").and_then(serde_json::Value::as_u64).unwrap_or(0);
	if let Some(protocol) = value.get("protocol").and_then(serde_json::Value::as_u64)
		&& protocol != u64::from(PROTOCOL)
	{
		return invalid(
			id,
			EngineError::new(ErrorKind::ProtocolMismatch, format!("engine protocol {PROTOCOL}, request protocol {protocol}")),
		);
	}
	match serde_json::from_value::<Request>(value) {
		Ok(request) => Answer { op: request.op.name(), id, response: Response::of(id, handle(&request)) },
		Err(error) => invalid(id, EngineError::bad_request(format!("malformed request frame: {error}"))),
	}
}

pub fn handle(request: &Request) -> Result<Reply> {
	let context = &request.context;
	match &request.op {
		Op::Resolve { url, skip_directory_listing } => handler::resolve(url, *skip_directory_listing, context),
		Op::ReadDirectory { url } => handler::read_directory(url, context),
		Op::Locate { url } => Ok(handler::locate(url, context)),
		Op::Agents { sid } => {
			let session = routes::locate_session(&Locator::Prefix(sid.clone()), context)?;
			Ok(Reply::Rows { rows: transcript_tree(&session)? })
		}
		Op::Events { sid, agent, types, offset, limit } => {
			let session = routes::locate_session(&Locator::Prefix(sid.clone()), context)?;
			let agent = agent.as_deref().unwrap_or("");
			let file = if agent.is_empty() { session.file.clone() } else { routes::agent_file(&session, agent)? };
			let types = match types {
				Some(types) => filter::parse_type_filter(&[types.as_str()])?,
				None => None,
			};
			let (loaded, filter) = filter::load_filtered(&file, &session.sid, agent, types, None, time::now_ms(), FilteredRead::Records)?;
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
fn transcript_url(sid: &str, agent: &str) -> String {
	if agent.is_empty() { format!("session://{sid}") } else { format!("session://{sid}/agents/{agent}") }
}

/// One transcript row: `{sid, agent, parent, file, bytes, modified}` with its size and mtime now.
fn transcript_row(sid: &str, agent: &str, parent: Option<&str>, file: &Path) -> Result<Value> {
	let stat = std::fs::metadata(file).map_err(|error| EngineError::io(file, &error))?;
	let modified = stat
		.modified()
		.ok()
		.and_then(|at| at.duration_since(UNIX_EPOCH).ok())
		.map_or(0, |since| i64::try_from(since.as_millis()).unwrap_or(i64::MAX));
	let mut row = Obj::new();
	row.insert("sid", Value::Str(sid.to_owned()));
	row.insert("agent", Value::Str(agent.to_owned()));
	row.insert("parent", parent.map_or(Value::Null, |parent| Value::Str(parent.to_owned())));
	row.insert("file", Value::Str(file.to_string_lossy().into_owned()));
	row.insert("bytes", Value::Num(stat.len() as f64));
	row.insert("modified", Value::Str(time::iso_utc(modified)));
	Ok(Value::Obj(row))
}

/// The root transcript and every subagent below it, parents before children.
fn transcript_tree(session: &routes::SessionRef) -> Result<Vec<Value>> {
	let mut queue: Vec<(String, PathBuf)> = vec![(String::new(), session.file.clone())];
	let mut rows = vec![transcript_row(&session.sid, "", None, &session.file)?];
	let mut index = 0;
	while index < queue.len() {
		let (parent, file) = queue[index].clone();
		index += 1;
		for agent in routes::child_agents(&file) {
			let child = routes::child_dir(&file).join(format!("{agent}.jsonl"));
			rows.push(transcript_row(&session.sid, &agent, Some(&parent), &child)?);
			queue.push((agent, child));
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
