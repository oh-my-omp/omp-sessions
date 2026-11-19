//! `?type=` filter over timeline events. Accepted forms, freely combined:
//!   type=thinking             one event kind (the timeline header labels: calls, result … are aliases)
//!   type=thinking,say         a comma list;  type=say&type=irc   repeated parameters
//!   type=irc | speech | ai    a group of kinds
//!   type=call:bash            events carrying a call of that tool (plain calls, irc_out = write, dispatch = task)
//!   type=marker:compaction    markers of one kind (`-`, `_` and spaces are interchangeable)
//!   type=-call  type=!marker  exclusion; with no inclusion everything else is kept
//! Matching is case-insensitive. Unknown tokens are a `bad_request` listing the
//! accepted forms. [`EventFilter`] ANDs this filter with the time window;
//! [`load_filtered`] applies both before events are built in full.

use std::collections::HashSet;
use std::path::Path;

use crate::error::{EngineError, Result};
use crate::events::{self, Build, EventKind, LoadedSession, SessionEvent};
use crate::window::{self, TimeWindow, WindowSpec};

const KINDS: [&str; 11] = [
	"user", "developer", "thinking", "say", "call", "irc_out", "irc_in", "dispatch", "subresult", "exec", "marker",
];

const ALIASES: [(&str, &str); 6] = [
	("calls", "call"),
	("result", "subresult"),
	("results", "subresult"),
	("markers", "marker"),
	("irc_send", "irc_out"),
	("irc_receive", "irc_in"),
];

const GROUPS: [(&str, &[&str]); 5] = [
	("irc", &["irc_in", "irc_out"]),
	("subagent", &["dispatch", "subresult"]),
	("speech", &["user", "developer", "say", "irc_in", "irc_out", "dispatch", "subresult"]),
	("ai", &["thinking", "say", "irc_out", "dispatch"]),
	("tools", &["call", "exec"]),
];

#[derive(Clone, Debug, PartialEq)]
enum Selector {
	Kind { kinds: &'static [&'static str] },
	Tool { tool: String },
	Marker { what: String },
}

#[derive(Clone, Debug, PartialEq)]
pub struct TypeFilter {
	include: Vec<Selector>,
	exclude: Vec<Selector>,
	/// The filter as written, for headers and continuation hints.
	pub source: String,
}

/// Everything a URL filters events by: its type filter and its time window, combined by AND.
#[derive(Clone, Debug, PartialEq)]
pub struct EventFilter {
	pub types: Option<TypeFilter>,
	pub window: Option<TimeWindow>,
}

/// What a filtered read prints: `Timeline` needs every event's structure and
/// the first prompt's text; `Records` needs only the events the filter shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FilteredRead {
	Timeline,
	Records,
}

/// JavaScript's `String.prototype.trim`: Unicode whitespace plus U+FEFF.
fn js_trim(text: &str) -> &str {
	text.trim_matches(|c: char| c.is_whitespace() || c == '\u{FEFF}')
}

