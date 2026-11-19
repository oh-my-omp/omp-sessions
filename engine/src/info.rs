//! Header-level session metadata from a session file's head and tail windows,
//! by omp's own listing rules (`session-listing.ts` `scanSessionFile`,
//! `deriveSessionStatus`): id, cwd, title, parent, timestamps, turn counts,
//! first prompt and lifecycle status.

use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde_json::Value;

use crate::json;

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

/// `SESSION_LIST_PREFIX_BYTES`: the head window, also the cap of a prompt recovered past it.
const PREFIX_BYTES: u64 = 4096;
/// `SESSION_LIST_SUFFIX_BYTES`: the tail window read for the status.
const SUFFIX_BYTES: u64 = 32_768;

/// The metadata of one session file; None when unreadable or not a session.
pub fn read_session_info(file: &Path, with_status: bool) -> Option<SessionInfo> {
	let stat = std::fs::metadata(file).ok()?;
	let size = stat.len();
	let modified_ms = stat
		.modified()
		.ok()
		.and_then(|at| at.duration_since(UNIX_EPOCH).ok())
		.map_or(0, |since| i64::try_from(since.as_millis()).unwrap_or(i64::MAX));
	let (content, suffix) = read_ends(file, PREFIX_BYTES, if with_status { SUFFIX_BYTES } else { 0 }).ok()?;
	let entries = parse_jsonl_lenient(&content);
	let header = parse_header(&content, &entries)?;

	let mut parsed_messages: u64 = 0;
	let mut assistant_turns: u64 = 0;
	let mut first_message = String::new();
	let mut short_summary: Option<String> = None;
	for entry in entries.iter().skip(1) {
		// `entry.type` on a null record throws, and the scan answers undefined.
		if entry.is_null() {
			return None;
		}
		let kind = entry.get("type").and_then(Value::as_str);
		if kind == Some("compaction")
			&& let Some(summary) = entry.get("shortSummary").and_then(Value::as_str)
		{
			short_summary = Some(summary.to_owned());
		}
		let Some(message) = entry.get("message").filter(|m| truthy(m)) else { continue };
		if kind != Some("message") {
			continue;
		}
		parsed_messages += 1;
		let role = message.get("role").and_then(Value::as_str);
		if role == Some("assistant") {
			assistant_turns += 1;
		}
		if role == Some("user") || role == Some("assistant") {
			let text = text_content(message.get("content"), " ")?;
			if !text.is_empty() && first_message.is_empty() && role == Some("user") {
				first_message = text;
			}
		}
	}

	assistant_turns = assistant_turns.max(count_markers(&content, Marker::Assistant)).max(count_markers(&suffix, Marker::Assistant));
	// Records between the bounded windows go unseen; one line pass recovers a
	// first prompt and an assistant turn the windows missed.
	let mut need_prompt = first_message.is_empty();
	let mut need_turn = assistant_turns == 0;
	if (need_prompt || need_turn) && size > PREFIX_BYTES {
		recover_from_lines(file, &mut need_prompt, &mut need_turn, &mut first_message, &mut assistant_turns);
	}
	if first_message.is_empty() {
		first_message = first_display_message(&content).unwrap_or_default();
	}
	let message_count = parsed_messages.max(count_markers(&content, Marker::Message));
	Some(SessionInfo {
		path: file.to_path_buf(),
		id: header.id,
		cwd: header.cwd.unwrap_or_default(),
		title: header.title.or(short_summary),
		parent_session_path: header.parent_session,
		continuation_of: header.continuation_of,
		created_ms: header.timestamp.as_deref().and_then(crate::time::parse_ms),
		modified_ms,
		message_count,
		assistant_turns,
		size,
		first_message: if first_message.is_empty() { "(no messages)".to_owned() } else { first_message },
		status: with_status.then(|| derive_status(&suffix)),
	})
}

