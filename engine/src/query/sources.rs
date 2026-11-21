//! Schema `src`: the relations one `sql` or `export` call reads, built at the
//! start of the call and dropped at its end, so nothing in it outlives the call.
//! For the sessions a call names, only the relations its SQL reaches exist:
//!   src.files        each transcript file (root and subagents) with its sid and agent id
//!   src.entries      a view reading those JSONL files in place at query time, one row per line
//!   src.sessions     session metadata from each root session file's head and tail
//!   src.transcripts  each transcript's tree position and branch counts
//!   src.events       the active-branch events of every transcript (the events.jsonl records)
//! Rows passed in `tables` become `src.<name>`.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashSet};
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Condvar, LazyLock, Mutex, mpsc};

use regex::Regex;

use super::{Cancel, failed};
use crate::cache::Cache;
use crate::duck::{Appender, ColumnSlice, Duck, Scalar};
use crate::error::{EngineError, Result};
use crate::filter::{self, FilteredRead};
use crate::info;
use crate::json::{self, Obj, Value};
use crate::ops::{self, TranscriptRow};
use crate::protocol::{Context, Selection};
use crate::routes::{self, Locator, SessionRef};
use crate::{render, time};

/// Names the semantic and raw relations take in schema `src`; caller tables cannot reuse them.
pub const SOURCE_RELATIONS: [&str; 5] = ["files", "entries", "sessions", "transcripts", "events"];

/// Words that let a stored view or macro reach `src` without naming one relation:
/// `src` itself, the catalog views and functions that list relations (`duckdb_*`,
/// `pg_*`, `sqlite_*`, `information_schema`, `SHOW`, `DESCRIBE`, `SUMMARIZE`), and
/// the table functions that take a relation name at run time (`query`,
/// `query_table`, `pragma_*`). A definition holding any of them builds every
/// relation: its `CREATE VIEW` text is not a SELECT `json_serialize_sql` accepts.
static DEFINITION_REACHES_SRC: LazyLock<Regex> = LazyLock::new(|| {
	Regex::new(r"(?i)\b(?:src|query|query_table|pragma_\w+|duckdb_\w+|pg_\w+|sqlite_\w+|information_schema|show|describe|summarize)\b")
		.expect("valid pattern")
});

/// A relation the catalog owns: its rows list or describe the relations in `src`
/// (`information_schema.tables`, `pg_catalog.pg_class`, `sqlite_master`,
/// `duckdb_*` views), so a query that reads one builds every relation and its
/// rows match what a fully built `src` shows.
static CATALOG_SCHEMA: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^(?:information_schema|pg_catalog)$").expect("valid pattern"));
static CATALOG_RELATION: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^(?:duckdb|pg|sqlite)_\w+$").expect("valid pattern"));

/// A caller table name.
static TABLE_NAME: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z_][A-Za-z0-9_]{0,63}$").expect("valid pattern"));

/// Slack above a file's size for the longest line DuckDB may meet in it: a
/// transcript still being written grows while the call runs.
const LINE_SLACK_BYTES: u64 = 16 << 20;

/// A SQL string literal.
pub fn literal(value: &str) -> String {
	format!("'{}'", value.replace('\'', "''"))
}

/// A SQL identifier.
pub fn identifier(value: &str) -> String {
	format!("\"{}\"", value.replace('"', "\"\""))
}

/// The relations a call's `src` must hold.
enum Needed {
	All,
	Some(HashSet<&'static str>),
}

impl Needed {
	fn has(&self, name: &str) -> bool {
		match self {
			Needed::All => true,
			Needed::Some(names) => names.contains(name),
		}
	}

