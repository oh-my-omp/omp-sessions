//! Renderers: the timeline (column-0 headers, every body line indented two
//! spaces), events JSONL records for jq, and one-entry detail. Pure functions
//! of the loaded session; `base` is the session:// URL the reader used for this
//! session (or subagent), so every pointer printed here can be read back directly.

use std::collections::HashMap;

use crate::events::{Call, CallStatus, EntryDetail, EventKind, LoadedSession, SessionEvent, Text, rewrite_refs};
use crate::filter::{EventFilter, keeps, shows};
use crate::json::{self, Obj, Value, utf16_len, utf16_prefix};
use crate::time;
use crate::window::within;

const IDLE_MS: i64 = 5 * 60_000;
const SLOW_MS: i64 = 10_000;
const ARG_INLINE: usize = 80;
const COMMAND_INLINE: usize = 160;

fn is_path_key(key: &str) -> bool {
	matches!(key, "path" | "paths" | "file" | "cwd" | "workdir" | "url" | "pattern" | "pat" | "glob")
}

fn is_command_key(key: &str) -> bool {
	matches!(key, "command" | "cmd")
}

/// JavaScript's `String.prototype.trim`: Unicode whitespace plus U+FEFF.
fn js_trim(text: &str) -> &str {
	text.trim_matches(|c: char| c.is_whitespace() || c == '\u{FEFF}')
}

/// Header-safe single line: control characters become spaces (`/\p{Cc}+/gu → " "`).
fn flat(value: &str) -> String {
	let mut out = String::with_capacity(value.len());
	let mut run = false;
	for c in value.chars() {
		if c.is_control() {
			run = true;
		} else {
			if run {
				out.push(' ');
				run = false;
			}
			out.push(c);
		}
	}
	if run {
		out.push(' ');
	}
	js_trim(&out).to_string()
}

/// `1 call`, `2 calls`; `many` names an irregular plural (`entries`).
fn plural(count: usize, one: &str, many: &str) -> String {
	format!("{} {}", count, if count == 1 { one } else { many })
}

fn duration(ms: i64) -> String {
	let seconds = (ms as f64 / 1000.0 + 0.5).floor() as i64;
	if seconds < 60 {
		return format!("{seconds}s");
	}
	let minutes = seconds.div_euclid(60);
	if minutes < 60 {
		return format!("{minutes}m{:02}s", seconds % 60);
	}
	format!("{}h{:02}m", minutes.div_euclid(60), minutes % 60)
}

struct DayTime {
	date: String,
	time: String,
}

fn day_time(ms: i64) -> Option<DayTime> {
	let parts = time::local_parts(ms)?;
	Some(DayTime {
		date: format!("{:04}-{:02}-{:02}", parts.year, parts.month, parts.day),
		time: format!("{:02}:{:02}:{:02}", parts.hour, parts.minute, parts.second),
	})
}

/// Local ISO-8601 time with the UTC offset, e.g. `2026-10-26 16:18:30 +08:00`;
/// the input unchanged when it is not a valid timestamp.
pub fn local_stamp(iso: &str) -> String {
	let Some(at) = time::parse_ms(iso) else { return iso.to_string() };
	let Some(parts) = time::local_parts(at) else { return iso.to_string() };
	let Some(stamp) = day_time(at) else { return iso.to_string() };
	let offset = parts.offset_minutes;
	let sign = if offset >= 0 { "+" } else { "-" };
	let abs = offset.unsigned_abs();
	format!("{} {} {}{:02}:{:02}", stamp.date, stamp.time, sign, abs / 60, abs % 60)
}

/// Body lines, each indented two spaces; a clipped text ends with a pointer to the full entry.
fn body(value: &Text, detail: &str) -> Vec<String> {
	let mut lines: Vec<String> = value.shown().split('\n').map(|line| format!("  {line}")).collect();
	if let Text::Clipped(clipped) = value {
		lines.push(format!(
			"  …[clipped: {} of {} chars shown; full: {detail}]",
			utf16_len(&clipped.text),
			clipped.chars
		));
	}
	lines
}