/// One line-boundary pass over the whole file (`storage.scanLines`), stopping
/// once both a user prompt and an assistant turn are known. A read failure, or
/// content `textContent` cannot walk, keeps the bounded-window evidence.
fn recover_from_lines(file: &Path, need_prompt: &mut bool, need_turn: &mut bool, first_message: &mut String, turns: &mut u64) {
	let Ok(handle) = File::open(file) else { return };
	let mut reader = BufReader::with_capacity(1 << 20, handle);
	let mut buf = Vec::new();
	loop {
		buf.clear();
		match reader.read_until(b'\n', &mut buf) {
			Ok(0) | Err(_) => return,
			Ok(_) => {}
		}
		let mut line: &[u8] = &buf;
		if let Some(rest) = line.strip_suffix(b"\n") {
			line = rest;
		}
		if let Some(rest) = line.strip_suffix(b"\r") {
			line = rest;
		}
		if memchr::memmem::find(line, b"\"user\"").is_none() && memchr::memmem::find(line, b"\"assistant\"").is_none() {
			continue;
		}
		let text = String::from_utf8_lossy(line);
		let Some((role, message)) = turn_message(&text) else { continue };
		if role == "assistant" && *need_turn {
			*turns = 1;
			*need_turn = false;
		} else if role == "user" && *need_prompt {
			let Some(full) = text_content(message.get("content"), " ") else { return };
			let capped = json::utf16_prefix(&full, PREFIX_BYTES as usize);
			if !capped.is_empty() {
				*first_message = capped.to_owned();
				*need_prompt = false;
			}
		}
		if !*need_prompt && !*need_turn {
			return;
		}
	}
}

/// `parseTurnLine`: the role and message of a `message` record that is a user or assistant turn.
fn turn_message(line: &str) -> Option<(String, Value)> {
	let mut record: Value = serde_json::from_str(line).ok()?;
	if record.get("type").and_then(Value::as_str) != Some("message") {
		return None;
	}
	let message = record.get_mut("message")?.take();
	let role = message.get("role").and_then(Value::as_str)?.to_owned();
	(role == "user" || role == "assistant").then_some((role, message))
}

/// `peekFileEnds` + `TextDecoder.decode`: up to the first `prefix` and last
/// `suffix` bytes, each decoded lossily with a leading BOM dropped.
fn read_ends(file: &Path, prefix: u64, suffix: u64) -> std::io::Result<(String, String)> {
	let mut handle = File::open(file)?;
	let size = handle.metadata()?.len();
	let head_len = prefix.min(size);
	let tail_len = suffix.min(size);
	let mut head = Vec::with_capacity(head_len as usize);
	(&mut handle).take(head_len).read_to_end(&mut head)?;
	let tail = if tail_len == 0 {
		Vec::new()
	} else if size <= head_len {
		head[head.len().saturating_sub(tail_len as usize)..].to_vec()
	} else {
		handle.seek(SeekFrom::Start(size - tail_len))?;
		let mut tail = Vec::with_capacity(tail_len as usize);
		handle.take(tail_len).read_to_end(&mut tail)?;
		tail
	};
	Ok((decode(&head), decode(&tail)))
}

fn decode(bytes: &[u8]) -> String {
	let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
	String::from_utf8_lossy(bytes).into_owned()
}

/// `parseJsonlLenient`: every complete JSON value of the text, one per line;
/// malformed lines and an unterminated final record are skipped.
fn parse_jsonl_lenient(content: &str) -> Vec<Value> {
	content
		.split('\n')
		.filter(|line| !is_js_blank(line))
		.filter_map(|line| serde_json::from_str(line).ok())
		.collect()
}

fn is_js_blank(text: &str) -> bool {
	js_trim(text).is_empty()
}

/// `String.prototype.trim`: Unicode white space, line terminators and U+FEFF.
fn js_trim(text: &str) -> &str {
	text.trim_matches(|c: char| c.is_whitespace() || c == '\u{FEFF}')
}

fn truthy(value: &Value) -> bool {
	match value {
		Value::Null => false,
		Value::Bool(b) => *b,
		Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
		Value::String(s) => !s.is_empty(),
		Value::Array(_) | Value::Object(_) => true,
	}
}