	fn is_empty(&self) -> bool {
		matches!(self, Needed::Some(names) if names.is_empty())
	}
}

/// The `src` relations one query's parse tree names, or `All` when that tree does
/// not name them: a statement DuckDB cannot serialize to a tree (anything but a
/// SELECT, so a CTAS included), a parse error, a reference resolved at run time
/// (a table function or a `SHOW`, whose relation names the tree lacks), or a
/// catalog relation (whose rows list the relations in `src`).
///
/// The tree is DuckDB's own parse (`json_serialize_sql`), so quoted, commented,
/// spaced, catalog-qualified, CTE and subquery references arrive as the
/// `schema_name`/`table_name` they denote; the walk covers every node rather than
/// recognized shapes. A parse error is not reported from here: the query's own
/// run reports it.
fn parse_references(duck: &Duck, sql: &str) -> Needed {
	let Ok(rows) = duck.strings("SELECT json_serialize_sql(?)", &[Scalar::Str(sql.to_owned())]) else { return Needed::All };
	let Some(Some(text)) = rows.into_iter().next() else { return Needed::All };
	let Ok(tree) = serde_json::from_str::<serde_json::Value>(&text) else { return Needed::All };
	if tree.get("error").and_then(serde_json::Value::as_bool) == Some(true) {
		return Needed::All;
	}
	let Some(statements) = tree.get("statements").and_then(serde_json::Value::as_array) else { return Needed::All };
	let mut names = HashSet::new();
	fn walk(node: &serde_json::Value, names: &mut HashSet<&'static str>) -> bool {
		match node {
			serde_json::Value::Array(items) => items.iter().all(|item| walk(item, names)),
			serde_json::Value::Object(fields) => {
				let kind = fields.get("type").and_then(serde_json::Value::as_str);
				if kind == Some("TABLE_FUNCTION") || kind == Some("SHOW_REF") {
					return false;
				}
				if kind == Some("BASE_TABLE") {
					let field = |name: &str| fields.get(name).and_then(serde_json::Value::as_str).unwrap_or("").to_lowercase();
					let (schema, table) = (field("schema_name"), field("table_name"));
					if CATALOG_SCHEMA.is_match(&schema) || CATALOG_RELATION.is_match(&table) {
						return false;
					}
					if schema == "src"
						&& let Some(name) = SOURCE_RELATIONS.iter().find(|name| **name == table)
					{
						names.insert(*name);
					}
				}
				fields.values().all(|value| walk(value, names))
			}
			_ => true,
		}
	}
	for statement in statements {
		if !walk(statement, &mut names) {
			return Needed::All;
		}
	}
	Needed::Some(names)
}

/// The SQL of every non-internal view and macro this database holds, in every
/// schema including `temp`: an object the caller kept across calls can reach
/// `src` without the call's own text naming it.
fn definition_texts(duck: &Duck) -> std::result::Result<Vec<String>, crate::duck::DuckError> {
	Ok(duck
		.strings(
			"SELECT sql FROM duckdb_views() WHERE NOT internal AND sql IS NOT NULL UNION ALL SELECT macro_definition FROM duckdb_functions() WHERE NOT internal AND macro_definition IS NOT NULL",
			&[],
		)?
		.into_iter()
		.flatten()
		.collect())
}

/// What one call's `src` must hold: `All` when the SQL's parse tree cannot name
/// its relations exactly, or when a stored view or macro definition can reach
/// `src`; else the relations the SQL names. Building a relation nobody reads only
/// costs time; missing one the query reads fails it, so an uncertain answer is `All`.
fn referenced_relations(duck: &Duck, sql: &str, definitions: &[String]) -> Needed {
	if definitions.iter().any(|text| DEFINITION_REACHES_SRC.is_match(text)) {
		return Needed::All;
	}
	parse_references(duck, sql)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ColumnType {
	Varchar,
	Bigint,
	Double,
	Boolean,
	Json,
}

impl ColumnType {
	fn sql(self) -> &'static str {
		match self {
			ColumnType::Varchar => "VARCHAR",
			ColumnType::Bigint => "BIGINT",
			ColumnType::Double => "DOUBLE",
			ColumnType::Boolean => "BOOLEAN",
			ColumnType::Json => "JSON",
		}
	}
}

/// One cell of a built relation.
enum Cell {
	Null,
	Str(String),
	Int(i64),
	Double(f64),
	Bool(bool),
}

impl Cell {
	fn text(value: Option<String>) -> Cell {
		value.map_or(Cell::Null, Cell::Str)
	}
}

struct Column {
	name: String,
	kind: ColumnType,
}

fn columns(spec: &[(&str, ColumnType)]) -> Vec<Column> {
	spec.iter().map(|(name, kind)| Column { name: (*name).to_owned(), kind: *kind }).collect()
}

use ColumnType::{Bigint, Json, Varchar};

const FILES: [(&str, ColumnType); 6] =
	[("sid", Varchar), ("agent", Varchar), ("parent", Varchar), ("file", Varchar), ("bytes", Bigint), ("modified", Varchar)];

const SESSIONS: [(&str, ColumnType); 11] = [
	("sid", Varchar),
	("project", Varchar),
	("cwd", Varchar),
	("title", Varchar),
	("created", Varchar),
	("modified", Varchar),
	("status", Varchar),
	("assistant_turns", Bigint),
	("size", Bigint),
	("parent", Varchar),
	("url", Varchar),
];

const TRANSCRIPTS: [(&str, ColumnType); 10] = [
	("sid", Varchar),
	("agent", Varchar),
	("parent", Varchar),
	("title", Varchar),
	("created", Varchar),
	("parent_session", Varchar),
	("entries", Bigint),
	("on_branch", Bigint),
	("events", Bigint),
	("url", Varchar),
];

const EVENTS: [(&str, ColumnType); 16] = [
	("sid", Varchar),
	("agent", Varchar),
	("seq", Bigint),
	("turn", Bigint),
	("t", Varchar),
	("kind", Varchar),
	("entry", Varchar),
	("block", Bigint),
	("line", Bigint),
	("call_id", Varchar),
	("tool", Varchar),
	("status", Varchar),
	("result_entry", Varchar),
	("text", Varchar),
	("data", Json),
	("url", Varchar),
];

/// A table of `src` being filled: its appender and its columns.
struct Table {
	appender: Appender,
	columns: Vec<Column>,
}

impl Table {
	/// Creates `src.<name>` and opens its appender.
	fn create(duck: &Duck, name: &str, columns: Vec<Column>, cancel: &Cancel) -> Result<Table> {
		let defs: Vec<String> = columns.iter().map(|column| format!("{} {}", identifier(&column.name), column.kind.sql())).collect();
		duck.run(&format!("CREATE TABLE src.{} ({})", identifier(name), defs.join(", "))).map_err(|error| failed(error, cancel))?;
		let appender = duck.appender("src", name).map_err(|error| failed(error, cancel))?;
		Ok(Table { appender, columns })
	}

