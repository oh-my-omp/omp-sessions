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

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;

use crate::error::Result;
use crate::json::{Clipped, Obj, Value, utf16_len, utf16_prefix, write_number};
use crate::scan::{self, Child, Keep, LineSpan, Policy, ScannedRecord, ScanStats, SKIP, each, keep_all, keys, scalar};

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
	nodes: Vec<BranchNode>,
	collector: Collector,
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

const SPEECH: usize = 1 << 20;
const ARG: usize = 64 * 1024;
const DETAIL_CAP: usize = 2 << 20;
const FIRST_LINE_MAX: usize = 160;

static ID: Policy = scalar(512);
static FLAG: Policy = scalar(0);
static SPEECH_TEXT: Policy = scalar(SPEECH);
static ARG_TEXT: Policy = scalar(ARG);
static SUMMARY_TEXT: Policy = scalar(400);

// The TIMELINE and LIGHT trees below are the two instantiations of the
// TypeScript `entryPolicy(speech, arg, summary, args)`; the const `keys` tables
// force each instantiation to be written out.

/// TIMELINE = entryPolicy(SPEECH, ARG, 400, keepAll(ARG, 500)).
static TIMELINE_DETAILS: Policy = keys(&[
	("from", &ID),
	("to", &ID),
	("message", &SPEECH_TEXT),
	("body", &SPEECH_TEXT),
	("pool", &ID),
]);
static TIMELINE_ARGS: Policy = keep_all(ARG, 500);
static TIMELINE_BLOCK: Policy = keys(&[
	("type", &ID),
	("text", &SPEECH_TEXT),
	("thinking", &SPEECH_TEXT),
	("summary", &FLAG),
	("name", &ID),
	("id", &ID),
	("arguments", &TIMELINE_ARGS),
	("path", &ID),
]);
static TIMELINE_CONTENT: Policy =
	Policy::Keep(Keep { cap: SPEECH, max_items: 1000, child: Child::Each(&TIMELINE_BLOCK) });
static ENTRY_DATA: Policy = keys(&[
	("toolCallId", &ID),
	("toolName", &ID),
	("startedAt", &ID),
	("intent", &ID),
	("reason", &ID),
	("kind", &ID),
]);
static FILE_ITEM: Policy = keys(&[("path", &ID)]);
static FILES: Policy = each(&FILE_ITEM, 200);
static TIMELINE_MESSAGE: Policy = keys(&[
	("role", &ID),
	("content", &TIMELINE_CONTENT),
	("toolCallId", &ID),
	("toolName", &ID),
	("isError", &FLAG),
	("attribution", &ID),
	("customType", &ID),
	("display", &FLAG),
	("details", &TIMELINE_DETAILS),
	("command", &ARG_TEXT),
	("code", &ARG_TEXT),
	("exitCode", &FLAG),
	("cancelled", &FLAG),
	("excludeFromContext", &FLAG),
	("summary", &SUMMARY_TEXT),
	("files", &FILES),
]);
/// Everything an event shows.
static TIMELINE: Policy = keys(&[
	("type", &ID),
	("id", &ID),
	("parentId", &ID),
	("timestamp", &ID),
	("activeLeafId", &ID),
	("version", &FLAG),
	("cwd", &ID),
	("title", &ID),
	("parentSession", &ID),
	("customType", &ID),
	("display", &FLAG),
	("attribution", &ID),
	("content", &TIMELINE_CONTENT),
	("details", &TIMELINE_DETAILS),
	("data", &ENTRY_DATA),
	("summary", &SUMMARY_TEXT),
	("model", &ID),
	("role", &ID),
	("thinkingLevel", &ID),
	("mode", &ID),
	("label", &ID),
	("targetId", &ID),
	("fromId", &ID),
	("message", &TIMELINE_MESSAGE),
]);

/// A toolCall's arguments under LIGHT: the `path`/`i` an event kind reads for
/// identity plus the fields `callEvent` derives an event from, measured, with
/// each child's `name` and `agent` kept exact.
static LIGHT_TASK: Policy = keys(&[("name", &ARG_TEXT), ("agent", &ARG_TEXT), ("task", &FLAG)]);
static LIGHT_TASKS: Policy = each(&LIGHT_TASK, 500);
static LIGHT_ARGS: Policy = keys(&[
	("path", &ID),
	("i", &ID),
	("content", &FLAG),
	("context", &FLAG),
	("task", &FLAG),
	("name", &ARG_TEXT),
	("agent", &ARG_TEXT),
	("tasks", &LIGHT_TASKS),
]);
static LIGHT_BLOCK: Policy = keys(&[
	("type", &ID),
	("text", &FLAG),
	("thinking", &FLAG),
	("summary", &FLAG),
	("name", &ID),
	("id", &ID),
	("arguments", &LIGHT_ARGS),
	("path", &ID),
]);
static LIGHT_CONTENT: Policy = Policy::Keep(Keep { cap: 0, max_items: 1000, child: Child::Each(&LIGHT_BLOCK) });
static LIGHT_DETAILS: Policy =
	keys(&[("from", &ID), ("to", &ID), ("message", &FLAG), ("body", &FLAG), ("pool", &ID)]);
static LIGHT_MESSAGE: Policy = keys(&[
	("role", &ID),
	("content", &LIGHT_CONTENT),
	("toolCallId", &ID),
	("toolName", &ID),
	("isError", &FLAG),
	("attribution", &ID),
	("customType", &ID),
	("display", &FLAG),
	("details", &LIGHT_DETAILS),
	("command", &FLAG),
	("code", &FLAG),
	("exitCode", &FLAG),
	("cancelled", &FLAG),
	("excludeFromContext", &FLAG),
	("summary", &FLAG),
	("files", &FILES),
]);
/// LIGHT = entryPolicy(0, 0, 0, LIGHT_ARGS): an event's structure and measures
/// without its texts.
static LIGHT: Policy = keys(&[
	("type", &ID),
	("id", &ID),
	("parentId", &ID),
	("timestamp", &ID),
	("activeLeafId", &ID),
	("version", &FLAG),
	("cwd", &ID),
	("title", &ID),
	("parentSession", &ID),
	("customType", &ID),
	("display", &FLAG),
	("attribution", &ID),
	("content", &LIGHT_CONTENT),
	("details", &LIGHT_DETAILS),
	("data", &ENTRY_DATA),
	("summary", &FLAG),
	("model", &ID),
	("role", &ID),
	("thinkingLevel", &ID),
	("mode", &ID),
	("label", &ID),
	("targetId", &ID),
	("fromId", &ID),
	("message", &LIGHT_MESSAGE),
]);

/// What the index pass reads of every record: tree links, header and title,
/// the role and attribution that open a turn, a tool result's call, error flag
/// and measured content, a tool start's timing and intent.
static MEASURED_BLOCK: Policy = keys(&[("type", &ID), ("text", &FLAG)]);
static SKELETON_CONTENT: Policy =
	Policy::Keep(Keep { cap: 0, max_items: 1000, child: Child::Each(&MEASURED_BLOCK) });