/// pi-tui `textContent`: a string as is; an array's `text` blocks joined by
/// `separator`. None where JavaScript would throw (content neither a string
/// nor an array, or a null block).
fn text_content(content: Option<&Value>, separator: &str) -> Option<String> {
	match content? {
		Value::String(text) => Some(text.clone()),
		Value::Array(blocks) => {
			let mut text = String::new();
			let mut boundary = "";
			for block in blocks {
				if block.is_null() {
					return None;
				}
				if block.get("type").and_then(Value::as_str) != Some("text") {
					continue;
				}
				text.push_str(boundary);
				match block.get("text") {
					None | Some(Value::Null) => {}
					Some(value) => text.push_str(&js_string(value)),
				}
				boundary = separator;
			}
			Some(text)
		}
		Value::Null | Value::Bool(_) | Value::Number(_) | Value::Object(_) => None,
	}
}

/// JavaScript `String(value)` of a parsed JSON value.
fn js_string(value: &Value) -> String {
	match value {
		Value::Null => "null".to_owned(),
		Value::Bool(b) => b.to_string(),
		Value::Number(n) => {
			let mut out = String::new();
			json::write_number(&mut out, n.as_f64().unwrap_or(f64::NAN));
			out
		}
		Value::String(s) => s.clone(),
		Value::Array(items) => items
			.iter()
			.map(|item| match item {
				Value::Null => String::new(),
				other => js_string(other),
			})
			.collect::<Vec<_>>()
			.join(","),
		Value::Object(_) => "[object Object]".to_owned(),
	}
}

struct Header {
	id: String,
	cwd: Option<String>,
	title: Option<String>,
	parent_session: Option<String>,
	continuation_of: Option<String>,
	timestamp: Option<String>,
}

/// A title slot's override: absent, blank (clears the header title), or a title.
#[derive(Clone)]
enum TitleSlot {
	Absent,
	Blank,
	Title(String),
}

impl TitleSlot {
	fn of(title: Option<String>) -> TitleSlot {
		match title {
			None => TitleSlot::Absent,
			Some(title) if js_trim(&title).is_empty() => TitleSlot::Blank,
			Some(title) => TitleSlot::Title(title),
		}
	}

	fn apply(self, header_title: Option<String>) -> Option<String> {
		match self {
			TitleSlot::Absent => header_title,
			TitleSlot::Blank => None,
			TitleSlot::Title(title) => Some(title),
		}
	}
}

fn string_field(record: &Value, key: &str) -> Option<String> {
	record.get(key).and_then(Value::as_str).map(str::to_owned)
}

/// `parseSessionListHeader`: the parsed header record (after an optional title
/// slot), else the first non-blank line read by string extraction.
fn parse_header(content: &str, entries: &[Value]) -> Option<Header> {
	let first = entries.first();
	let first_is_title = first.and_then(|e| e.get("type")).and_then(Value::as_str) == Some("title");
	let slot = TitleSlot::of(if first_is_title { first.and_then(|e| string_field(e, "title")) } else { None });
	let record = entries.get(usize::from(first_is_title));
	if let Some(record) = record
		&& record.get("type").and_then(Value::as_str) == Some("session")
		&& let Some(id) = string_field(record, "id")
	{
		return Some(Header {
			id,
			cwd: string_field(record, "cwd"),
			title: slot.apply(string_field(record, "title")),
			parent_session: string_field(record, "parentSession"),
			continuation_of: string_field(record, "continuationOf"),
			timestamp: string_field(record, "timestamp"),
		});
	}

	let mut slot = TitleSlot::Absent;
	let mut first_non_empty = true;
	for raw in content.split('\n') {
		let line = js_trim(raw.strip_suffix('\r').unwrap_or(raw));
		if line.is_empty() {
			continue;
		}
		if first_non_empty && extract_string(line, "type", 0).as_deref() == Some("title") {
			slot = TitleSlot::of(extract_string(line, "title", 0));
			first_non_empty = false;
			continue;
		}
		if extract_string(line, "type", 0).as_deref() != Some("session") {
			return None;
		}
		let id = extract_string(line, "id", 0).filter(|id| !id.is_empty())?;
		return Some(Header {
			id,
			cwd: extract_string(line, "cwd", 0),
			title: slot.apply(extract_string(line, "title", 0)),
			parent_session: extract_string(line, "parentSession", 0),
			continuation_of: extract_string(line, "continuationOf", 0),
			timestamp: extract_string(line, "timestamp", 0),
		});
	}
	None
}