	/// One row; cells past the row's end are NULL.
	fn row(&mut self, cells: Vec<Cell>, cancel: &Cancel) -> Result<()> {
		let mut cells = cells.into_iter();
		for column in &self.columns {
			let cell = cells.next().unwrap_or(Cell::Null);
			let appended = match (cell, column.kind) {
				(Cell::Null, _) => self.appender.null(),
				(Cell::Str(text), _) => self.appender.varchar(&text),
				(Cell::Int(number), ColumnType::Varchar | ColumnType::Json) => self.appender.varchar(&number.to_string()),
				(Cell::Int(number), ColumnType::Double) => self.appender.double(number as f64),
				(Cell::Int(number), _) => self.appender.int64(number),
				(Cell::Double(number), ColumnType::Varchar | ColumnType::Json) => {
					self.appender.varchar(&json::stringify(&Value::Num(number), None))
				}
				(Cell::Double(number), _) => self.appender.double(number),
				(Cell::Bool(flag), ColumnType::Varchar | ColumnType::Json) => self.appender.varchar(if flag { "true" } else { "false" }),
				(Cell::Bool(flag), _) => self.appender.boolean(flag),
			};
			appended.map_err(|error| failed(error, cancel))?;
		}
		self.appender.end_row().map_err(|error| failed(error, cancel))
	}

	/// `src.events` rows built on a worker, appended a chunk at a time.
	fn events(&mut self, rows: &EventRows, cancel: &Cancel) -> Result<()> {
		self.appender.columns(rows.len(), &rows.columns()).map_err(|error| failed(error, cancel))
	}

	fn close(self, cancel: &Cancel) -> Result<()> {
		self.appender.close().map_err(|error| failed(error, cancel))
	}
}

/// A VARCHAR column of rows being built: the values back to back, where each
/// row's value ends, and which rows are not NULL.
#[derive(Default)]
struct TextColumn {
	bytes: String,
	ends: Vec<usize>,
	valid: Vec<bool>,
}

impl TextColumn {
	fn push(&mut self, text: Option<&str>) {
		if let Some(text) = text {
			self.bytes.push_str(text);
		}
		self.end_row(text.is_some());
	}

	/// Ends a row whose value was written to `bytes` directly.
	fn end_row(&mut self, valid: bool) {
		self.ends.push(self.bytes.len());
		self.valid.push(valid);
	}

	fn slice(&self) -> ColumnSlice<'_> {
		ColumnSlice::Text { bytes: &self.bytes, ends: &self.ends, valid: &self.valid }
	}
}

/// A BIGINT column of rows being built.
#[derive(Default)]
struct IntColumn {
	values: Vec<i64>,
	valid: Vec<bool>,
}

impl IntColumn {
	fn push(&mut self, value: Option<i64>) {
		self.values.push(value.unwrap_or(0));
		self.valid.push(value.is_some());
	}

