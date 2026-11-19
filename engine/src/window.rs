//! `?since=` / `?until=` time window over timeline events. Accepted values:
//!   30m 2h 3d                     that long before now
//!   2026-10-28                    a local calendar day
//!   2026-10-28T1430[15[.250]]     a local wall time; `T` or a space, colons optional,
//!   2026-10-28T14:30Z / …+0800    a zone (`Z`, `±hhmm`, `±hh:mm`) makes it absolute
//!   1430 / 14:30:15               a wall time on the session's only local date
//! Both bounds include their whole written unit (until=2026-10-28 is that entire
//! day, until=1430 runs through 14:30:59.999); internally the window is
//! half-open [start, end). omp's read peels a trailing `:<digits>` off an
//! internal URL as a line selector, so `until=14:30` at the end of a URL
//! arrives as `until=14`; that truncated shape fails with the colonless forms.

use crate::error::Result;

/// The bounds as written; at least one is present.
#[derive(Clone, Debug, PartialEq)]
pub struct WindowSpec {
	// slice: window — the written bounds
}

/// Resolved window in epoch ms: `start` inclusive, `end` exclusive; None is open.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TimeWindow {
	pub start: Option<i64>,
	pub end: Option<i64>,
}

impl WindowSpec {
	/// Whether a bound is a bare wall time, which takes its date from the session's events.
	pub fn has_clock(&self) -> bool {
		todo!("slice: window")
	}
}

/// Parses the written bounds; None when neither is given. A malformed value is a `bad_request`.
pub fn parse_window(since: Option<&str>, until: Option<&str>) -> Result<Option<WindowSpec>> {
	let _ = (since, until);
	todo!("slice: window")
}

/// Resolves written bounds against the session's event timestamps (clock-only
/// bounds) and `now_ms` (relative bounds).
pub fn resolve_window(spec: &WindowSpec, timestamps: &mut dyn Iterator<Item = &str>, now_ms: i64) -> Result<TimeWindow> {
	let _ = (spec, timestamps, now_ms);
	todo!("slice: window")
}

/// Whether timestamp `t` falls in the window; an invalid timestamp never does.
pub fn within(window: &TimeWindow, t: &str) -> bool {
	let _ = (window, t);
	todo!("slice: window")
}

/// The window as absolute `since`/`until` query parameters (UTC, millisecond units).
pub fn window_query(window: &TimeWindow) -> String {
	let _ = window;
	todo!("slice: window")
}