/// `decodeJsonStringFragment`: the JSON string body decoded, else the common
/// escapes replaced by hand.
fn decode_fragment(value: &str) -> String {
	let safe = value.strip_suffix('\\').unwrap_or(value);
	if let Ok(text) = serde_json::from_str::<String>(&format!("\"{safe}\"")) {
		return text;
	}
	safe.replace("\\n", "\n").replace("\\r", "\r").replace("\\t", "\t").replace("\\\"", "\"").replace("\\\\", "\\")
}

/// `extractStringProperty`: the string value of the first `"name"` at or
/// after byte `start`, read without parsing; an unterminated value is decoded
/// to the end of the text.
fn extract_string(source: &str, name: &str, start: usize) -> Option<String> {
	let bytes = source.as_bytes();
	let needle = format!("\"{name}\"");
	let property = start + memchr::memmem::find(&bytes[start..], needle.as_bytes())?;
	let colon_from = property + name.len() + 2;
	let colon = colon_from + memchr::memchr(b':', bytes.get(colon_from..)?)?;
	let mut value = colon + 1;
	while let Some(&c) = bytes.get(value) {
		if c != b' ' && c != b'\t' && c != b'\n' && c != b'\r' {
			break;
		}
		value += 1;
	}
	if bytes.get(value) != Some(&b'"') {
		return None;
	}
	let value_start = value + 1;
	let mut escaped = false;
	for (offset, &c) in bytes[value_start..].iter().enumerate() {
		if escaped {
			escaped = false;
			continue;
		}
		if c == b'\\' {
			escaped = true;
			continue;
		}
		if c == b'"' {
			return Some(decode_fragment(&source[value_start..value_start + offset]));
		}
	}
	Some(decode_fragment(&source[value_start..]))
}

#[derive(Clone, Copy)]
enum Marker {
	Assistant,
	Message,
}

/// `countRoleMarkers`: `"role": "assistant"` or `"type": "message"` pairs found by string extraction.
fn count_markers(content: &str, marker: Marker) -> u64 {
	let (name, want) = match marker {
		Marker::Assistant => ("role", "assistant"),
		Marker::Message => ("type", "message"),
	};
	let key = format!("\"{name}\"");
	let bytes = content.as_bytes();
	let mut count = 0;
	let mut index = 0;
	while index < bytes.len() {
		let Some(found) = memchr::memmem::find(&bytes[index..], key.as_bytes()) else { break };
		let key_index = index + found;
		let Some(colon) = memchr::memchr(b':', &bytes[key_index + key.len()..]) else { break };
		if extract_string(content, name, key_index).as_deref() == Some(want) {
			count += 1;
		}
		index = key_index + key.len() + colon + 1;
	}
	count
}

/// `extractFirstDisplayMessageFromPrefix`: the first user text in the head
/// window, else the first developer or assistant text.
fn first_display_message(content: &str) -> Option<String> {
	let mut fallback = None;
	let mut index = content.find("\"role\"");
	while let Some(at) = index {
		let role = extract_string(content, "role", at);
		let text = extract_string(content, "content", at).or_else(|| extract_string(content, "text", at));
		if let Some(text) = text.filter(|t| !t.is_empty()) {
			match role.as_deref() {
				Some("user") => return Some(text),
				Some("developer" | "assistant") if fallback.is_none() => fallback = Some(text),
				_ => {}
			}
		}
		index = content.get(at + 6..).and_then(|rest| rest.find("\"role\"")).map(|found| at + 6 + found);
	}
	fallback
}

/// `deriveSessionStatus`: the class of the last `message` record in the tail window.
fn derive_status(suffix: &str) -> SessionStatus {
	if suffix.is_empty() {
		return SessionStatus::Unknown;
	}
	for line in suffix.split('\n').rev() {
		if !line.starts_with('{') {
			continue;
		}
		let Ok(entry) = serde_json::from_str::<Value>(line) else { continue };
		if entry.get("type").and_then(Value::as_str) != Some("message") {
			continue;
		}
		let Some(message) = entry.get("message").filter(|m| truthy(m)) else { continue };
		return status_of(message);
	}
	SessionStatus::Unknown
}