static SKELETON_DATA: Policy = keys(&[("toolCallId", &ID), ("startedAt", &ID), ("intent", &ID)]);
static SKELETON_MESSAGE: Policy = keys(&[
	("role", &ID),
	("attribution", &ID),
	("toolCallId", &ID),
	("toolName", &ID),
	("isError", &FLAG),
	("customType", &ID),
	("content", &SKELETON_CONTENT),
]);
static SKELETON: Policy = keys(&[
	("type", &ID),
	("id", &ID),
	("parentId", &ID),
	("timestamp", &ID),
	("activeLeafId", &ID),
	("version", &FLAG),
	("cwd", &ID),
	("title", &ID),
	("parentSession", &ID),
	("customType", &ID),
	("data", &SKELETON_DATA),
	("message", &SKELETON_MESSAGE),
]);

/// What `load_entry`'s index pass reads: ids, a toolCall block's id, a tool
/// result's call id and a tool start's call id.
static INDEX_DATA: Policy = keys(&[("toolCallId", &ID)]);
static INDEX_BLOCK: Policy = keys(&[("type", &ID), ("id", &ID)]);
static INDEX_CONTENT: Policy = each(&INDEX_BLOCK, usize::MAX);
static INDEX_MESSAGE: Policy = keys(&[("role", &ID), ("toolCallId", &ID), ("content", &INDEX_CONTENT)]);
static INDEX: Policy = keys(&[
	("type", &ID),
	("id", &ID),
	("customType", &ID),
	("data", &INDEX_DATA),
	("message", &INDEX_MESSAGE),
]);

/// The detail projection of one record: everything kept except the signature
/// and provider blobs, strings capped at 2 MiB, containers at 5000 children.
static DETAIL: Policy = Policy::Keep(Keep {
	cap: DETAIL_CAP,
	max_items: 5000,
	child: Child::SameExcept(&["thinkingSignature", "textSignature", "providerPayload"]),
});

/// `str` of the TypeScript scanner helpers: a plain string, or a clipped
/// string's captured prefix.
fn strv(value: Option<&Value>) -> Option<&str> {
	value.and_then(Value::as_str)
}

/// `text`: the value as a free text when it is a (possibly clipped) string.
fn textv(value: Option<&Value>) -> Option<Text> {
	match value {
		Some(Value::Str(text)) => Some(Text::Plain(text.clone())),
		Some(Value::Clipped(clipped)) => Some(Text::Clipped(clipped.clone())),
		_ => None,
	}
}

/// `obj`: the value as an object (arrays and clipped strings are not objects).
fn objv(value: Option<&Value>) -> Option<&Obj> {
	value.and_then(Value::as_obj)
}

/// `value === true`.
fn flag(value: Option<&Value>) -> bool {
	matches!(value, Some(Value::Bool(true)))
}

fn numv(value: Option<&Value>) -> Option<f64> {
	match value {
		Some(Value::Num(number)) => Some(*number),
		_ => None,
	}
}

/// Joins a string-or-blocks content field's text blocks; images become `[image]`.
fn content_text(value: Option<&Value>) -> Option<Text> {
	if let Some(direct) = textv(value) {
		return Some(direct);
	}
	let blocks = value.and_then(Value::as_arr)?;
	let mut parts: Vec<String> = Vec::new();
	let mut clipped = false;
	let mut chars = 0usize;
	for block in blocks {
		let Some(fields) = objv(Some(block)) else { continue };
		if strv(fields.get("type")) == Some("image") {
			parts.push("[image]".to_string());
		}
		let Some(body) = textv(fields.get("text")) else { continue };
		match body {
			Text::Clipped(capture) => {
				clipped = true;
				chars += capture.chars;
				parts.push(capture.text);
			}
			Text::Plain(text) => {
				chars += utf16_len(&text);
				parts.push(text);
			}
		}
	}
	let joined = parts.join("\n");
	if clipped {
		let lines = joined.split('\n').count();
		Some(Text::Clipped(Clipped { text: joined, chars, lines }))
	} else {
		Some(Text::Plain(joined))
	}
}

fn text_chars(value: &Text) -> usize {
	match value {
		Text::Plain(text) => utf16_len(text),
		Text::Clipped(clipped) => clipped.chars,
	}
}

/// `textChars(contentText(content))` of a TIMELINE projection, computed from
/// the measured SKELETON projection: joined with `\n` and `[image]` parts while
/// no block passes the speech cap, else the sum of the block texts' lengths
/// (contentText's Clipped form).
fn content_chars(value: Option<&Value>) -> u64 {
	if let Some(direct) = textv(value) {
		return text_chars(&direct) as u64;
	}
	let Some(blocks) = value.and_then(Value::as_arr) else { return 0 };
	let mut parts = 0u64;
	let mut images = 0u64;
	let mut chars = 0u64;
	let mut clipped = false;
	for block in blocks {
		let Some(fields) = objv(Some(block)) else { continue };
		if strv(fields.get("type")) == Some("image") {
			parts += 1;
			images += 1;
		}
		let Some(body) = textv(fields.get("text")) else { continue };
		let length = text_chars(&body) as u64;
		parts += 1;
		chars += length;
		if length > SPEECH as u64 {
			clipped = true;
		}
	}
	if clipped { chars } else { chars + images * "[image]".len() as u64 + parts.saturating_sub(1) }
}

static TAG_ONLY: LazyLock<Regex> =
	LazyLock::new(|| Regex::new(r"^</?[A-Za-z0-9_:.-]+(?:[\s\x{FEFF}][^>]*)?>$").expect("static regex"));

/// `String.prototype.trim`: Unicode whitespace plus U+FEFF.
fn js_trim(text: &str) -> &str {
	text.trim_matches(|ch: char| ch.is_whitespace() || ch == '\u{feff}')
}

/// First line carrying content (bare wrapper tags such as `<system-notice>` skipped), cut to `FIRST_LINE_MAX`.
fn first_line(value: Option<&Text>) -> String {
	let text = value.map_or("", Text::shown);
	let flat = text
		.split('\n')
		.map(js_trim)
		.find(|line| !line.is_empty() && !TAG_ONLY.is_match(line))
		.unwrap_or("");
	if utf16_len(flat) > FIRST_LINE_MAX {
		format!("{}…", utf16_prefix(flat, FIRST_LINE_MAX - 1))
	} else {
		flat.to_string()
	}
}

static ARTIFACT_REF: LazyLock<Regex> =
	LazyLock::new(|| Regex::new(r"(?-u:\b)artifact://([0-9]+)").expect("static regex"));
static AGENT_REF: LazyLock<Regex> =
	LazyLock::new(|| Regex::new(r"(?-u:\b)(?:agent|history)://([A-Za-z0-9_.-]+)").expect("static regex"));