	fn slice(&self) -> ColumnSlice<'_> {
		ColumnSlice::Int { values: &self.values, valid: &self.valid }
	}
}

/// One transcript's `src.events` rows, column by column, built on a worker so
/// the thread owning DuckDB only hands whole chunks to the appender.
#[derive(Default)]
struct EventRows {
	sid: TextColumn,
	agent: TextColumn,
	seq: IntColumn,
	turn: IntColumn,
	t: TextColumn,
	kind: TextColumn,
	entry: TextColumn,
	block: IntColumn,
	line: IntColumn,
	call_id: TextColumn,
	tool: TextColumn,
	status: TextColumn,
	result_entry: TextColumn,
	text: TextColumn,
	data: TextColumn,
	url: TextColumn,
}

impl EventRows {
	fn len(&self) -> usize {
		self.seq.values.len()
	}

	/// One row. The text column shows `text`, else `source`; `data` holds the
	/// record without exactly that field, so no text lives in the table twice.
	fn push(&mut self, transcript: &TranscriptRow, seq: usize, record: &Obj) {
		let text = text_of(record, "text");
		let source = text_of(record, "source");
		let copied = if text.is_some() { "text" } else if source.is_some() { "source" } else { "" };
		self.sid.push(Some(&transcript.sid));
		self.agent.push(Some(&transcript.agent));
		self.seq.push(Some(seq as i64));
		self.turn.push(int_of(record, "turn"));
		self.t.push(text_of(record, "t").as_deref());
		self.kind.push(text_of(record, "kind").as_deref());
		self.entry.push(text_of(record, "entry").as_deref());
		self.block.push(int_of(record, "block"));
		self.line.push(int_of(record, "line"));
		self.call_id.push(text_of(record, "callId").as_deref());
		self.tool.push(text_of(record, "tool").as_deref());
		self.status.push(text_of(record, "status").as_deref());
		self.result_entry.push(text_of(record, "resultEntry").as_deref());
		self.text.push(text.or(source).as_deref());
		json::write_without(&mut self.data.bytes, record, copied);
		self.data.end_row(true);
		self.url.push(text_of(record, "url").as_deref());
	}

	/// The columns in `EVENTS` order.
	fn columns(&self) -> [ColumnSlice<'_>; 16] {
		[
			self.sid.slice(),
			self.agent.slice(),
			self.seq.slice(),
			self.turn.slice(),
			self.t.slice(),
			self.kind.slice(),
			self.entry.slice(),
			self.block.slice(),
			self.line.slice(),
			self.call_id.slice(),
			self.tool.slice(),
			self.status.slice(),
			self.result_entry.slice(),
			self.text.slice(),
			self.data.slice(),
			self.url.slice(),
		]
	}
}

/// `record[key]` when it is a string (a clipped string reads as its marked text).
fn text_of<'a>(record: &'a Obj, key: &str) -> Option<Cow<'a, str>> {
	match record.get(key)? {
		Value::Str(text) => Some(Cow::Borrowed(text)),
		Value::Clipped(clipped) => Some(Cow::Owned(clipped.marked())),
		_ => None,
	}
}

/// `record[key]` when it is a number.
fn int_of(record: &Obj, key: &str) -> Option<i64> {
	match record.get(key)? {
		Value::Num(number) => Some(*number as i64),
		_ => None,
	}
}

/// One `src.sessions` row from a root session file's head and tail, by the
/// listing rules the TypeScript `metadata` applied to omp's `readSessionInfo`.
fn session_row(session: &SessionRef) -> Option<Vec<Cell>> {
	let info = info::read_session_info(&session.file, true)?;
	let project = info.path.parent().and_then(|dir| dir.file_name()).map(|name| name.to_string_lossy().into_owned());
	// `info.title ?? firstMessage.split("\n", 1)[0].slice(0, 200)`
	let title = info
		.title
		.clone()
		.unwrap_or_else(|| json::utf16_prefix(info.first_message.split('\n').next().unwrap_or(""), 200).to_owned());
	Some(vec![
		Cell::Str(info.id.clone()),
		Cell::text(project),
		Cell::Str(info.cwd.clone()),
		Cell::Str(title),
		Cell::text(info.created_ms.map(time::iso_utc)),
		Cell::Str(time::iso_utc(info.modified_ms)),
		Cell::text(info.status.map(|status| status.name().to_owned())),
		Cell::Int(info.assistant_turns as i64),
		Cell::Int(info.size as i64),
		Cell::text(info.parent_session_path.clone()),
		Cell::Str(format!("session://{}", info.id)),
	])
}

