//! Renderers: the timeline (column-0 headers, every body line indented two
//! spaces), events JSONL records for jq, and one-entry detail. Pure functions
//! of the loaded session; `base` is the session:// URL the reader used for this
//! session (or subagent), so every pointer printed here can be read back directly.

use crate::events::{EntryDetail, LoadedSession};
use crate::filter::EventFilter;
use crate::json::Value;

/// Local ISO-8601 time with the UTC offset, e.g. `2026-10-26 16:18:30 +08:00`;
/// the input unchanged when it is not a valid timestamp.
pub fn local_stamp(iso: &str) -> String {
	let _ = iso;
	todo!("slice: render")
}

/// The timeline. `root` is the root session's canonical URL (subagent results
/// point under it); `title` is the title from the session's header scan, else
/// the journal's, else the first prompt.
pub fn render_timeline(loaded: &LoadedSession, base: &str, root: &str, title: Option<&str>, filter: &EventFilter) -> String {
	let _ = (loaded, base, root, title, filter);
	todo!("slice: render")
}

/// One JSON object per event that `filter` shows, in the field order events.jsonl writes.
pub fn event_records(loaded: &LoadedSession, base: &str, filter: &EventFilter) -> Vec<Value> {
	let _ = (loaded, base, filter);
	todo!("slice: render")
}

/// One entry in full: a tool result shows its call (arguments, intent, timing) and the whole result text.
pub fn render_entry(detail: &EntryDetail, base: &str, sid: &str) -> String {
	let _ = (detail, base, sid);
	todo!("slice: render")
}