fn scalar_arg(key: &str, value: &str, chars: usize, lines: usize, tool: &str) -> String {
	let first = value.split('\n').next().unwrap_or("");
	if is_path_key(key) {
		return json::stringify(&Value::Str(flat(value)), None);
	}
	let limit = if is_command_key(key) || tool == "bash" { COMMAND_INLINE } else { ARG_INLINE };
	if lines <= 1 && chars <= limit {
		return json::stringify(&Value::Str(value.to_string()), None);
	}
	let shown = if utf16_len(first) > limit {
		format!("{}…", utf16_prefix(first, limit))
	} else {
		first.to_string()
	};
	format!("{}…({}, {} chars)", json::stringify(&Value::Str(shown), None), plural(lines, "line", "lines"), chars)
}

fn item_name(item: &Value) -> Option<&str> {
	match item {
		Value::Str(text) => {
			if utf16_len(text) <= 40 { Some(text) } else { None }
		}
		Value::Obj(obj) => {
			for key in ["name", "id", "path", "title"] {
				if let Some(Value::Str(value)) = obj.get(key) {
					return Some(value);
				}
			}
			None
		}
		_ => None,
	}
}

fn arg_value(key: &str, value: &Value, tool: &str) -> String {
	match value {
		Value::Str(text) => scalar_arg(
			key,
			text,
			utf16_len(text),
			if text.is_empty() { 0 } else { text.split('\n').count() },
			tool,
		),
		Value::Clipped(clipped) => scalar_arg(key, &clipped.text, clipped.chars, clipped.lines, tool),
		Value::Null | Value::Bool(_) | Value::Num(_) => json::stringify(value, None),
		Value::Arr(items) => {
			let names: Vec<&str> = items.iter().filter_map(item_name).collect();
			if names.is_empty() {
				return format!("[{}]", items.len());
			}
			let shown: Vec<String> = names.iter().take(3).map(|name| flat(name)).collect();
			format!(
				"[{}: {}{}]",
				items.len(),
				shown.join(", "),
				if items.len() > shown.len() { ", …" } else { "" }
			)
		}
		Value::Obj(obj) => {
			let keys: Vec<&str> = obj.iter().map(|(key, _)| key).collect();
			let shown = &keys[..keys.len().min(5)];
			format!("{{{}{}}}", shown.join(","), if keys.len() > 5 { ",…" } else { "" })
		}
	}
}

/// Compact call signature: `tool(key=value, …)`, `i` dropped (shown as the intent).
fn signature(call: &Call) -> String {
	match &call.args {
		Value::Obj(args) => {
			let parts: Vec<String> = args
				.iter()
				.filter(|(key, _)| *key != "i")
				.map(|(key, value)| format!("{key}={}", arg_value(key, value, &call.tool)))
				.collect();
			format!("{}({})", call.tool, parts.join(", "))
		}
		Value::Null => format!("{}()", call.tool),
		args => format!("{}({})", call.tool, arg_value("", args, &call.tool)),
	}
}

fn call_tail(call: &Call) -> String {
	let mut parts: Vec<String> = vec![call.status.name().to_string()];
	if let Some(dur) = call.dur_ms
		&& dur >= SLOW_MS
	{
		parts.push(duration(dur));
	}
	if let Some(entry) = &call.result_entry {
		parts.push(format!("#{entry}"));
	}
	let tail = parts.join(" ");
	match &call.intent {
		Some(intent) if !intent.is_empty() => format!("{tail}  // {}", flat(intent)),
		_ => tail,
	}
}

fn ref_of(event: &SessionEvent) -> String {
	match &event.base.block {
		None => format!("#{}", event.base.entry),
		Some(block) => format!("#{}.{}", event.base.entry, block),
	}
}

fn turn_header(turn: u32, events: &[&SessionEvent]) -> String {
	// Tool counts in first-seen order, as a JavaScript Map keeps its keys.
	let mut counts: Vec<(&str, usize)> = Vec::new();
	let mut errors = 0;
	for event in events {
		let Some(call) = event.kind.call() else { continue };
		match counts.iter_mut().find(|(tool, _)| *tool == call.tool) {
			Some((_, count)) => *count += 1,
			None => counts.push((call.tool.as_str(), 1)),
		}
		if call.status == CallStatus::Error {
			errors += 1;
		}
	}
	let mut parts: Vec<String> = vec![if turn == 0 { "── preamble".to_string() } else { format!("── turn {turn}") }];
	if let (Some(first), Some(last)) = (events.first(), events.last())
		&& let (Some(start_ms), Some(end_ms)) = (time::parse_ms(&first.base.t), time::parse_ms(&last.base.t))
		&& let (Some(start), Some(end)) = (day_time(start_ms), day_time(end_ms))
	{
		parts.push(format!("{} {}→{} ({})", start.date, start.time, end.time, duration(end_ms - start_ms)));
	}
	let total: usize = counts.iter().map(|(_, count)| *count).sum();
	if total > 0 {
		// Stable sort by count descending, JavaScript's Array.prototype.sort.
		counts.sort_by_key(|(_, count)| std::cmp::Reverse(*count));
		let by_tool: Vec<String> = counts.iter().map(|(tool, count)| format!("{tool}×{count}")).collect();
		parts.push(format!("{} ({})", plural(total, "call", "calls"), by_tool.join(" ")));
	}
	if errors > 0 {
		parts.push(plural(errors, "error", "errors"));
	}
	parts.join(" · ")
}