/// The located sessions of `sids`, each once, in the order first named.
fn locate(sids: &[String], context: &Context) -> Result<Vec<SessionRef>> {
	let mut refs: Vec<SessionRef> = Vec::new();
	for prefix in sids {
		let session = routes::locate_session(&Locator::Prefix(prefix.clone()), context)?;
		if !refs.iter().any(|known| known.sid == session.sid) {
			refs.push(session);
		}
	}
	Ok(refs)
}

/// One `src.entries` scan of a transcript file. DuckDB sizes every read buffer
/// by `maximum_object_size`, so each file gets its own bound: its size plus slack.
fn entries_scan(file: &TranscriptRow) -> String {
	format!(
		"SELECT filename AS file, json FROM read_json([{}], format = 'newline_delimited', records = false, columns = {{'json': 'JSON'}}, filename = true, maximum_object_size = {})",
		literal(&file.file.to_string_lossy()),
		file.bytes + LINE_SLACK_BYTES
	)
}

/// One transcript's `src.transcripts` row and, when wanted, its `src.events` rows.
struct TranscriptRows {
	transcript: Vec<Cell>,
	events: EventRows,
}

fn transcript_rows(cache: Option<&Cache>, transcript: &TranscriptRow, wants_events: bool, now: i64) -> Result<TranscriptRows> {
	let (loaded, filter) =
		filter::load_filtered(cache, &transcript.file, &transcript.sid, &transcript.agent, None, None, now, FilteredRead::Records)?;
	let url = ops::transcript_url(&transcript.sid, &transcript.agent);
	let records = render::event_records(&loaded, &url, &filter);
	let mut events = EventRows::default();
	if wants_events {
		for (seq, record) in records.iter().enumerate() {
			if let Some(record) = record.as_obj() {
				events.push(transcript, seq, record);
			}
		}
	}
	let meta = &loaded.meta;
	let row = vec![
		Cell::Str(transcript.sid.clone()),
		Cell::Str(transcript.agent.clone()),
		Cell::text(transcript.parent.clone()),
		Cell::text(meta.title.clone()),
		Cell::Str(meta.created.clone()),
		Cell::text(meta.parent_session.clone()),
		Cell::Int(meta.entries as i64),
		Cell::Int(meta.on_branch as i64),
		Cell::Int(records.len() as i64),
		Cell::Str(url),
	];
	Ok(TranscriptRows { transcript: row, events })
}

/// Source bytes of the transcripts a worker may have taken, from the one the
/// appender waits for through its own.
const LOAD_WINDOW_BYTES: u64 = 1 << 30;

/// Loads `transcripts` on one worker per core and hands each one's rows to
/// `append` in list order, on the calling thread (the one owning DuckDB). A
/// worker takes a transcript past the one `append` waits for only while the
/// source bytes from that one through its own stay within `window_bytes`, so
/// the rows held follow the bytes loaded, and workers build several large
/// transcripts at once instead of waiting behind one. The transcript `append`
/// waits for is always taken, however large.
/// The first failure, in list order, stops the workers and is returned.
fn load_in_order(
	cache: Option<&Cache>,
	transcripts: &[TranscriptRow],
	wants_events: bool,
	cancel: &Cancel,
	window_bytes: u64,
	mut append: impl FnMut(TranscriptRows) -> Result<()>,
) -> Result<()> {
	let workers = std::thread::available_parallelism().map_or(1, NonZeroUsize::get).min(transcripts.len()).max(1);
	// `ends[i]` is the source bytes of transcripts `0..i`.
	let ends: Vec<u64> = std::iter::once(0)
		.chain(transcripts.iter().scan(0u64, |total, transcript| {
			*total += transcript.bytes;
			Some(*total)
		}))
		.collect();
	let now = time::now_ms();
	let next = AtomicUsize::new(0);
	let stop = AtomicBool::new(false);
	// The index `append` waits for, and the signal that it moved or the load stopped.
	let appended = Mutex::new(0usize);
	let moved = Condvar::new();
	let (sender, receiver) = mpsc::channel::<(usize, Result<TranscriptRows>)>();
	std::thread::scope(|scope| {
		for _ in 0..workers {
			let sender = sender.clone();
			let (next, stop, appended, moved, ends) = (&next, &stop, &appended, &moved, &ends);
			scope.spawn(move || {
				loop {
					let index = next.fetch_add(1, Ordering::Relaxed);
					let Some(transcript) = transcripts.get(index) else { return };
					{
						let mut waited = appended.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
						while index > *waited && ends[index + 1] - ends[*waited] > window_bytes && !stop.load(Ordering::Relaxed) {
							waited = moved.wait(waited).unwrap_or_else(std::sync::PoisonError::into_inner);
						}
					}
					if stop.load(Ordering::Relaxed) {
						return;
					}
					let rows = cancel.check().and_then(|()| transcript_rows(cache, transcript, wants_events, now));
					if sender.send((index, rows)).is_err() {
						return;
					}
				}
			});
		}
		std::mem::drop(sender);
		let mut pending: BTreeMap<usize, Result<TranscriptRows>> = BTreeMap::new();
		let mut waiting = 0usize;
		let mut outcome = Ok(());
		'receive: for (index, rows) in &receiver {
			pending.insert(index, rows);
			while let Some(rows) = pending.remove(&waiting) {
				waiting += 1;
				*appended.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = waiting;
				moved.notify_all();
				if let Err(error) = rows.and_then(&mut append) {
					outcome = Err(error);
					break 'receive;
				}
			}
		}
		if outcome.is_err() {
			// Wakes the workers waiting on the window; each ends after its current transcript.
			let _guard = appended.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
			stop.store(true, Ordering::Relaxed);
			moved.notify_all();
		}
		outcome
	})
}