static LOCAL_REF: LazyLock<Regex> =
	LazyLock::new(|| Regex::new(r#"(?-u:\b)local://([^\s\x{FEFF})\]`'"<>]+)"#).expect("static regex"));

/// Rewrites references to the owning session's resources into session:// URLs.
pub fn rewrite_refs(text: &str, sid: &str) -> String {
	let rewritten = ARTIFACT_REF.replace_all(text, |m: &regex::Captures<'_>| {
		format!("session://{sid}/artifacts/{}", &m[1])
	});
	let rewritten = AGENT_REF.replace_all(&rewritten, |m: &regex::Captures<'_>| {
		let id = &m[1];
		// The TypeScript `(?!all\b)` lookahead, by hand: a capture starting with
		// `all` followed by the end or a non-word byte is left alone.
		if id.starts_with("all")
			&& (id.len() == 3 || !matches!(id.as_bytes()[3], b'0'..=b'9' | b'A'..=b'Z' | b'a'..=b'z' | b'_'))
		{
			return m[0].to_string();
		}
		format!("session://{sid}/agents/{id}")
	});
	LOCAL_REF
		.replace_all(&rewritten, |m: &regex::Captures<'_>| format!("session://{sid}/local/{}", &m[1]))
		.into_owned()
}

/// `rewriteRefs` of one free text; a clipped text keeps its measures.
fn rewrite_text(value: Text, sid: &str) -> Text {
	match value {
		Text::Plain(text) => Text::Plain(rewrite_refs(&text, sid)),
		Text::Clipped(mut clipped) => {
			clipped.text = rewrite_refs(&clipped.text, sid);
			Text::Clipped(clipped)
		}
	}
}

static TASK_RESULT: LazyLock<Regex> = LazyLock::new(|| {
	Regex::new(r#"<task-result id="([^"]*)" agent="([^"]*)" status="([^"]*)" duration="([^"]*)">([\s\S]*?)</task-result>"#)
		.expect("static regex")
});
static OUTPUT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<output>\n?([\s\S]*?)\n?</output>").expect("static regex"));

/// `<task-result>` blocks of a result body, each carrying its inner text
/// (`<output>` unwrapped when present, trimmed).
fn task_results(body: &Text) -> Vec<Item> {
	let mut items = Vec::new();
	for found in TASK_RESULT.captures_iter(body.shown()) {
		let inner = found.get(5).map_or("", |group| group.as_str());
		let text = OUTPUT
			.captures(inner)
			.and_then(|m| m.get(1))
			.map_or(inner, |group| group.as_str());
		items.push(Item::Subresult {
			id: found[1].to_string(),
			agent: found[2].to_string(),
			status: found[3].to_string(),
			duration: found[4].to_string(),
			text: Text::Plain(js_trim(text).to_string()),
		});
	}
	items
}

/// What an entry can yield: `None` never yields items (a tool result of an
/// ordinary tool, a custom entry other than session_exit), `Subresults` may
/// carry `<task-result>` blocks, `Items` everything else.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Yield {
	None,
	Subresults,
	Items,
}

/// An item awaiting call completion; its turn comes from the index pass.
#[derive(Clone, Debug)]
enum Item {
	User { text: Text, injected: bool },
	Developer { text: Text },
	Thinking { block: usize, text: Text },
	Say { block: usize, text: Text },
	Call { block: usize, id: String, tool: String, args: Value },
	IrcIn { from: String, to: String, text: Text },
	Subresult { id: String, agent: String, status: String, duration: String, text: Text },
	Exec { lang: ExecLang, source: Text, status: String },
	Marker { what: String, detail: String },
}

impl Item {
	/// The content-block index the item carries (`"block" in item`), else None.
	fn block(&self) -> Option<usize> {
		match self {
			Item::Thinking { block, .. } | Item::Say { block, .. } | Item::Call { block, .. } => Some(*block),
			Item::User { .. }
			| Item::Developer { .. }
			| Item::IrcIn { .. }
			| Item::Subresult { .. }
			| Item::Exec { .. }
			| Item::Marker { .. } => None,
		}
	}
}

/// The tool names whose toolResult body carries `<task-result>` blocks.
fn is_subagent_tool(name: Option<&str>) -> bool {
	matches!(name, Some("wait" | "task" | "fork_task"))
}

/// Items of a custom (non-tool) message, from either a custom_message entry or a custom-role message.
fn custom_items(custom_type: &str, fields: &Obj, agent: &str) -> Vec<Item> {
	let details = objv(fields.get("details"));
	let body = content_text(fields.get("content"));
	match custom_type {
		"irc:incoming" => vec![Item::IrcIn {
			from: details.and_then(|d| strv(d.get("from"))).unwrap_or("?").to_string(),
			to: if agent.is_empty() { "main".to_string() } else { agent.to_string() },
			text: details
				.and_then(|d| textv(d.get("message")))
				.or(body)
				.unwrap_or_else(|| Text::Plain(String::new())),
		}],
		"irc:relay" => vec![Item::IrcIn {
			from: details.and_then(|d| strv(d.get("from"))).unwrap_or("?").to_string(),
			to: details.and_then(|d| strv(d.get("to"))).unwrap_or("?").to_string(),
			text: details
				.and_then(|d| textv(d.get("body")))
				.or(body)
				.unwrap_or_else(|| Text::Plain(String::new())),
		}],
		"irc:workpool" => vec![Item::IrcIn {
			from: format!("pool {}", details.and_then(|d| strv(d.get("pool"))).unwrap_or("?")),
			to: details.and_then(|d| strv(d.get("to"))).unwrap_or("?").to_string(),
			text: details
				.and_then(|d| textv(d.get("body")))
				.or(body)
				.unwrap_or_else(|| Text::Plain(String::new())),
		}],
		"async-result" => {
			let results = body.as_ref().map_or_else(Vec::new, task_results);
			if results.is_empty() {
				vec![Item::Marker { what: custom_type.to_string(), detail: first_line(body.as_ref()) }]
			} else {
				results
			}
		}
		_ => vec![Item::Marker { what: custom_type.to_string(), detail: first_line(body.as_ref()) }],
	}
}

/// `[a, b].filter(Boolean).join(" · ")`: nonempty parts joined with ` · `.
fn join_nonempty(parts: impl IntoIterator<Item = Option<String>>) -> String {
	parts
		.into_iter()
		.flatten()
		.filter(|part| !part.is_empty())
		.collect::<Vec<_>>()
		.join(" · ")
}

/// Items of one `message` entry.
fn message_items(message: &Obj, entry: &str, agent: &str) -> Vec<Item> {
	let role = strv(message.get("role"));
	match role {
		Some("user") => {
			let body = content_text(message.get("content"));
			body.map_or_else(Vec::new, |text| {
				vec![Item::User { text, injected: strv(message.get("attribution")) == Some("agent") }]
			})
		}
		Some("developer") => {
			let body = content_text(message.get("content"));
			body.map_or_else(Vec::new, |text| vec![Item::Developer { text }])
		}
		Some("assistant") => {
			let mut items = Vec::new();
			let blocks = message.get("content").and_then(Value::as_arr).unwrap_or(&[]);
			for (block, value) in blocks.iter().enumerate() {
				let Some(fields) = objv(Some(value)) else { continue };
				match strv(fields.get("type")) {
					Some("thinking") => {
						if let Some(body) = textv(fields.get("thinking"))
							&& text_chars(&body) > 0
						{
							items.push(Item::Thinking { block, text: body });
						}
					}
					Some("redactedThinking") => {
						items.push(Item::Marker { what: "thinking".to_string(), detail: "redacted".to_string() });
					}
					Some("text") => {
						if let Some(body) = textv(fields.get("text"))
							&& text_chars(&body) > 0
						{
							items.push(Item::Say { block, text: body });
						}
					}
					Some("toolCall") => items.push(Item::Call {
						block,
						id: strv(fields.get("id")).map_or_else(|| format!("{entry}.{block}"), str::to_string),
						tool: strv(fields.get("name")).unwrap_or("?").to_string(),
						args: fields.get("arguments").cloned().unwrap_or(Value::Null),
					}),
					Some(other) if other != "image" => items.push(Item::Marker {
						what: other.to_string(),
						detail: strv(fields.get("name")).unwrap_or("").to_string(),
					}),
					_ => {}
				}
			}
			items
		}
		Some("toolResult") => {
			// Only the subagent tools deliver `<task-result>` blocks; elsewhere the text is tool output (measured by the index pass).
			let body = content_text(message.get("content"));
			match body {
				Some(body) if is_subagent_tool(strv(message.get("toolName"))) => task_results(&body),
				_ => Vec::new(),
			}
		}
		Some(role @ ("bashExecution" | "pythonExecution")) => {
			if flag(message.get("excludeFromContext")) {
				return Vec::new();
			}
			let status = if flag(message.get("cancelled")) {
				"cancelled".to_string()
			} else if let Some(exit) = numv(message.get("exitCode"))
				&& exit != 0.0
			{
				let mut status = String::from("error · exit ");
				write_number(&mut status, exit);
				status
			} else {
				"ok".to_string()
			};
			let source = textv(message.get(if role == "bashExecution" { "command" } else { "code" }))
				.unwrap_or_else(|| Text::Plain(String::new()));
			vec![Item::Exec {
				lang: if role == "bashExecution" { ExecLang::Bash } else { ExecLang::Python },
				source,
				status,
			}]
		}
		Some(role @ ("custom" | "hookMessage")) => {
			custom_items(strv(message.get("customType")).unwrap_or(role), message, agent)
		}
		Some("fileMention") => {
			let files = message.get("files").and_then(Value::as_arr).map_or_else(Vec::new, |files| {
				files
					.iter()
					.map(|file| objv(Some(file)).and_then(|f| strv(f.get("path"))).unwrap_or("?").to_string())
					.collect::<Vec<_>>()
			});
			vec![Item::Marker { what: "file mention".to_string(), detail: files.join(", ") }]
		}
		Some(role @ ("branchSummary" | "compactionSummary")) => vec![Item::Marker {
			what: role.to_string(),
			detail: first_line(textv(message.get("summary")).as_ref()),
		}],
		Some(role) => vec![Item::Marker { what: role.to_string(), detail: String::new() }],
		None => Vec::new(),
	}
}

/// Items of one non-header entry.
fn entry_items(record: &Obj, entry: &str, agent: &str) -> Vec<Item> {
	match strv(record.get("type")) {
		Some("message") => {
			objv(record.get("message")).map_or_else(Vec::new, |message| message_items(message, entry, agent))
		}
		Some("custom_message") => {
			custom_items(strv(record.get("customType")).unwrap_or("custom_message"), record, agent)
		}
		Some("custom") => {
			let data = objv(record.get("data"));
			if strv(record.get("customType")) == Some("session_exit") {
				let detail = join_nonempty([
					data.and_then(|d| strv(d.get("kind"))).map(str::to_string),
					data.and_then(|d| strv(d.get("reason"))).map(str::to_string),
				]);
				vec![Item::Marker { what: "session exit".to_string(), detail }]
			} else {
				Vec::new()
			}
		}
		Some("compaction") => vec![Item::Marker {
			what: "compaction".to_string(),
			detail: first_line(textv(record.get("summary")).as_ref()),
		}],
		Some("branch_summary") => vec![Item::Marker {
			what: "branch summary".to_string(),
			detail: format!(
				"from #{} · {}",
				strv(record.get("fromId")).unwrap_or("?"),
				first_line(textv(record.get("summary")).as_ref())
			),
		}],
		Some("model_change") => vec![Item::Marker {
			what: "model".to_string(),
			detail: join_nonempty([
				strv(record.get("model")).map(str::to_string),
				strv(record.get("role")).map(str::to_string),
			]),
		}],
		Some("thinking_level_change") => vec![Item::Marker {
			what: "thinking level".to_string(),
			detail: strv(record.get("thinkingLevel")).unwrap_or("off").to_string(),
		}],
		Some("mode_change") => {
			vec![Item::Marker { what: "mode".to_string(), detail: strv(record.get("mode")).unwrap_or("").to_string() }]
		}
		Some("title_change") => {
			vec![Item::Marker { what: "title".to_string(), detail: strv(record.get("title")).unwrap_or("").to_string() }]
		}
		Some("reset_boundary") => {
			vec![Item::Marker { what: "context reset".to_string(), detail: String::new() }]
		}
		Some("label") => vec![Item::Marker {
			what: "label".to_string(),
			detail: format!(
				"{} → #{}",
				strv(record.get("label")).unwrap_or("(cleared)"),
				strv(record.get("targetId")).unwrap_or("?")
			),
		}],
		Some("session_init") => vec![Item::Marker { what: "session init".to_string(), detail: String::new() }],
		_ => Vec::new(),
	}
}

/// What `entry_items` can yield for a record, judged from its SKELETON projection.
fn entry_yields(record: &Obj) -> Yield {
	match strv(record.get("type")) {
		Some("message") => {
			let message = objv(record.get("message"));
			match message.and_then(|m| strv(m.get("role"))) {
				Some("toolResult") => {
					if is_subagent_tool(message.and_then(|m| strv(m.get("toolName")))) {
						Yield::Subresults
					} else {
						Yield::None
					}
				}
				Some(role @ ("custom" | "hookMessage")) => {
					if message.and_then(|m| strv(m.get("customType"))).unwrap_or(role) == "async-result" {
						Yield::Subresults
					} else {
						Yield::Items
					}
				}
				_ => Yield::Items,
			}
		}
		Some("custom_message") => {
			if strv(record.get("customType")) == Some("async-result") {
				Yield::Subresults
			} else {
				Yield::Items
			}
		}
		Some("custom") => {
			if strv(record.get("customType")) == Some("session_exit") {
				Yield::Items
			} else {
				Yield::None
			}
		}
		_ => Yield::Items,
	}
}

#[derive(Debug)]
struct ResultInfo {
	entry: String,
	is_error: bool,
	chars: u64,
	t: String,
}

#[derive(Debug)]
struct StartInfo {
	intent: Option<String>,
	started_at: Option<String>,
}

/// The tool results and tool starts the index pass collected for the whole file.
#[derive(Debug, Default)]
struct Collector {
	results: HashMap<String, ResultInfo>,
	starts: HashMap<String, StartInfo>,
}

/// Records a tool result or tool start of a SKELETON projection into `collector`; results and starts anywhere in the file count.
fn collect(record: &Obj, entry: &str, t: &str, collector: &mut Collector) {
	match strv(record.get("type")) {
		Some("message") => {
			let message = objv(record.get("message"));
			let call_id = message.and_then(|m| strv(m.get("toolCallId")));
			if let (Some(message), Some(call_id)) = (message, call_id)
				&& strv(message.get("role")) == Some("toolResult")
			{
				collector.results.insert(
					call_id.to_string(),
					ResultInfo {
						entry: entry.to_string(),
						is_error: flag(message.get("isError")),
						chars: content_chars(message.get("content")),
						t: t.to_string(),
					},
				);
			}
		}
		Some("custom") if strv(record.get("customType")) == Some("tool_execution_start") => {
			let data = objv(record.get("data"));
			if let Some(call_id) = data.and_then(|d| strv(d.get("toolCallId"))) {
				collector.starts.insert(
					call_id.to_string(),
					StartInfo {
						intent: data.and_then(|d| strv(d.get("intent"))).map(str::to_string),
						started_at: data.and_then(|d| strv(d.get("startedAt"))).map(str::to_string),
					},
				);
			}
		}
		_ => {}
	}
}

/// The completed call (status, result entry, intent, duration) for a call item at `t`.
fn complete_call(id: String, tool: String, args: Value, t: &str, collector: &Collector) -> Call {
	let result = collector.results.get(&id);
	let start = collector.starts.get(&id);
	let began = start
		.and_then(|s| s.started_at.as_deref())
		.and_then(crate::time::parse_ms)
		.or_else(|| crate::time::parse_ms(t));
	let ended = result.and_then(|r| crate::time::parse_ms(&r.t));
	let arg_intent = objv(Some(&args)).and_then(|a| strv(a.get("i"))).map(str::to_string);
	Call {
		id,
		tool,
		args,
		intent: start.and_then(|s| s.intent.clone()).or(arg_intent),
		status: match result {
			None => CallStatus::Pending,
			Some(result) if result.is_error => CallStatus::Error,
			Some(_) => CallStatus::Ok,
		},
		result_entry: result.map(|r| r.entry.clone()),
		result_chars: result.map(|r| r.chars),
		dur_ms: began.zip(ended).map(|(began, ended)| (ended - began).max(0)),
	}
}

static WRITE_TARGET: LazyLock<Regex> =
	LazyLock::new(|| Regex::new(r"^agent://([^/?#\s\x{FEFF}]+)$").expect("static regex"));

/// One dispatch child of a `task`/`fork_task` arguments object.
fn child_task(tool: &str, item: &Obj) -> ChildTask {
	ChildTask {
		name: strv(item.get("name")).unwrap_or("?").to_string(),
		agent: strv(item.get("agent")).map_or_else(
			|| if tool == "fork_task" { "fork".to_string() } else { "task".to_string() },
			str::to_string,
		),
		task: textv(item.get("task")).unwrap_or_else(|| Text::Plain(String::new())),
	}
}

/// Turns a completed call into its event kind: outgoing IRC, dispatch, or a plain call.
fn call_event(base: EventBase, call: Call) -> SessionEvent {
	let args = objv(Some(&call.args));
	if call.tool == "write"
		&& let Some(target) = args.and_then(|a| strv(a.get("path")))
		&& let Some(found) = WRITE_TARGET.captures(target)
	{
		return SessionEvent {
			base,
			kind: EventKind::IrcOut {
				to: found[1].to_string(),
				text: args.and_then(|a| textv(a.get("content"))).unwrap_or_else(|| Text::Plain(String::new())),
				call,
			},
		};
	}
	if call.tool == "task" || call.tool == "fork_task" {
		let tasks = args.and_then(|a| a.get("tasks")).and_then(Value::as_arr);
		let children: Vec<ChildTask> = match tasks {
			Some(tasks) => tasks
				.iter()
				.filter_map(|value| objv(Some(value)).map(|item| child_task(&call.tool, item)))
				.collect(),
			None => args.map(|item| child_task(&call.tool, item)).into_iter().collect(),
		};
		let context = args.and_then(|a| textv(a.get("context")));
		return SessionEvent { base, kind: EventKind::Dispatch { call, context, children } };
	}
	SessionEvent { base, kind: EventKind::Call { call } }
}

/// Rewrites the session-resource references of one event's texts.
fn rewrite_event(event: SessionEvent, sid: &str) -> SessionEvent {
	let SessionEvent { base, kind } = event;
	let kind = match kind {
		EventKind::User { text, injected } => EventKind::User { text: rewrite_text(text, sid), injected },
		EventKind::Developer { text } => EventKind::Developer { text: rewrite_text(text, sid) },
		EventKind::Thinking { text } => EventKind::Thinking { text: rewrite_text(text, sid) },
		EventKind::Say { text } => EventKind::Say { text: rewrite_text(text, sid) },
		EventKind::IrcIn { from, to, text } => EventKind::IrcIn { from, to, text: rewrite_text(text, sid) },
		EventKind::Subresult { id, agent, status, duration, text } => {
			EventKind::Subresult { id, agent, status, duration, text: rewrite_text(text, sid) }
		}
		EventKind::IrcOut { to, text, call } => EventKind::IrcOut { to, text: rewrite_text(text, sid), call },
		EventKind::Dispatch { call, context, children } => EventKind::Dispatch {
			call,
			context: context.map(|text| rewrite_text(text, sid)),
			children: children
				.into_iter()
				.map(|mut child| {
					child.task = rewrite_text(child.task, sid);
					child
				})
				.collect(),
		},
		same @ (EventKind::Call { .. } | EventKind::Exec { .. } | EventKind::Marker { .. }) => same,
	};
	SessionEvent { base, kind }
}

/// A journal node as the leaf replay and branch walk see it.
#[derive(Debug)]
struct IndexNode {
	id: String,
	parent_id: Option<String>,
	/// `activeLeafId` replay state: None = field absent (self-selecting),
	/// Some(None) = JSON null (leaf cleared), Some(Some) = captured leaf id.
	active: Option<Option<String>>,
	line: u64,
	span: LineSpan,
	t: String,
	yields: Yield,
	/// The entry is a prompt that starts a new turn.
	opens_turn: bool,
}

/// `sessionEntryLeafId`: the leaf one entry selects on replay.
fn node_leaf(node: &IndexNode) -> Option<&str> {
	match &node.active {
		Some(leaf) => leaf.as_deref(),
		None => Some(node.id.as_str()),
	}
}

/// `resolveSessionLeafId`: the active leaf after replaying `nodes` in file
/// order; a captured leaf that no longer exists (rollback, rewrite) falls back
/// to the newest self-selecting entry.
fn resolve_leaf(nodes: &[IndexNode]) -> Option<String> {
	let last = nodes.last()?;
	let leaf = node_leaf(last);
	if leaf.is_none_or(|leaf| leaf == last.id) || leaf.is_some_and(|leaf| nodes.iter().any(|node| node.id == leaf)) {
		return leaf.map(str::to_string);
	}
	nodes.iter().rev().find(|node| node_leaf(node) == Some(node.id.as_str())).map(|node| node.id.clone())
}

/// The branch-walk form of a branch entry: what the build pass needs beyond
/// [`BranchEntry`] — its yields and byte span.
#[derive(Debug)]
struct BranchNode {
	entry: String,
	line: u64,
	span: LineSpan,
	t: String,
	turn: u32,
	yields: Yield,
}

/// Whether a record's SKELETON projection is a prompt that opens a turn:
/// a user message whose content reads as contentText (a string or blocks).
/// A subagent's first prompt comes from its parent with agent attribution, so
/// every prompt there opens a turn.
fn opens_turn(fields: &Obj, agent: &str) -> bool {
	if strv(fields.get("type")) != Some("message") {
		return false;
	}
	let Some(message) = objv(fields.get("message")) else { return false };
	let prompt = strv(message.get("role")) == Some("user")
		&& matches!(message.get("content"), Some(Value::Str(_) | Value::Clipped(_) | Value::Arr(_)));
	prompt && (strv(message.get("attribution")) != Some("agent") || !agent.is_empty())
}

/// The index pass over one session file (`agent` "" for the root session,
/// else the subagent id; `sid` the root session's full id). No text is captured.
pub fn index_session(file: &Path, sid: &str, agent: &str) -> Result<SessionIndex> {
	crate::metrics::transcript();
	let mut nodes: Vec<IndexNode> = Vec::new();
	let mut collector = Collector::default();
	let mut header: Option<Obj> = None;
	let mut title: Option<String> = None;
	let mut previous: Option<String> = None;
	let stats = scan::scan_file(
		file,
		&mut |_| &SKELETON,
		&mut |record: ScannedRecord| {
			let ScannedRecord { line, start, end, value } = record;
			let Some(fields) = value.as_obj() else { return };
			match strv(fields.get("type")) {
				Some("session") => {
					header = Some(fields.clone());
					return;
				}
				Some("title") => {
					title = strv(fields.get("title")).map(str::to_string);
					return;
				}
				_ => {}
			}
			if strv(fields.get("type")) == Some("title_change")
				&& let Some(text) = strv(fields.get("title"))
			{
				title = Some(text.to_string());
			}
			// v1 journals carry no ids; OMP's migration links them linearly, so do the same with stable line ids.
			let version = header.as_ref().and_then(|h| h.get("version"));
			let legacy = !matches!(version, Some(Value::Num(number)) if *number >= 2.0);
			let id = if legacy {
				format!("legacy-{line}")
			} else {
				strv(fields.get("id")).filter(|id| !id.is_empty()).map_or_else(|| format!("legacy-{line}"), str::to_string)
			};
			let parent_id = if legacy {
				previous.clone()
			} else {
				strv(fields.get("parentId")).map(str::to_string)
			};
			previous = Some(id.clone());
			let t = strv(fields.get("timestamp")).unwrap_or("").to_string();
			let active = match fields.get("activeLeafId") {
				None => None,
				Some(Value::Null) => Some(None),
				Some(value) => Some(value.as_str().map(str::to_string)),
			};
			nodes.push(IndexNode {
				parent_id,
				active,
				line,
				span: LineSpan { line, start, end },
				t: t.clone(),
				yields: entry_yields(fields),
				opens_turn: opens_turn(fields, agent),
				id: id.clone(),
			});
			collect(fields, &id, &t, &mut collector);
		},
	)?;
	let leaf = resolve_leaf(&nodes);
	let by_id: HashMap<&str, usize> = nodes.iter().enumerate().map(|(i, node)| (node.id.as_str(), i)).collect();
	let mut chain: Vec<usize> = Vec::new();
	let mut seen: HashSet<&str> = HashSet::new();
	let mut current = leaf.as_deref().and_then(|leaf| by_id.get(leaf)).copied();
	while let Some(i) = current {
		if !seen.insert(nodes[i].id.as_str()) {
			break;
		}
		chain.push(i);
		current = nodes[i].parent_id.as_deref().and_then(|parent| by_id.get(parent)).copied();
	}
	chain.reverse();
	let mut turn = 0u32;
	let mut branch_nodes: Vec<BranchNode> = Vec::with_capacity(chain.len());
	for i in chain {
		if nodes[i].opens_turn {
			turn += 1;
		}
		branch_nodes.push(BranchNode {
			entry: nodes[i].id.clone(),
			line: nodes[i].line,
			span: nodes[i].span,
			t: nodes[i].t.clone(),
			turn,
			yields: nodes[i].yields,
		});
	}
	let branch = branch_nodes
		.iter()
		.map(|node| BranchEntry { entry: node.entry.clone(), line: node.line, t: node.t.clone(), turn: node.turn })
		.collect();
	Ok(SessionIndex {
		meta: SessionMeta {
			sid: sid.to_string(),
			agent: agent.to_string(),
			file: file.to_path_buf(),
			cwd: header.as_ref().and_then(|h| strv(h.get("cwd"))).unwrap_or("").to_string(),
			title: title
				.filter(|title| !title.is_empty())
				.or_else(|| header.as_ref().and_then(|h| strv(h.get("title"))).map(str::to_string))
				.filter(|title| !title.is_empty()),
			created: header
				.as_ref()
				.and_then(|h| strv(h.get("timestamp")))
				.or_else(|| nodes.first().map(|node| node.t.as_str()))
				.unwrap_or("")
				.to_string(),
			parent_session: header.as_ref().and_then(|h| strv(h.get("parentSession"))).map(str::to_string),
			leaf,
			entries: nodes.len(),
			on_branch: branch_nodes.len(),
			stats,
		},
		branch,
		nodes: branch_nodes,
		collector,
	})
}

/// A branch entry the build pass projects: its node index, policy and whether
/// its events are returned.
struct Planned {
	node: usize,
	policy: &'static Policy,
	emit: bool,
}

/// The build pass: events of the branch entries `build` asks for, in branch
/// order, projected as it says. Entries that yield nothing are never read;
/// entries that may carry subagent results are always built in full so
/// duplicates are dropped exactly as over the whole branch.
pub fn build_events(index: &SessionIndex, build: &dyn Fn(&BranchEntry) -> Build) -> Result<Vec<SessionEvent>> {
	let mut planned: HashMap<u64, Planned> = HashMap::new();
	for (i, node) in index.nodes.iter().enumerate() {
		if node.yields == Yield::None {
			continue;
		}
		let mode = build(&index.branch[i]);
		if node.yields == Yield::Subresults {
			planned.insert(node.line, Planned { node: i, policy: &TIMELINE, emit: mode != Build::Skip });
			continue;
		}
		match mode {
			Build::Full => {
				planned.insert(node.line, Planned { node: i, policy: &TIMELINE, emit: true });
			}
			Build::Light => {
				planned.insert(node.line, Planned { node: i, policy: &LIGHT, emit: true });
			}
			Build::Skip => {}
		}
	}
	let mut spans: Vec<LineSpan> =
		index.nodes.iter().filter(|node| planned.contains_key(&node.line)).map(|node| node.span).collect();
	spans.sort_unstable_by_key(|span| span.start);
	let mut built: HashMap<u64, Vec<Item>> = HashMap::new();
	if !spans.is_empty() {
		scan::scan_lines(
			&index.meta.file,
			&spans,
			&mut |line| planned.get(&line).map_or(&SKIP, |plan| plan.policy),
			&mut |record: ScannedRecord| {
				if let (Some(fields), Some(plan)) = (record.value.as_obj(), planned.get(&record.line)) {
					built.insert(record.line, entry_items(fields, &index.nodes[plan.node].entry, &index.meta.agent));
				}
			},
		)?;
	}
	Ok(assemble_events(index, &mut built, &planned))
}

/// Assembles the built items into events in branch order, completing calls and
/// dropping duplicate subagent results, then rewriting references.
fn assemble_events(
	index: &SessionIndex,
	built: &mut HashMap<u64, Vec<Item>>,
	planned: &HashMap<u64, Planned>,
) -> Vec<SessionEvent> {
	let mut events: Vec<SessionEvent> = Vec::new();
	let mut seen_results: HashSet<String> = HashSet::new();
	for node in &index.nodes {
		let (Some(items), Some(plan)) = (built.remove(&node.line), planned.get(&node.line)) else { continue };
		for item in items {
			let base = EventBase {
				entry: node.entry.clone(),
				block: item.block(),
				t: node.t.clone(),
				line: node.line,
				turn: node.turn,
			};
			let event = match item {
				Item::Call { id, tool, args, .. } => call_event(base, complete_call(id, tool, args, &node.t, &index.collector)),
				Item::Subresult { id, agent, status, duration, text } => {
					let key = format!("{id}|{status}|{duration}");
					if !seen_results.insert(key) {
						continue;
					}
					SessionEvent { base, kind: EventKind::Subresult { id, agent, status, duration, text } }
				}
				Item::User { text, injected } => SessionEvent { base, kind: EventKind::User { text, injected } },
				Item::Developer { text } => SessionEvent { base, kind: EventKind::Developer { text } },
				Item::Thinking { text, .. } => SessionEvent { base, kind: EventKind::Thinking { text } },
				Item::Say { text, .. } => SessionEvent { base, kind: EventKind::Say { text } },
				Item::IrcIn { from, to, text } => SessionEvent { base, kind: EventKind::IrcIn { from, to, text } },
				Item::Exec { lang, source, status } => SessionEvent { base, kind: EventKind::Exec { lang, source, status } },
				Item::Marker { what, detail } => SessionEvent { base, kind: EventKind::Marker { what, detail } },
			};
			if plan.emit {
				events.push(rewrite_event(event, &index.meta.sid));
			}
		}
	}
	events
}

/// Loads one session file as its full active-branch timeline.
pub fn load_session(file: &Path, sid: &str, agent: &str) -> Result<LoadedSession> {
	let index = index_session(file, sid, agent)?;
	let events = build_events(&index, &|_| Build::Full)?;
	Ok(LoadedSession { meta: index.meta, events })
}

/// Finds one entry anywhere in the file and projects it in detail; None when absent.
///
/// The index pass records the byte spans of the target line, the toolCall line
/// and the start line; the detail pass then reads only those lines (the
/// TypeScript does two full passes — same result).
pub fn load_entry(file: &Path, selector: &EntrySelector) -> Result<Option<EntryDetail>> {
	let mut call_spans: HashMap<String, LineSpan> = HashMap::new();
	let mut start_spans: HashMap<String, LineSpan> = HashMap::new();
	let mut target: Option<(LineSpan, Option<String>)> = None;
	scan::scan_file(
		file,
		&mut |_| &INDEX,
		&mut |record: ScannedRecord| {
			let span = LineSpan { line: record.line, start: record.start, end: record.end };
			let Some(fields) = record.value.as_obj() else { return };
			let message = objv(fields.get("message"));
			if let Some(blocks) = message.and_then(|m| m.get("content")).and_then(Value::as_arr) {
				for block in blocks {
					if let Some(fields) = objv(Some(block))
						&& strv(fields.get("type")) == Some("toolCall")
						&& let Some(id) = strv(fields.get("id"))
					{
						call_spans.insert(id.to_string(), span);
					}
				}
			}
			if strv(fields.get("customType")) == Some("tool_execution_start")
				&& let Some(call_id) = objv(fields.get("data")).and_then(|d| strv(d.get("toolCallId")))
			{
				start_spans.insert(call_id.to_string(), span);
			}
			let hit = match selector {
				EntrySelector::Line(line) => *line == record.line,
				EntrySelector::Id(id) => strv(fields.get("id")) == Some(id.as_str()),
			};
			if hit && target.is_none() {
				let call_id = if message.and_then(|m| strv(m.get("role"))) == Some("toolResult") {
					message.and_then(|m| strv(m.get("toolCallId"))).map(str::to_string)
				} else {
					None
				};
				target = Some((span, call_id));
			}
		},
	)?;
	let Some((span, call_id)) = target else { return Ok(None) };
	let call_span = call_id.as_deref().and_then(|id| call_spans.get(id)).copied();
	let start_span = call_id.as_deref().and_then(|id| start_spans.get(id)).copied();
	let mut spans: Vec<LineSpan> = [Some(span), call_span, start_span].into_iter().flatten().collect();
	spans.sort_unstable_by_key(|span| span.start);
	spans.dedup();
	let mut records: HashMap<u64, Value> = HashMap::new();
	scan::scan_lines(
		file,
		&spans,
		&mut |_| &DETAIL,
		&mut |record: ScannedRecord| {
			records.insert(record.line, record.value);
		},
	)?;
	let blocks = call_span
		.and_then(|span| records.get(&span.line))
		.and_then(|value| objv(Some(value)))
		.and_then(|record| objv(record.get("message")))
		.and_then(|message| message.get("content"))
		.and_then(Value::as_arr);
	let call = blocks
		.and_then(|blocks| {
			blocks.iter().find(|block| objv(Some(*block)).and_then(|b| strv(b.get("id"))) == call_id.as_deref())
		})
		.cloned();
	let record = records.get(&span.line).cloned().unwrap_or(Value::Null);
	let start = start_span.and_then(|span| records.get(&span.line).cloned());
	Ok(Some(EntryDetail { line: span.line, record, call, start }))
}

#[cfg(test)]
mod tests {
	use super::*;

	fn obj(pairs: &[(&str, Value)]) -> Obj {
		let mut out = Obj::new();
		for (key, value) in pairs {
			out.insert(*key, value.clone());
		}
		out
	}

	fn text(s: &str) -> Value {
		Value::Str(s.to_string())
	}

	fn node(id: &str, parent: Option<&str>, active: Option<Option<&str>>, opens_turn: bool) -> IndexNode {
		IndexNode {
			id: id.to_string(),
			parent_id: parent.map(str::to_string),
			active: active.map(|leaf| leaf.map(str::to_string)),
			line: 0,
			span: LineSpan { line: 0, start: 0, end: 0 },
			t: String::new(),
			yields: Yield::Items,
			opens_turn,
		}
	}

	#[test]
	fn leaf_is_last_self_selecting_entry() {
		let nodes = [node("a", None, None, false), node("b", Some("a"), None, false)];
		assert_eq!(resolve_leaf(&nodes), Some("b".to_string()));
	}

	#[test]
	fn leaf_rolls_back_to_newest_self_selecting_when_captured_leaf_is_missing() {
		// The last entry captured leaf "gone" (deleted by a rollback): replay
		// falls back to the newest entry that selects itself — "b" selects "a",
		// so "a" is the newest self-selecting.
		let nodes = [
			node("a", None, None, false),
			node("b", Some("a"), Some(Some("a")), false),
			node("c", Some("b"), Some(Some("gone")), false),
		];
		assert_eq!(resolve_leaf(&nodes), Some("a".to_string()));
		// A captured leaf that still exists stands.
		let nodes = [
			node("a", None, None, false),
			node("b", Some("a"), None, false),
			node("c", Some("b"), Some(Some("a")), false),
		];
		assert_eq!(resolve_leaf(&nodes), Some("a".to_string()));
	}

	#[test]
	fn null_active_leaf_id_clears_the_leaf() {
		let nodes = [node("a", None, None, false), node("b", Some("a"), Some(None), false)];
		assert_eq!(resolve_leaf(&nodes), None);
		assert_eq!(resolve_leaf(&[]), None);
	}

	fn prompt(attribution: Option<&str>) -> Obj {
		let mut message = Obj::new();
		message.insert("role", text("user"));
		message.insert("content", text("hi"));
		if let Some(attribution) = attribution {
			message.insert("attribution", text(attribution));
		}
		obj(&[("type", text("message")), ("message", Value::Obj(message))])
	}

	#[test]
	fn agent_attributed_prompts_open_turns_only_in_subagents() {
		let injected = prompt(Some("agent"));
		assert!(!opens_turn(&injected, ""));
		assert!(opens_turn(&injected, "sub.1"));
		let own = prompt(None);
		assert!(opens_turn(&own, ""));
		assert!(opens_turn(&own, "sub.1"));
		// Non-message records never open a turn.
		let marker = obj(&[("type", text("mode_change")), ("mode", text("x"))]);
		assert!(!opens_turn(&marker, ""));
	}

	#[test]
	fn call_completes_from_result_and_start() {
		let mut collector = Collector::default();
		collector.starts.insert(
			"c1".to_string(),
			StartInfo { intent: Some("probe".to_string()), started_at: Some("2026-01-01T00:00:00.000Z".to_string()) },
		);
		collector.results.insert(
			"c1".to_string(),
			ResultInfo { entry: "e9".to_string(), is_error: false, chars: 42, t: "2026-01-01T00:00:01.500Z".to_string() },
		);
		let call = complete_call("c1".into(), "bash".into(), Value::Null, "2026-01-01T00:00:02.000Z", &collector);
		assert_eq!(call.status, CallStatus::Ok);
		assert_eq!(call.result_entry.as_deref(), Some("e9"));
		assert_eq!(call.result_chars, Some(42));
		assert_eq!(call.dur_ms, Some(1500));
		assert_eq!(call.intent.as_deref(), Some("probe"));
	}

	#[test]
	fn call_status_error_pending_and_arg_intent() {
		let mut collector = Collector::default();
		collector.results.insert(
			"c2".to_string(),
			ResultInfo { entry: "e9".to_string(), is_error: true, chars: 0, t: "bad time".to_string() },
		);
		let mut args = Obj::new();
		args.insert("i", text("arg intent"));
		let call = complete_call("c2".into(), "read".into(), Value::Obj(args), "2026-01-01T00:00:00.000Z", &collector);
		assert_eq!(call.status, CallStatus::Error);
		// start absent → intent from args.i; result timestamp unparseable → no duration.
		assert_eq!(call.intent.as_deref(), Some("arg intent"));
		assert_eq!(call.dur_ms, None);
		let pending = complete_call("nope".into(), "read".into(), Value::Null, "now", &collector);
		assert_eq!(pending.status, CallStatus::Pending);
		assert_eq!(pending.dur_ms, None);
	}

	#[test]
	fn subresults_dedupe_on_id_status_duration() {
		let node = BranchNode {
			entry: "e1".to_string(),
			line: 1,
			span: LineSpan { line: 1, start: 0, end: 10 },
			t: String::new(),
			turn: 1,
			yields: Yield::Subresults,
		};
		let index = SessionIndex {
			meta: SessionMeta {
				sid: "s1".to_string(),
				agent: String::new(),
				file: PathBuf::from("f"),
				cwd: String::new(),
				title: None,
				created: String::new(),
				parent_session: None,
				leaf: Some("e1".to_string()),
				entries: 1,
				on_branch: 1,
				stats: ScanStats::default(),
			},
			branch: vec![BranchEntry { entry: "e1".to_string(), line: 1, t: String::new(), turn: 1 }],
			nodes: vec![node],
			collector: Collector::default(),
		};
		let subresult = |id: &str| Item::Subresult {
			id: id.to_string(),
			agent: "w".to_string(),
			status: "ok".to_string(),
			duration: "1s".to_string(),
			text: Text::Plain("out".to_string()),
		};
		let mut built = HashMap::from([(1u64, vec![subresult("a"), subresult("a"), subresult("b")])]);
		let planned = HashMap::from([(1u64, Planned { node: 0, policy: &TIMELINE, emit: true })]);
		let events = assemble_events(&index, &mut built, &planned);
		assert_eq!(events.len(), 2);
		assert!(events.iter().all(|e| matches!(&e.kind, EventKind::Subresult { .. })));
	}

	#[test]
	fn rewrites_session_references() {
		let out = rewrite_refs("see artifact://12 and agent://worker.1", "s1");
		assert_eq!(out, "see session://s1/artifacts/12 and session://s1/agents/worker.1");
		assert_eq!(rewrite_refs("history://a-b", "s1"), "session://s1/agents/a-b");
		assert_eq!(rewrite_refs("local://notes.md", "s1"), "session://s1/local/notes.md");
		// The broadcast id and anything `all`-boundary is left alone; `all.x` is too.
		assert_eq!(rewrite_refs("agent://all", "s1"), "agent://all");
		assert_eq!(rewrite_refs("agent://all.x", "s1"), "agent://all.x");
		assert_eq!(rewrite_refs("agent://all2", "s1"), "session://s1/agents/all2");
	}

	#[test]
	fn first_line_skips_wrapper_tags_and_cuts() {
		let body = Text::Plain("<system-notice>\n\n  hello world  \nbye".to_string());
		assert_eq!(first_line(Some(&body)), "hello world");
		let long = Text::Plain("x".repeat(200));
		let cut = first_line(Some(&long));
		assert_eq!(cut.chars().count(), 160);
		assert!(cut.ends_with('…'));
	}
}