/// Lines of one non-call event.
fn event_lines(event: &SessionEvent, time: &str, base: &str, root: &str) -> Vec<String> {
	let detail = format!("{base}/entry/{}", event.base.entry);
	match &event.kind {
		EventKind::User { text, injected } => {
			let mut lines = vec![format!(
				"{time} user {}{}",
				ref_of(event),
				if *injected { " (agent-attributed: an extension or the parent agent)" } else { "" }
			)];
			lines.extend(body(text, &detail));
			lines
		}
		EventKind::Developer { text } | EventKind::Thinking { text } | EventKind::Say { text } => {
			let mut lines = vec![format!("{time} {} {}", event.kind.name(), ref_of(event))];
			lines.extend(body(text, &detail));
			lines
		}
		EventKind::IrcOut { to, text, call } => {
			let mut lines = vec![format!("{time} irc me → {to} {} {}", ref_of(event), call_tail(call))];
			lines.extend(body(text, &detail));
			lines
		}
		EventKind::IrcIn { from, to, text } => {
			let mut lines = vec![format!("{time} irc {} → {} {}", flat(from), flat(to), ref_of(event))];
			lines.extend(body(text, &detail));
			lines
		}
		EventKind::Dispatch { call, context, children } => {
			let mut agents: Vec<&str> = Vec::new();
			for child in children {
				if !agents.contains(&child.agent.as_str()) {
					agents.push(child.agent.as_str());
				}
			}
			let mut lines = vec![format!(
				"{time} dispatch {} → {} ({}) {}",
				ref_of(event),
				plural(children.len(), "agent", "agents"),
				agents.join(", "),
				call_tail(call)
			)];
			if let Some(context) = context {
				lines.push("  context:".to_string());
				lines.extend(body(context, &detail).iter().map(|line| format!("  {line}")));
			}
			for child in children {
				lines.push(format!("  {} ({}):", flat(&child.name), flat(&child.agent)));
				lines.extend(body(&child.task, &detail).iter().map(|line| format!("  {line}")));
			}
			lines
		}
		EventKind::Subresult { id, agent, status, duration: span, text } => {
			let mut lines = vec![format!(
				"{time} result {} · {} · {} · {} {}",
				flat(id),
				flat(agent),
				flat(status),
				flat(span),
				ref_of(event)
			)];
			lines.extend(body(text, &detail));
			lines.push(format!("  → {root}/agents/{id}"));
			lines
		}
		EventKind::Exec { lang, source, status } => {
			let mut lines = vec![format!("{time} exec {}! {} {}", lang.name(), ref_of(event), flat(status))];
			lines.extend(body(source, &detail));
			lines
		}
		EventKind::Marker { what, detail: marker_detail } => vec![format!(
			"{time} {} {}{}",
			flat(what),
			ref_of(event),
			if marker_detail.is_empty() { String::new() } else { format!(" · {}", flat(marker_detail)) }
		)],
		EventKind::Call { call } => {
			// Calls are printed by render_timeline under their `calls #entry`
			// header; kept here for completeness.
			vec![format!("  {} {}", signature(call), call_tail(call))]
		}
	}
}