/// Builds the relations `needed` names for `selection`'s sessions. Tables and
/// their appenders open before the load and take each row as it is produced.
/// `src.sessions` reads each root file's head and tail; `src.transcripts` and
/// `src.events` load each transcript once, serving both, in parallel and
/// appended in tree order; nothing loads a session when neither is named.
fn session_relations(duck: &Duck, selection: &Selection, needed: &Needed, context: &Context, cancel: &Cancel) -> Result<()> {
	if needed.is_empty() {
		return Ok(());
	}
	let refs = locate(&selection.sids, context)?;
	// The entries view reads the file list it embeds, so it implies files.
	let wants_files = needed.has("files") || needed.has("entries");
	let wants_tree = wants_files || needed.has("transcripts") || needed.has("events");
	let open = |name: &str, spec: &[(&str, ColumnType)]| -> Result<Option<Table>> {
		if needed.has(name) { Table::create(duck, name, columns(spec), cancel).map(Some) } else { Ok(None) }
	};
	let mut files_table = open("files", &FILES)?;
	let mut sessions_table = open("sessions", &SESSIONS)?;
	let mut transcripts_table = open("transcripts", &TRANSCRIPTS)?;
	let mut events_table = open("events", &EVENTS)?;
	let mut transcripts: Vec<TranscriptRow> = Vec::new();
	for session in &refs {
		cancel.check()?;
		let tree = if wants_tree { ops::transcript_tree(session)? } else { Vec::new() };
		if let Some(table) = sessions_table.as_mut()
			&& let Some(row) = session_row(session)
		{
			table.row(row, cancel)?;
		}
		let take = if selection.agents.unwrap_or(true) { tree.len() } else { tree.len().min(1) };
		for transcript in tree.into_iter().take(take) {
			cancel.check()?;
			if let Some(table) = files_table.as_mut() {
				table.row(
					vec![
						Cell::Str(transcript.sid.clone()),
						Cell::Str(transcript.agent.clone()),
						Cell::text(transcript.parent.clone()),
						Cell::Str(transcript.file.to_string_lossy().into_owned()),
						Cell::Int(transcript.bytes as i64),
						Cell::Str(transcript.modified.clone()),
					],
					cancel,
				)?;
			}
			transcripts.push(transcript);
		}
	}
	if transcripts_table.is_some() || events_table.is_some() {
		let cache = Cache::of(context)?;
		load_in_order(cache.as_ref(), &transcripts, events_table.is_some(), cancel, LOAD_WINDOW_BYTES, |rows| {
			if let Some(table) = transcripts_table.as_mut() {
				table.row(rows.transcript, cancel)?;
			}
			if let Some(table) = events_table.as_mut() {
				table.events(&rows.events, cancel)?;
			}
			Ok(())
		})?;
	}
	for table in [files_table, sessions_table, transcripts_table, events_table].into_iter().flatten() {
		table.close(cancel)?;
	}
	if needed.has("entries") {
		let scans: Vec<String> = if transcripts.is_empty() {
			vec!["SELECT NULL::VARCHAR AS file, NULL::JSON AS json WHERE false".to_owned()]
		} else {
			transcripts.iter().map(entries_scan).collect()
		};
		duck.run(&format!(
			"CREATE VIEW src.entries AS SELECT file, json, json->>'type' AS type, json->>'id' AS id, json->>'parentId' AS parent_id, json->>'timestamp' AS t FROM ({})",
			scans.join(" UNION ALL ")
		))
		.map_err(|error| failed(error, cancel))?;
	}
	Ok(())
}

