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

use std::path::Path;

use crate::error::Result;
use crate::events::{LoadedSession, SessionEvent};
use crate::window::{TimeWindow, WindowSpec};

#[derive(Clone, Debug, PartialEq)]
pub struct TypeFilter {
	// slice: filter — inclusions and exclusions
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

/// Parses every `type` value of a query; None when none is given (or all are empty).
pub fn parse_type_filter(values: &[&str]) -> Result<Option<TypeFilter>> {
	let _ = values;
	todo!("slice: filter")
}

/// Whether `event` passes: any inclusion (or none given), and no exclusion.
pub fn keeps(filter: &TypeFilter, event: &SessionEvent) -> bool {
	let _ = (filter, event);
	todo!("slice: filter")
}

/// Whether `event` passes both the type filter and the time window.
pub fn shows(filter: &EventFilter, event: &SessionEvent) -> bool {
	let _ = (filter, event);
	todo!("slice: filter")
}

/// Loads a session for a read filtered by `types` and the written window
/// `spec` (resolved against the session and `now_ms`). The loaded events
/// render exactly as the fully built session filtered afterwards would.
/// Without a filter this is `load_session`.
pub fn load_filtered(
	file: &Path,
	sid: &str,
	agent: &str,
	types: Option<TypeFilter>,
	spec: Option<WindowSpec>,
	now_ms: i64,
	read: FilteredRead,
) -> Result<(LoadedSession, EventFilter)> {
	let _ = (file, sid, agent, types, spec, now_ms, read);
	todo!("slice: filter")
}