/// The `filter …` header line, or None when nothing is filtered. The window is
/// printed as a half-open interval of local stamps: `time [14:30:00 …, 15:00:00 …)`.
fn filter_line(events: &[SessionEvent], filter: &EventFilter) -> Option<String> {
	if filter.types.is_none() && filter.window.is_none() {
		return None;
	}
	let mut parts = vec!["filter".to_string()];
	if let Some(types) = &filter.types {
		parts.push(format!("type={}", types.source));
	}
	if let Some(window) = &filter.window {
		let stamp = |at: Option<i64>| at.map_or_else(|| "…".to_string(), |ms| local_stamp(&time::iso_utc(ms)));
		parts.push(format!("time [{}, {})", stamp(window.start), stamp(window.end)));
	}
	let mut summary = vec![
		parts.join(" "),
		format!("{} of {} shown", events.iter().filter(|event| shows(filter, event)).count(), plural(events.len(), "event", "events")),
	];
	let untimed = if filter.window.is_none() {
		0
	} else {
		events.iter().filter(|event| time::parse_ms(&event.base.t).is_none()).count()
	};
	if untimed > 0 {
		summary.push(format!("{untimed} without a timestamp"));
	}
	Some(summary.join(" · "))
}

/// The timeline. `root` is the root session's canonical URL (subagent results
/// point under it); `title` is the title from the session's header scan, else
/// the journal's, else the first prompt.
pub fn render_timeline(loaded: &LoadedSession, base: &str, root: &str, title: Option<&str>, filter: &EventFilter) -> String {
	let meta = &loaded.meta;
	let events = &loaded.events;
	let first_prompt = events.iter().find(|event| matches!(event.kind, EventKind::User { .. }));
	let fallback = first_prompt.and_then(|event| match &event.kind {
		EventKind::User { text, .. } => Some(utf16_prefix(text.shown().split('\n').next().unwrap_or(""), 120).to_string()),
		_ => None,
	});
	let mut lines: Vec<String> = vec![
		format!(
			"session {}{} · {}",
			meta.sid,
			if meta.agent.is_empty() { String::new() } else { format!(" · agent {}", meta.agent) },
			meta.cwd
		),
		format!("title {}", flat(title.or(meta.title.as_deref()).or(fallback.as_deref()).unwrap_or("(untitled)"))),
		format!(
			"start {} · {} · {} on the active branch (leaf #{})",
			local_stamp(&meta.created),
			plural(meta.entries, "entry", "entries"),
			meta.on_branch,
			meta.leaf.as_deref().unwrap_or("none")
		),
		format!("details {base}/entry/<id> · events {base}/events.jsonl · raw {base}/raw.jsonl"),
	];
	if !meta.stats.invalid_lines.is_empty() {
		lines.push(format!(
			"warning {} skipped: {}",
			plural(meta.stats.invalid_lines.len(), "unparsable line", "unparsable lines"),
			meta.stats.invalid_lines.iter().take(20).map(u64::to_string).collect::<Vec<_>>().join(", ")
		));
	}
	if let Some(tail) = meta.stats.incomplete_tail {
		lines.push(format!("warning line {tail} is still being written and is not shown"));
	}
	if let Some(filtered) = filter_line(events, filter) {
		lines.push(filtered);
	}
	lines.push(String::new());

	// Turns in first-seen order, as a JavaScript Map keeps its keys.
	let mut order: Vec<u32> = Vec::new();
	let mut turns: HashMap<u32, Vec<&SessionEvent>> = HashMap::new();
	for event in events {
		match turns.get_mut(&event.base.turn) {
			Some(list) => list.push(event),
			None => {
				order.push(event.base.turn);
				turns.insert(event.base.turn, vec![event]);
			}
		}
	}
	let mut previous_at: Option<i64> = None;
	let mut shown_date: Option<String> = None;
	let mut gap: i64 = 0;
	for turn in &order {
		let turn_events = &turns[turn];
		let mut header_done = false;
		let mut calls_entry: Option<&str> = None;
		for event in turn_events {
			if let Some(window) = &filter.window
				&& !within(window, &event.base.t)
			{
				previous_at = None;
				gap = 0;
				continue;
			}
			let at = time::parse_ms(&event.base.t);
			if let (Some(previous), Some(at)) = (previous_at, at) {
				gap = gap.max(at - previous);
			}
			if at.is_some() {
				previous_at = at;
			}
			if let Some(types) = &filter.types
				&& !keeps(types, event)
			{
				continue;
			}
			if !header_done {
				lines.push(turn_header(*turn, turn_events));
				header_done = true;
			}
			let parts = at.and_then(day_time);
			if gap >= IDLE_MS {
				lines.push(format!("── idle {} ──", duration(gap)));
			}
			gap = 0;
			if let Some(parts) = &parts {
				if shown_date.as_ref().is_some_and(|shown| *shown != parts.date) {
					lines.push(format!("── {} ──", parts.date));
				}
				shown_date = Some(parts.date.clone());
			}
			let stamp = parts.as_ref().map_or_else(|| "--:--:--".to_string(), |parts| parts.time.clone());
			if let EventKind::Call { call } = &event.kind {
				if calls_entry != Some(event.base.entry.as_str()) {
					lines.push(format!("{stamp} calls #{}", event.base.entry));
				}
				calls_entry = Some(event.base.entry.as_str());
				lines.push(format!("  {} {}", signature(call), call_tail(call)));
				continue;
			}
			calls_entry = None;
			lines.extend(event_lines(event, &stamp, base, root));
		}
	}
	format!("{}\n", lines.join("\n"))
}