/// The column type one caller value wants: null votes for nothing.
fn value_type(value: &Value) -> Option<ColumnType> {
	match value {
		Value::Null => None,
		Value::Bool(_) => Some(ColumnType::Boolean),
		// Number.isSafeInteger, else a finite number is DOUBLE.
		Value::Num(number) if number.fract() == 0.0 && number.abs() <= 9_007_199_254_740_991.0 => Some(ColumnType::Bigint),
		Value::Num(number) if number.is_finite() => Some(ColumnType::Double),
		Value::Str(_) | Value::Clipped(_) => Some(ColumnType::Varchar),
		Value::Num(_) | Value::Arr(_) | Value::Obj(_) => Some(ColumnType::Json),
	}
}

/// A caller column's type so far: the type a whole-column pass would pick.
fn merge_type(kind: Option<ColumnType>, value: &Value) -> Option<ColumnType> {
	let Some(wanted) = value_type(value) else { return kind };
	let Some(kind) = kind else { return Some(wanted) };
	if kind == wanted {
		return Some(kind);
	}
	let numeric = |kind: ColumnType| matches!(kind, ColumnType::Bigint | ColumnType::Double);
	Some(if numeric(kind) && numeric(wanted) { ColumnType::Double } else { ColumnType::Json })
}

fn caller_cell(value: Option<&Value>, kind: ColumnType) -> Cell {
	match (value, kind) {
		(None | Some(Value::Null), _) => Cell::Null,
		(Some(value), ColumnType::Json) => Cell::Str(json::stringify(value, None)),
		(Some(Value::Str(text)), _) => Cell::Str(text.clone()),
		(Some(Value::Num(number)), ColumnType::Bigint) => Cell::Int(*number as i64),
		(Some(Value::Num(number)), _) => Cell::Double(*number),
		(Some(Value::Bool(flag)), _) => Cell::Bool(*flag),
		(Some(value), _) => Cell::Str(json::stringify(value, None)),
	}
}

/// `src.<name>` from the caller's rows, its columns in the order their names
/// first appear and each typed by its values.
fn caller_table(duck: &Duck, name: &str, rows: &[Value], cancel: &Cancel) -> Result<()> {
	let mut kinds: Vec<(String, Option<ColumnType>)> = Vec::new();
	let mut objects: Vec<&Obj> = Vec::with_capacity(rows.len());
	for row in rows {
		let Some(row) = row.as_obj() else {
			return Err(EngineError::bad_request(format!("tables.{name}: every row is an object")));
		};
		for (key, value) in row.iter() {
			match kinds.iter_mut().find(|(known, _)| known == key) {
				Some(slot) => slot.1 = merge_type(slot.1, value),
				None => kinds.push((key.to_owned(), merge_type(None, value))),
			}
		}
		objects.push(row);
	}
	if kinds.is_empty() {
		return Err(EngineError::bad_request(format!("tables.{name}: the rows carry no columns")));
	}
	let columns: Vec<Column> =
		kinds.into_iter().map(|(name, kind)| Column { name, kind: kind.unwrap_or(ColumnType::Varchar) }).collect();
	let kinds: Vec<ColumnType> = columns.iter().map(|column| column.kind).collect();
	let names: Vec<String> = columns.iter().map(|column| column.name.clone()).collect();
	let mut table = Table::create(duck, name, columns, cancel)?;
	for row in objects {
		let cells = names.iter().zip(&kinds).map(|(column, kind)| caller_cell(row.get(column), *kind)).collect();
		table.row(cells, cancel)?;
	}
	table.close(cancel)
}

/// The caller tables of a request, checked: `{name: [row, …]}` with each name
/// a SQL identifier outside `SOURCE_RELATIONS` and at least one row.
fn caller_tables(tables: Option<&Value>) -> Result<Vec<(&str, &[Value])>> {
	let Some(tables) = tables else { return Ok(Vec::new()) };
	let Some(tables) = tables.as_obj() else { return Err(EngineError::bad_request("tables: an object of row arrays")) };
	let mut out = Vec::with_capacity(tables.len());
	for (name, rows) in tables.iter() {
		if !TABLE_NAME.is_match(name) {
			return Err(EngineError::bad_request(format!("tables.{name}: a table name is a letter or _ then up to 63 letters, digits or _")));
		}
		if SOURCE_RELATIONS.contains(&name) {
			return Err(EngineError::bad_request(format!("tables.{name}: src.{name} is built from the named sessions; pick another name")));
		}
		match rows.as_arr() {
			Some(rows) if !rows.is_empty() => out.push((name, rows)),
			_ => return Err(EngineError::bad_request(format!("tables.{name}: a non-empty array of rows"))),
		}
	}
	Ok(out)
}