/// `-`, `_` and spaces compare equal; case folded (`/[-_\s]+/g → " "`, with
/// JavaScript's `\s` set, which also covers U+FEFF).
fn fold(value: &str) -> String {
	let mut out = String::with_capacity(value.len());
	let mut run = false;
	for c in js_trim(value).to_lowercase().chars() {
		if c == '-' || c == '_' || c == '\u{FEFF}' || c.is_whitespace() {
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
	out
}

fn usage(token: &str) -> EngineError {
	EngineError::bad_request(format!(
		"Unknown type \"{token}\". Use a kind ({}), a group ({}), \
		 call:<tool>, marker:<what>, comma lists or repeated type=, and -<type> / !<type> to exclude.",
		KINDS.join(", "),
		GROUPS.iter().map(|(name, _)| *name).collect::<Vec<_>>().join(", ")
	))
}

fn selector(token: &str) -> Result<Selector> {
	if let Some(colon) = token.find(':') {
		let head = fold(&token[..colon]);
		let rest = js_trim(&token[colon + 1..]);
		if rest.is_empty() {
			return Err(usage(token));
		}
		if head == "call" || head == "calls" || head == "tool" {
			return Ok(Selector::Tool { tool: rest.to_lowercase() });
		}
		if head == "marker" || head == "markers" {
			return Ok(Selector::Marker { what: fold(rest) });
		}
		return Err(usage(token));
	}
	let name = fold(token).replace(' ', "_");
	if let Some(at) = KINDS.iter().position(|kind| *kind == name) {
		return Ok(Selector::Kind { kinds: &KINDS[at..at + 1] });
	}
	for (alias, kind) in &ALIASES {
		if *alias == name {
			let at = KINDS.iter().position(|candidate| candidate == kind).unwrap_or(0);
			return Ok(Selector::Kind { kinds: &KINDS[at..at + 1] });
		}
	}
	for (group, kinds) in &GROUPS {
		if *group == name {
			return Ok(Selector::Kind { kinds });
		}
	}
	Err(usage(token))
}

/// Parses every `type` value of a query; None when none is given (or all are empty).
pub fn parse_type_filter(values: &[&str]) -> Result<Option<TypeFilter>> {
	let mut include: Vec<Selector> = Vec::new();
	let mut exclude: Vec<Selector> = Vec::new();
	let mut written: Vec<&str> = Vec::new();
	for raw in values.iter().flat_map(|value| value.split(',')) {
		let token = js_trim(raw);
		if token.is_empty() {
			continue;
		}
		written.push(token);
		let negated = token.starts_with('-') || token.starts_with('!');
		let item = selector(if negated { &token[1..] } else { token })?;
		if negated {
			exclude.push(item);
		} else {
			include.push(item);
		}
	}
	if written.is_empty() {
		return Ok(None);
	}
	Ok(Some(TypeFilter { include, exclude, source: written.join(",") }))
}

fn matches(selector: &Selector, event: &SessionEvent) -> bool {
	match selector {
		Selector::Kind { kinds } => kinds.contains(&event.kind.name()),
		Selector::Tool { tool } => match event.kind.call() {
			Some(call) => call.tool.to_lowercase() == *tool,
			None => false,
		},
		Selector::Marker { what } => match &event.kind {
			EventKind::Marker { what: event_what, .. } => fold(event_what) == *what,
			_ => false,
		},
	}
}

/// Whether `event` passes: any inclusion (or none given), and no exclusion.
pub fn keeps(filter: &TypeFilter, event: &SessionEvent) -> bool {
	if !filter.include.is_empty() && !filter.include.iter().any(|item| matches(item, event)) {
		return false;
	}
	!filter.exclude.iter().any(|item| matches(item, event))
}

/// Whether `event` passes both the type filter and the time window.
pub fn shows(filter: &EventFilter, event: &SessionEvent) -> bool {
	filter.types.as_ref().is_none_or(|types| keeps(types, event))
		&& filter.window.is_none_or(|window| window::within(&window, &event.base.t))
}

/// Loads a session for a read filtered by `types` and the written window
/// `spec` (resolved against the session and `now_ms`). When the read or the
/// filter needs it — `Timeline` (turn spans, counts, the filter line), any
/// `types`, or a clock bound for `sessionDay` — a LIGHT outline of every
/// branch entry is built first, then `Full` for the entries that print (and,
/// for `Timeline`, the first prompt that labels an untitled session). The
/// loaded events render exactly as the fully built session filtered
/// afterwards would. Without a filter this is `load_session`.
pub fn load_filtered(
	file: &Path,
	sid: &str,
	agent: &str,
	types: Option<TypeFilter>,
	spec: Option<WindowSpec>,
	now_ms: i64,
	read: FilteredRead,
) -> Result<(LoadedSession, EventFilter)> {
	let index = events::index_session(file, sid, agent)?;
	if types.is_none() && spec.is_none() {
		let events = events::build_events(&index, &|_| Build::Full)?;
		let loaded = LoadedSession { meta: index.meta, events };
		return Ok((loaded, EventFilter { types: None, window: None }));
	}
	// A bare clock time takes its date from the session's events.
	let clock = spec.as_ref().is_some_and(WindowSpec::has_clock);
	let outline = if read == FilteredRead::Timeline || types.is_some() || clock {
		Some(events::build_events(&index, &|_| Build::Light)?)
	} else {
		None
	};
	let resolved = match &spec {
		None => None,
		Some(spec) => {
			let mut timestamps: Box<dyn Iterator<Item = &str>> = match &outline {
				Some(events) => Box::new(events.iter().map(|event| event.base.t.as_str())),
				None => Box::new(std::iter::empty()),
			};
			Some(window::resolve_window(spec, &mut *timestamps, now_ms)?)
		}
	};
	let filter = EventFilter { types, window: resolved };
	let mut printed: HashSet<u64> = HashSet::new();
	match &outline {
		None => {
			for entry in &index.branch {
				if filter.window.is_none_or(|window| window::within(&window, &entry.t)) {
					printed.insert(entry.line);
				}
			}
		}
		Some(events) => {
			for event in events {
				if shows(&filter, event) {
					printed.insert(event.base.line);
				}
			}
			if read == FilteredRead::Timeline
				&& let Some(prompt) = events.iter().find(|event| matches!(event.kind, EventKind::User { .. }))
			{
				printed.insert(prompt.base.line);
			}
		}
	}
	let full = events::build_events(&index, &|entry| {
		if printed.contains(&entry.line) { Build::Full } else { Build::Skip }
	})?;
	if outline.is_none() || read == FilteredRead::Records {
		let loaded = LoadedSession { meta: index.meta, events: full };
		return Ok((loaded, filter));
	}
	let outline = outline.unwrap_or_default();
	// An entry's events come out of LIGHT and FULL builds alike, so FULL ones
	// replace LIGHT ones entry by entry.
	let mut by_line: std::collections::HashMap<u64, std::collections::VecDeque<SessionEvent>> =
		std::collections::HashMap::new();
	for event in full {
		by_line.entry(event.base.line).or_default().push_back(event);
	}
	let mut events: Vec<SessionEvent> = Vec::new();
	for event in outline {
		match by_line.get_mut(&event.base.line) {
			Some(built) if !built.is_empty() => events.extend(built.drain(..)),
			Some(_) => {}
			None => events.push(event),
		}
	}
	let loaded = LoadedSession { meta: index.meta, events };
	Ok((loaded, filter))
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::events::{Call, CallStatus, EventBase, EventKind, SessionEvent, Text};
	use crate::json::Value;

	fn event(kind: EventKind) -> SessionEvent {
		SessionEvent {
			base: EventBase { entry: "e1".to_string(), block: None, t: "2026-10-28T10:00:00".to_string(), line: 1, turn: 0 },
			kind,
		}
	}

	fn call(tool: &str) -> Call {
		Call {
			id: "c1".to_string(),
			tool: tool.to_string(),
			args: Value::Null,
			intent: None,
			status: CallStatus::Ok,
			result_entry: None,
			result_chars: None,
			dur_ms: None,
		}
	}

	#[test]
	fn parses_kinds_aliases_groups_and_tools() {
		let filter = parse_type_filter(&["thinking,SAY", "calls", "irc", "call:bash", "marker: compaction"]).unwrap().unwrap();
		assert_eq!(filter.source, "thinking,SAY,calls,irc,call:bash,marker: compaction");
		assert_eq!(filter.include.len(), 6);
		assert!(matches!(&filter.include[0], Selector::Kind { kinds } if *kinds == ["thinking"]));
		assert!(matches!(&filter.include[1], Selector::Kind { kinds } if *kinds == ["say"]));
		assert!(matches!(&filter.include[2], Selector::Kind { kinds } if *kinds == ["call"]));
		assert!(matches!(&filter.include[3], Selector::Kind { kinds } if *kinds == ["irc_in", "irc_out"]));
		assert!(matches!(&filter.include[4], Selector::Tool { tool } if tool == "bash"));
		assert!(matches!(&filter.include[5], Selector::Marker { what } if what == "compaction"));
	}

	#[test]
	fn fold_normalizes_separators() {
		assert_eq!(fold("  Tool-Call__Rate limit  "), "tool call rate limit");
		let filter = parse_type_filter(&["marker:rate-limit"]).unwrap().unwrap();
		assert!(matches!(&filter.include[0], Selector::Marker { what } if what == "rate limit"));
	}

	#[test]
	fn exclusion_only_keeps_everything_else() {
		let filter = parse_type_filter(&["-call", "!marker"]).unwrap().unwrap();
		let say = event(EventKind::Say { text: Text::Plain("hi".to_string()) });
		let call = event(EventKind::Call { call: call("bash") });
		let marker = event(EventKind::Marker { what: "compaction".to_string(), detail: String::new() });
		assert!(keeps(&filter, &say));
		assert!(!keeps(&filter, &call));
		assert!(!keeps(&filter, &marker));
	}

	#[test]
	fn tool_selector_matches_call_irc_and_dispatch() {
		let filter = parse_type_filter(&["call:Write"]).unwrap().unwrap();
		let write_call = call("write");
		assert!(keeps(&filter, &event(EventKind::Call { call: write_call.clone() })));
		assert!(keeps(
			&filter,
			&event(EventKind::IrcOut { to: "a".to_string(), text: Text::Plain("t".to_string()), call: write_call.clone() })
		));
		assert!(keeps(
			&filter,
			&event(EventKind::Dispatch { call: write_call, context: None, children: Vec::new() })
		));
		assert!(!keeps(&filter, &event(EventKind::Call { call: call("bash") })));
		assert!(!keeps(&filter, &event(EventKind::Say { text: Text::Plain("write".to_string()) })));
	}

	#[test]
	fn unknown_and_empty_rest_throw_usage() {
		let err = parse_type_filter(&["nonsense"]).unwrap_err();
		assert!(err.message.starts_with("Unknown type \"nonsense\". Use a kind"), "{}", err.message);
		assert!(err.message.contains("call:<tool>"), "{}", err.message);
		assert!(parse_type_filter(&["call:"]).is_err());
		assert!(parse_type_filter(&["foo:bar"]).is_err());
	}

	#[test]
	fn empty_and_none_give_no_filter() {
		assert!(parse_type_filter(&[]).unwrap().is_none());
		assert!(parse_type_filter(&["", " , "]).unwrap().is_none());
	}
}