/// Plain JSON value of a projection; a clipped string becomes its prefix plus a clip marker.
fn to_json(value: &Value) -> Value {
	match value {
		Value::Clipped(clipped) => Value::Str(clipped.marked()),
		Value::Arr(items) => Value::Arr(items.iter().map(to_json).collect()),
		Value::Obj(obj) => {
			let mut out = Obj::new();
			for (key, item) in obj.iter() {
				out.insert(key, to_json(item));
			}
			Value::Obj(out)
		}
		other => other.clone(),
	}
}

fn text_json(text: &Text) -> Value {
	match text {
		Text::Plain(value) => Value::Str(value.clone()),
		Text::Clipped(clipped) => Value::Str(clipped.marked()),
	}
}

fn opt_string(value: &Option<String>) -> Value {
	value.as_ref().map_or(Value::Null, |text| Value::Str(text.clone()))
}

fn num_or_null(value: Option<f64>) -> Value {
	value.map_or(Value::Null, Value::Num)
}

fn call_fields(call: &Call) -> Vec<(&'static str, Value)> {
	vec![
		("callId", Value::Str(call.id.clone())),
		("tool", Value::Str(call.tool.clone())),
		("args", to_json(&call.args)),
		("intent", opt_string(&call.intent)),
		("status", Value::Str(call.status.name().to_string())),
		("resultEntry", opt_string(&call.result_entry)),
		("resultChars", num_or_null(call.result_chars.map(|value| value as f64))),
		("durMs", num_or_null(call.dur_ms.map(|value| value as f64))),
	]
}

fn event_fields(event: &SessionEvent) -> Vec<(&'static str, Value)> {
	match &event.kind {
		EventKind::User { text, injected } => {
			vec![("text", text_json(text)), ("injected", Value::Bool(*injected))]
		}
		EventKind::Developer { text } | EventKind::Thinking { text } | EventKind::Say { text } => {
			vec![("text", text_json(text))]
		}
		EventKind::Call { call } => call_fields(call),
		EventKind::IrcOut { to, text, call } => {
			let mut fields = call_fields(call);
			fields.push(("to", Value::Str(to.clone())));
			fields.push(("text", text_json(text)));
			fields
		}
		EventKind::Dispatch { call, context, children } => {
			let mut fields = call_fields(call);
			fields.push(("context", context.as_ref().map_or(Value::Null, text_json)));
			fields.push((
				"children",
				Value::Arr(
					children
						.iter()
						.map(|child| {
							let mut obj = Obj::new();
							obj.insert("name", Value::Str(child.name.clone()));
							obj.insert("agent", Value::Str(child.agent.clone()));
							obj.insert("task", text_json(&child.task));
							Value::Obj(obj)
						})
						.collect(),
				),
			));
			fields
		}
		EventKind::IrcIn { from, to, text } => {
			vec![("from", Value::Str(from.clone())), ("to", Value::Str(to.clone())), ("text", text_json(text))]
		}
		EventKind::Subresult { id, agent, status, duration: span, text } => vec![
			("id", Value::Str(id.clone())),
			("agent", Value::Str(agent.clone())),
			("status", Value::Str(status.clone())),
			("duration", Value::Str(span.clone())),
			("text", text_json(text)),
		],
		EventKind::Exec { lang, source, status } => vec![
			("lang", Value::Str(lang.name().to_string())),
			("source", text_json(source)),
			("status", Value::Str(status.clone())),
		],
		EventKind::Marker { what, detail } => {
			vec![("what", Value::Str(what.clone())), ("detail", Value::Str(detail.clone()))]
		}
	}
}