/// Builds schema `src` for one call: only the relations `sql` (and persisted
/// `main` objects) reference, then the caller's tables.
pub fn build(duck: &Duck, selection: Option<&Selection>, tables: Option<&Value>, sql: &str, context: &Context, cancel: &Cancel) -> Result<()> {
	let tables = caller_tables(tables)?;
	duck.run("DROP SCHEMA IF EXISTS src CASCADE; CREATE SCHEMA src").map_err(|error| failed(error, cancel))?;
	if let Some(selection) = selection {
		let definitions = definition_texts(duck).map_err(|error| failed(error, cancel))?;
		let needed = referenced_relations(duck, sql, &definitions);
		session_relations(duck, selection, &needed, context, cancel)?;
	}
	for (name, rows) in tables {
		caller_table(duck, name, rows, cancel)?;
	}
	Ok(())
}

/// Drops schema `src` and everything the call put in it.
pub fn drop(duck: &Duck) {
	// Best effort: a failed drop leaves `src` for the next call's build, which drops it first.
	let _ = duck.run("DROP SCHEMA IF EXISTS src CASCADE");
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn caller_column_types_merge_like_a_whole_column_pass() {
		let int = Value::Num(3.0);
		let double = Value::Num(1.5);
		assert_eq!(merge_type(merge_type(None, &int), &double), Some(ColumnType::Double));
		assert_eq!(merge_type(merge_type(None, &int), &Value::Str("a".into())), Some(ColumnType::Json));
		assert_eq!(merge_type(merge_type(None, &Value::Null), &Value::Bool(true)), Some(ColumnType::Boolean));
		assert_eq!(value_type(&Value::Num(9_007_199_254_740_992.0)), Some(ColumnType::Double));
	}

	#[test]
	fn rows_reach_append_in_list_order_while_workers_wait_on_the_window() {
		let dir = std::env::temp_dir().join(format!("omp-sessions-load-order-{}", std::process::id()));
		std::fs::create_dir_all(&dir).unwrap();
		// Transcript i holds i + 1 user messages on one branch; its listed size
		// makes some workers wait behind a large head and others behind small ones.
		let sizes = [5u64, 1, 1, 4, 1, 1, 1, 6, 1, 2, 1, 1];
		let transcripts: Vec<TranscriptRow> = sizes
			.iter()
			.enumerate()
			.map(|(i, &bytes)| {
				let file = dir.join(format!("A{i}.jsonl"));
				let mut text = String::from("{\"type\":\"session\",\"id\":\"s\",\"timestamp\":\"2026-11-01T10:00:00.000Z\",\"cwd\":\"/w\"}\n");
				for m in 0..=i {
					let parent = if m == 0 { "null".to_owned() } else { format!("\"m{}\"", m - 1) };
					text.push_str(&format!(
						"{{\"type\":\"message\",\"id\":\"m{m}\",\"parentId\":{parent},\"timestamp\":\"2026-11-01T10:00:00.000Z\",\"message\":{{\"role\":\"user\",\"content\":\"hi\"}}}}\n"
					));
				}
				std::fs::write(&file, text).unwrap();
				TranscriptRow { sid: "s".into(), agent: format!("A{i}"), parent: Some(String::new()), file, bytes, modified: String::new() }
			})
			.collect();
		let cancel = Cancel { control: std::sync::Arc::new(super::super::Control::default()), id: 1 };
		let mut seen: Vec<(String, usize)> = Vec::new();
		let outcome = load_in_order(None, &transcripts, true, &cancel, 3, |rows| {
			let Cell::Str(agent) = &rows.transcript[1] else { panic!("agent cell") };
			seen.push((agent.clone(), rows.events.len()));
			Ok(())
		});
		std::fs::remove_dir_all(&dir).unwrap();
		outcome.unwrap();
		let expected: Vec<(String, usize)> = (0..sizes.len()).map(|i| (format!("A{i}"), i + 1)).collect();
		assert_eq!(seen, expected);
	}
}