fn status_of(message: &Value) -> SessionStatus {
	match message.get("role").and_then(Value::as_str) {
		Some("assistant") => {
			match message.get("stopReason").and_then(Value::as_str) {
				Some("error") => return SessionStatus::Error,
				Some("aborted") => return SessionStatus::Aborted,
				Some("length") => return SessionStatus::Interrupted,
				_ => {}
			}
			let calls = message
				.get("content")
				.and_then(Value::as_array)
				.is_some_and(|blocks| blocks.iter().any(|b| b.get("type").and_then(Value::as_str) == Some("toolCall")));
			if calls { SessionStatus::Interrupted } else { SessionStatus::Complete }
		}
		Some("toolResult") => SessionStatus::Interrupted,
		Some("user") => SessionStatus::Pending,
		_ => SessionStatus::Unknown,
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn temp_file(name: &str, content: &str) -> PathBuf {
		let path = std::env::temp_dir().join(format!("omp-sessions-info-{}-{name}.jsonl", std::process::id()));
		std::fs::write(&path, content).unwrap();
		path
	}

	#[test]
	fn header_title_slot_and_compaction_summary() {
		let blank_slot = "{\"type\":\"title\",\"title\":\"  \"}\n{\"type\":\"session\",\"id\":\"s1\",\"title\":\"old\",\"timestamp\":\"2026-01-02T03:04:05.000Z\",\"cwd\":\"/w\"}\n{\"type\":\"compaction\",\"shortSummary\":\"sum\"}\n";
		let path = temp_file("slot", blank_slot);
		let info = read_session_info(&path, true).unwrap();
		assert_eq!(info.id, "s1");
		assert_eq!(info.title.as_deref(), Some("sum"));
		assert_eq!(info.created_ms, Some(1_767_323_045_000));
		assert_eq!(info.first_message, "(no messages)");
		assert_eq!(info.status, Some(SessionStatus::Unknown));
		std::fs::remove_file(path).unwrap();
	}

	#[test]
	fn turns_prompt_and_status() {
		let text = concat!(
			"{\"type\":\"session\",\"id\":\"s2\"}\n",
			"{\"type\":\"message\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"a\"},{\"type\":\"image\"},{\"type\":\"text\",\"text\":\"b\"}]}}\n",
			"{\"type\":\"message\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"toolCall\"}]}}\n",
		);
		let path = temp_file("turns", text);
		let info = read_session_info(&path, true).unwrap();
		assert_eq!(info.first_message, "a b");
		assert_eq!(info.assistant_turns, 1);
		assert_eq!(info.message_count, 2);
		assert_eq!(info.created_ms, None);
		assert_eq!(info.status, Some(SessionStatus::Interrupted));
		std::fs::remove_file(path).unwrap();
	}

	#[test]
	fn prompt_behind_the_head_window_is_recovered_capped() {
		let big = "x".repeat(5000);
		let text = format!(
			"{{\"type\":\"session\",\"id\":\"s3\"}}\n{{\"type\":\"custom\",\"data\":\"{big}\"}}\n{{\"type\":\"message\",\"message\":{{\"role\":\"user\",\"content\":\"{big}\"}}}}\n"
		);
		let path = temp_file("recover", &text);
		let info = read_session_info(&path, false).unwrap();
		assert_eq!(utf16_len_of(&info.first_message), 4096);
		assert_eq!(info.status, None);
		std::fs::remove_file(path).unwrap();
	}

	fn utf16_len_of(text: &str) -> usize {
		json::utf16_len(text)
	}

	#[test]
	fn extraction_of_truncated_header() {
		assert_eq!(extract_string("{\"type\": \"session\",\"id\":\"ab\\\"c", "id", 0).as_deref(), Some("ab\"c"));
		assert_eq!(count_markers("{\"role\":\"assistant\"},{\"role\": \"user\"},{\"role\":\"assistant\"", Marker::Assistant), 2);
		assert_eq!(first_display_message("{\"role\":\"assistant\",\"content\":\"hi\"}{\"role\":\"user\",\"text\":\"yo\"}").as_deref(), Some("yo"));
	}

	#[test]
	fn not_a_session() {
		let path = temp_file("none", "{\"type\":\"message\"}\n");
		assert_eq!(read_session_info(&path, true), None);
		std::fs::remove_file(path).unwrap();
	}
}