/// One JSON object per event that `filter` shows, in the field order events.jsonl writes.
pub fn event_records(loaded: &LoadedSession, base: &str, filter: &EventFilter) -> Vec<Value> {
	let meta = &loaded.meta;
	let mut out = Vec::new();
	for event in &loaded.events {
		if !shows(filter, event) {
			continue;
		}
		let mut record = Obj::new();
		record.insert("sid", Value::Str(meta.sid.clone()));
		record.insert("agent", Value::Str(meta.agent.clone()));
		record.insert("turn", Value::Num(f64::from(event.base.turn)));
		record.insert("t", Value::Str(event.base.t.clone()));
		record.insert("kind", Value::Str(event.kind.name().to_string()));
		record.insert("entry", Value::Str(event.base.entry.clone()));
		record.insert("block", event.base.block.map_or(Value::Null, |block| Value::Num(block as f64)));
		record.insert("line", Value::Num(event.base.line as f64));
		for (key, value) in event_fields(event) {
			record.insert(key, value);
		}
		record.insert("url", Value::Str(format!("{base}/entry/{}", event.base.entry)));
		out.push(Value::Obj(record));
	}
	out
}

/// `String(x)` of a value that already went through `to_json` (no Clipped):
/// strings as is, numbers as `Number.prototype.toString`, booleans, `null`,
/// objects `[object Object]`, arrays joined on `,`.
fn js_string(value: &Value) -> String {
	match value {
		Value::Null => "null".to_string(),
		Value::Bool(value) => value.to_string(),
		Value::Num(number) => {
			let mut out = String::new();
			json::write_number(&mut out, *number);
			out
		}
		Value::Str(text) => text.clone(),
		Value::Clipped(clipped) => clipped.marked(),
		Value::Arr(items) => items.iter().map(js_string).collect::<Vec<_>>().join(","),
		Value::Obj(_) => "[object Object]".to_string(),
	}
}

fn plain_obj(value: &Value) -> Option<&Obj> {
	match value {
		Value::Obj(obj) => Some(obj),
		_ => None,
	}
}

fn content_text(value: Option<&Value>) -> Option<String> {
	match value? {
		Value::Str(text) => Some(text.clone()),
		Value::Clipped(clipped) => Some(clipped.marked()),
		Value::Arr(blocks) => {
			let mut parts: Vec<String> = Vec::with_capacity(blocks.len());
			for block in blocks {
				parts.push(match block {
					Value::Obj(obj) => {
						if obj.get("type").is_some_and(|kind| matches!(kind, Value::Str(name) if name == "image")) {
							"[image]".to_string()
						} else {
							match obj.get("text") {
								Some(Value::Str(text)) => text.clone(),
								Some(Value::Clipped(clipped)) => clipped.marked(),
								_ => String::new(),
							}
						}
					}
					_ => String::new(),
				});
			}
			Some(parts.join("\n"))
		}
		_ => None,
	}
}

/// JavaScript truthiness.
fn truthy(value: &Value) -> bool {
	match value {
		Value::Null => false,
		Value::Bool(value) => *value,
		Value::Num(number) => *number != 0.0 && !number.is_nan(),
		Value::Str(text) => !text.is_empty(),
		Value::Clipped(_) | Value::Arr(_) | Value::Obj(_) => true,
	}
}

/// One entry in full: a tool result shows its call (arguments, intent, timing) and the whole result text.
pub fn render_entry(detail: &EntryDetail, base: &str, sid: &str) -> String {
	let fields = plain_obj(&detail.record);
	let message = fields.and_then(|fields| fields.get("message"));
	let message_fields = message.and_then(plain_obj);
	let null = Value::Null;
	let mut lines = vec![format!(
		"entry #{} · line {} · {} · raw {}/raw.jsonl:{}",
		js_string(&to_json(fields.and_then(|fields| fields.get("id")).unwrap_or(&null))),
		detail.line,
		js_string(&to_json(fields.and_then(|fields| fields.get("type")).unwrap_or(&null))),
		base,
		detail.line
	)];
	if message_fields
		.and_then(|fields| fields.get("role"))
		.is_some_and(|role| matches!(role, Value::Str(name) if name == "toolResult"))
	{
		lines.push(format!(
			"tool {} · {} · call {}",
			js_string(&to_json(message_fields.and_then(|fields| fields.get("toolName")).unwrap_or(&null))),
			if message_fields
				.and_then(|fields| fields.get("isError"))
				.is_some_and(|error| matches!(error, Value::Bool(true)))
			{
				"error"
			} else {
				"ok"
			},
			js_string(&to_json(message_fields.and_then(|fields| fields.get("toolCallId")).unwrap_or(&null)))
		));
		let start_data = detail.start.as_ref().and_then(plain_obj).and_then(|start| start.get("data"));
		if let Some(data) = start_data
			&& truthy(data)
		{
			lines.push(format!("start {}", json::stringify(&to_json(data), None)));
		}
		lines.push(String::new());
		lines.push("arguments:".to_string());
		lines.push(json::stringify(&to_json(detail.call.as_ref().unwrap_or(&null)), Some("  ")));
		lines.push(String::new());
		lines.push("result:".to_string());
		lines.push(rewrite_refs(
			&content_text(message_fields.and_then(|fields| fields.get("content"))).unwrap_or_default(),
			sid,
		));
		return format!("{}\n", lines.join("\n"));
	}
	lines.push(String::new());
	lines.push(json::stringify(&to_json(&detail.record), Some("  ")));
	format!("{}\n", lines.join("\n"))
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::events::{CallStatus, EventBase, SessionMeta, Text};
	use crate::filter::{EventFilter, parse_type_filter};
	use crate::json::Clipped;
	use crate::scan::ScanStats;

	fn base(turn: u32, entry: &str, t: &str, line: u64) -> EventBase {
		EventBase { entry: entry.to_string(), block: None, t: t.to_string(), line, turn }
	}

	fn event(turn: u32, entry: &str, t: &str, line: u64, kind: EventKind) -> SessionEvent {
		SessionEvent { base: base(turn, entry, t, line), kind }
	}

	fn call(tool: &str, args: Value, status: CallStatus) -> Call {
		Call {
			id: "c1".to_string(),
			tool: tool.to_string(),
			args,
			intent: None,
			status,
			result_entry: None,
			result_chars: None,
			dur_ms: None,
		}
	}

	fn obj(pairs: &[(&str, Value)]) -> Value {
		let mut out = Obj::new();
		for (key, value) in pairs {
			out.insert(*key, value.clone());
		}
		Value::Obj(out)
	}

	fn loaded(events: Vec<SessionEvent>) -> LoadedSession {
		LoadedSession {
			meta: SessionMeta {
				sid: "s1".to_string(),
				agent: String::new(),
				file: std::path::PathBuf::from("/tmp/s.jsonl"),
				cwd: "/work".to_string(),
				title: None,
				created: "2026-10-28T08:00:00".to_string(),
				parent_session: None,
				leaf: Some("e2".to_string()),
				entries: 4,
				on_branch: 3,
				stats: ScanStats::default(),
			},
			events,
		}
	}

	fn no_filter() -> EventFilter {
		EventFilter { types: None, window: None }
	}

	#[test]
	fn duration_formats() {
		assert_eq!(duration(0), "0s");
		assert_eq!(duration(59_499), "59s");
		assert_eq!(duration(59_500), "1m00s");
		assert_eq!(duration(60_000), "1m00s");
		assert_eq!(duration(3_599_000), "59m59s");
		assert_eq!(duration(3_600_000), "1h00m");
		assert_eq!(duration(7_322_000), "2h02m");
	}

	#[test]
	fn signature_shapes_args() {
		let sig = signature(&call("read", obj(&[("path", Value::Str("/a/b\nc".to_string())), ("i", Value::Str("x".to_string()))]), CallStatus::Ok));
		assert_eq!(sig, "read(path=\"/a/b c\")");
		// A command key gets the longer inline cap.
		let long = "x".repeat(150);
		let sig = signature(&call("exec", obj(&[("command", Value::Str(long.clone()))]), CallStatus::Ok));
		assert_eq!(sig, format!("exec(command=\"{long}\")"));
		let sig = signature(&call("exec", obj(&[("command", Value::Str("x".repeat(170)))]), CallStatus::Ok));
		assert!(sig.contains("…(1 line, 170 chars)"), "{sig}");
		// Long multi-line value under a non-command key.
		let sig = signature(&call("write", obj(&[("content", Value::Str("line1\nline2".to_string()))]), CallStatus::Ok));
		assert_eq!(sig, "write(content=\"line1\"…(2 lines, 11 chars))");
		// Null args print bare parens.
		assert_eq!(signature(&call("noop", Value::Null, CallStatus::Ok)), "noop()");
	}

	#[test]
	fn idle_and_date_lines_appear() {
		let session = loaded(vec![
			event(1, "e1", "2026-10-28T10:00:00", 1, EventKind::User { text: Text::Plain("hi".to_string()), injected: false }),
			event(1, "e2", "2026-10-28T10:07:00", 2, EventKind::Say { text: Text::Plain("ok".to_string()) }),
			event(2, "e3", "2026-10-29T09:00:00", 3, EventKind::Say { text: Text::Plain("next".to_string()) }),
		]);
		let out = render_timeline(&session, "session://s1", "session://s1", None, &no_filter());
		assert!(out.contains("── turn 1"), "{out}");
		assert!(out.contains("── idle 7m00s ──"), "{out}");
		assert!(out.contains("── 2026-10-29 ──"), "{out}");
		assert!(out.contains("title hi"), "{out}");
	}

	#[test]
	fn calls_group_under_one_header() {
		let session = loaded(vec![
			event(1, "e1", "2026-10-28T10:00:00", 1, EventKind::User { text: Text::Plain("go".to_string()), injected: false }),
			event(1, "e2", "2026-10-28T10:01:00", 2, EventKind::Call { call: call("read", Value::Null, CallStatus::Ok) }),
			event(1, "e2", "2026-10-28T10:01:01", 2, EventKind::Call { call: call("write", Value::Null, CallStatus::Ok) }),
		]);
		let out = render_timeline(&session, "session://s1", "session://s1", None, &no_filter());
		assert_eq!(out.matches("calls #e2").count(), 1, "{out}");
		assert!(out.contains("read() ok"), "{out}");
		assert!(out.contains("write() ok"), "{out}");
	}

	#[test]
	fn event_records_key_order_and_filter() {
		let session = loaded(vec![
			event(1, "e1", "2026-10-28T10:00:00", 1, EventKind::User { text: Text::Plain("hi".to_string()), injected: false }),
			event(1, "e2", "2026-10-28T10:01:00", 2, EventKind::Say { text: Text::Clipped(Clipped { text: "abc".to_string(), chars: 100, lines: 3 }) }),
		]);
		let types = parse_type_filter(&["say"]).unwrap();
		let filter = EventFilter { types, window: None };
		let records = event_records(&session, "session://s1", &filter);
		assert_eq!(records.len(), 1);
		let text = json::stringify(&records[0], None);
		assert!(text.starts_with("{\"sid\":\"s1\",\"agent\":\"\",\"turn\":1,"), "{text}");
		assert!(text.contains("\"text\":\"abc…[clipped: 100 chars total]\""), "{text}");
		assert!(text.ends_with(",\"url\":\"session://s1/entry/e2\"}"), "{text}");
	}

	#[test]
	fn render_entry_plain_record() {
		// The toolResult branch goes through events::rewrite_refs (another
		// slice); this covers the common record-dump path.
		let record = obj(&[
			("id", Value::Str("e1".to_string())),
			("type", Value::Str("summary".to_string())),
			("leafId", Value::Str("e0".to_string())),
		]);
		let detail = EntryDetail { line: 7, record, call: None, start: None };
		let out = render_entry(&detail, "session://s1", "s1");
		assert!(out.starts_with("entry #e1 · line 7 · summary · raw session://s1/raw.jsonl:7\n"), "{out}");
		assert!(out.contains("\n{\n  \"id\": \"e1\","), "{out}");
	}

	#[test]
	fn content_text_blocks() {
		let text_block = obj(&[("type", Value::Str("text".to_string())), ("text", Value::Str("file body".to_string()))]);
		let image_block = obj(&[("type", Value::Str("image".to_string()))]);
		let clipped_block =
			obj(&[("type", Value::Str("text".to_string())), ("text", Value::Clipped(Clipped { text: "abc".to_string(), chars: 9, lines: 1 }))]);
		let content = Value::Arr(vec![text_block, image_block, clipped_block, Value::Num(1.0)]);
		assert_eq!(content_text(Some(&content)).as_deref(), Some("file body\n[image]\nabc…[clipped: 9 chars total]\n"));
		assert_eq!(content_text(Some(&Value::Str("plain".to_string()))).as_deref(), Some("plain"));
		assert_eq!(content_text(Some(&Value::Num(3.0))), None);
		assert_eq!(content_text(None), None);
	}
}
