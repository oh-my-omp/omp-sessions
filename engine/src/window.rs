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

use std::sync::LazyLock;

use regex::Regex;

use crate::error::{EngineError, Result};
use crate::time;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Day {
	year: i32,
	month: i32,
	day: i32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Clock {
	hour: i32,
	minute: i32,
	second: i32,
	milli: i32,
	/// Length of the written unit in ms: a minute, a second, or the fraction's last digit.
	unit: i64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Bound {
	/// `ms` stays a float because JavaScript counts it in doubles; huge values
	/// like `since=999999999999999999d` overflow an i64 but stay finite.
	Ago { ms: f64 },
	Day { day: Day },
	/// `zone` is minutes east of UTC; None is local time.
	Moment { day: Day, clock: Clock, zone: Option<i32> },
	Clock { clock: Clock },
}

/// The bounds as written; at least one is present.
#[derive(Clone, Debug, PartialEq)]
pub struct WindowSpec {
	since: Option<Bound>,
	until: Option<Bound>,
}

/// Resolved window in epoch ms: `start` inclusive, `end` exclusive; None is open.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TimeWindow {
	pub start: Option<i64>,
	pub end: Option<i64>,
}

#[derive(Clone, Copy)]
enum Key {
	Since,
	Until,
}

impl Key {
	fn name(self) -> &'static str {
		match self {
			Key::Since => "since",
			Key::Until => "until",
		}
	}
}

/// JavaScript's `String.prototype.trim`: Unicode whitespace plus U+FEFF.
fn js_trim(text: &str) -> &str {
	text.trim_matches(|c: char| c.is_whitespace() || c == '\u{FEFF}')
}

static AGO: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^([0-9]+)([mhdMHD])$").unwrap());
static DAY: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^([0-9]{4})-([0-9]{2})-([0-9]{2})$").unwrap());
static MOMENT: LazyLock<Regex> = LazyLock::new(|| {
	Regex::new(concat!(
		r"^([0-9]{4})-([0-9]{2})-([0-9]{2})[Tt ]",
		r"([0-9]{2}):?([0-9]{2})(?::?([0-9]{2})(?:\.([0-9]{1,3}))?)?",
		r"(Z|z|[+-][0-9]{2}:?[0-9]{2})?$"
	))
	.unwrap()
});
static CLOCK_ONLY: LazyLock<Regex> =
	LazyLock::new(|| Regex::new(r"^([0-9]{2}):?([0-9]{2})(?::?([0-9]{2})(?:\.([0-9]{1,3}))?)?$").unwrap());
/// What a peeled `:mm`, `:ss` or zone `:mm` leaves behind.
static TRUNCATED: LazyLock<Regex> = LazyLock::new(|| {
	Regex::new(r"^(?:[0-9]{4}-[0-9]{2}-[0-9]{2}[Tt ])?[0-9]{2}$|[Tt ][0-9]{2,6}(?:\.[0-9]{1,3})?[+-][0-9]{2}$").unwrap()
});

fn unit_ms(unit: char) -> i64 {
	match unit {
		'm' | 'M' => 60_000,
		'h' | 'H' => 3_600_000,
		'd' | 'D' => 86_400_000,
		_ => 0,
	}
}

fn bad_value(key: Key, raw: &str) -> EngineError {
	EngineError::bad_request(format!(
		"Bad {key}={raw}. Use {key}=<n>m|h|d (that long before now), YYYY-MM-DD (a local day), \
		 YYYY-MM-DDThhmm[ss][Z|±hhmm] (local unless zoned), or hhmm[ss] (a wall time on the session's only date).",
		key = key.name()
	))
}

/// `new Date(y, m - 1, d)` maps years 0–99 to 1900–1999.
fn js_year(year: i32) -> i32 {
	if (0..100).contains(&year) { year + 1900 } else { year }
}

fn day(key: Key, raw: &str, year: &str, month: &str, date: &str) -> Result<Day> {
	let parsed = Day {
		year: year.parse::<i32>().map_err(|_| bad_value(key, raw))?,
		month: month.parse::<i32>().map_err(|_| bad_value(key, raw))?,
		day: date.parse::<i32>().map_err(|_| bad_value(key, raw))?,
	};
	match time::local_parts(match time::local_ms(js_year(parsed.year), parsed.month, parsed.day, 0, 0, 0, 0) {
		Some(ms) => ms,
		None => return Err(bad_value(key, raw)),
	}) {
		Some(check)
			if i32::from(check.year) == parsed.year
				&& i32::from(check.month) == parsed.month
				&& i32::from(check.day) == parsed.day => Ok(parsed),
		_ => Err(bad_value(key, raw)),
	}
}

fn clock(key: Key, raw: &str, hour: &str, minute: &str, second: Option<&str>, fraction: Option<&str>) -> Result<Clock> {
	let parsed = Clock {
		hour: hour.parse::<i32>().map_err(|_| bad_value(key, raw))?,
		minute: minute.parse::<i32>().map_err(|_| bad_value(key, raw))?,
		second: match second {
			Some(second) => second.parse::<i32>().map_err(|_| bad_value(key, raw))?,
			None => 0,
		},
		milli: match fraction {
			Some(fraction) => {
				let mut padded = String::from(fraction);
				while padded.len() < 3 {
					padded.push('0');
				}
				padded.parse::<i32>().map_err(|_| bad_value(key, raw))?
			}
			None => 0,
		},
		unit: match (second, fraction) {
			(None, _) => 60_000,
			(Some(_), None) => 1000,
			(Some(_), Some(fraction)) => 10_i64.pow(3 - fraction.len() as u32),
		},
	};
	if parsed.hour > 23 || parsed.minute > 59 || parsed.second > 59 {
		return Err(bad_value(key, raw));
	}
	Ok(parsed)
}

fn zone(key: Key, raw: &str, written: Option<&str>) -> Result<Option<i32>> {
	let Some(written) = written else { return Ok(None) };
	if written.eq_ignore_ascii_case("z") {
		return Ok(Some(0));
	}
	let digits: String = written[1..].replace(':', "");
	let hours = digits[..2].parse::<i32>().map_err(|_| bad_value(key, raw))?;
	let minutes = digits[2..].parse::<i32>().map_err(|_| bad_value(key, raw))?;
	if hours > 14 || minutes > 59 {
		return Err(bad_value(key, raw));
	}
	let sign = if written.starts_with('-') { -1 } else { 1 };
	Ok(Some(sign * (hours * 60 + minutes)))
}

fn parse_bound(key: Key, raw: &str) -> Result<Bound> {
	let value = js_trim(raw);
	if let Some(ago) = AGO.captures(value) {
		let count = ago[1].parse::<f64>().map_err(|_| bad_value(key, raw))?;
		let unit = ago[2].chars().next().unwrap_or('m');
		return Ok(Bound::Ago { ms: count * unit_ms(unit) as f64 });
	}
	if let Some(date) = DAY.captures(value) {
		return Ok(Bound::Day { day: day(key, raw, &date[1], &date[2], &date[3])? });
	}
	if let Some(moment) = MOMENT.captures(value) {
		return Ok(Bound::Moment {
			day: day(key, raw, &moment[1], &moment[2], &moment[3])?,
			clock: clock(
				key,
				raw,
				&moment[4],
				&moment[5],
				moment.get(6).map(|m| m.as_str()),
				moment.get(7).map(|m| m.as_str()),
			)?,
			zone: zone(key, raw, moment.get(8).map(|m| m.as_str()))?,
		});
	}
	if let Some(clock_match) = CLOCK_ONLY.captures(value) {
		return Ok(Bound::Clock {
			clock: clock(
				key,
				raw,
				&clock_match[1],
				&clock_match[2],
				clock_match.get(3).map(|m| m.as_str()),
				clock_match.get(4).map(|m| m.as_str()),
			)?,
		});
	}
	if TRUNCATED.is_match(value) {
		return Err(EngineError::bad_request(format!(
			"{key}={raw} is cut short: read takes a trailing \":<digits>\" of a URL as a line selector, so a time \
			 like 14:30 at the end of the URL arrives without its \":30\". Write the time without colons (1430, \
			 2026-10-28T143000+0800), as 14%3A30, or put {key} before another query key.",
			key = key.name()
		)));
	}
	Err(bad_value(key, raw))
}

/// Parses the written bounds; None when neither is given. A malformed value is a `bad_request`.
pub fn parse_window(since: Option<&str>, until: Option<&str>) -> Result<Option<WindowSpec>> {
	if since.is_none() && until.is_none() {
		return Ok(None);
	}
	Ok(Some(WindowSpec {
		since: since.map(|value| parse_bound(Key::Since, value)).transpose()?,
		until: until.map(|value| parse_bound(Key::Until, value)).transpose()?,
	}))
}

fn day_name(value: Day) -> String {
	format!("{:04}-{:02}-{:02}", value.year, value.month, value.day)
}

/// The single local date of the session's timestamped events, for clock-only bounds.
fn session_day(timestamps: &mut dyn Iterator<Item = &str>) -> Result<Day> {
	// Days in first-seen order, as a JavaScript Map keeps its keys.
	let mut days: Vec<Day> = Vec::new();
	for t in timestamps {
		let Some(at) = time::parse_ms(t) else { continue };
		let Some(parts) = time::local_parts(at) else { continue };
		let value = Day {
			year: i32::from(parts.year),
			month: i32::from(parts.month),
			day: i32::from(parts.day),
		};
		if days.iter().all(|seen| *seen != value) {
			days.push(value);
		}
	}
	let mut values = days.iter();
	let Some(only) = values.next() else {
		return Err(EngineError::bad_request(
			"The session has no timestamped events; give since/until with a date (YYYY-MM-DD…).",
		));
	};
	if values.next().is_some() {
		let names: Vec<String> = days.iter().map(|day| day_name(*day)).collect();
		return Err(EngineError::bad_request(format!(
			"A bare time is ambiguous: the session spans {}. Add the date, e.g. {}T1430.",
			names.join(", "),
			names.last().map(String::as_str).unwrap_or("")
		)));
	}
	Ok(*only)
}

struct Span {
	start: i64,
	end: i64,
}

/// The half-open span [start, end) of a bound's written unit. `session` is the
/// session's only local day, required by and only by a bare clock bound.
fn span(bound: &Bound, session: Option<Day>, now_ms: i64) -> Result<Span> {
	match bound {
		Bound::Ago { ms } => {
			let start = (now_ms as f64 - ms) as i64;
			Ok(Span { start, end: start.saturating_add(1) })
		}
		Bound::Day { day } => {
			let start = time::local_ms(js_year(day.year), day.month, day.day, 0, 0, 0, 0);
			let end = time::local_ms(js_year(day.year), day.month, day.day + 1, 0, 0, 0, 0);
			match (start, end) {
				(Some(start), Some(end)) => Ok(Span { start, end }),
				_ => Err(EngineError::bad_request(format!(
					"Bad day {}-{:02}-{:02}.",
					day.year, day.month, day.day
				))),
			}
		}
		Bound::Moment { day, clock, zone } => {
			let start = match zone {
				None => time::local_ms(
					js_year(day.year),
					day.month,
					day.day,
					clock.hour,
					clock.minute,
					clock.second,
					clock.milli,
				),
				Some(zone) => time::utc_ms(
					js_year(day.year),
					day.month,
					day.day,
					clock.hour,
					clock.minute,
					clock.second,
					clock.milli,
				)
				.map(|ms| ms - i64::from(*zone) * 60_000),
			};
			match start {
				Some(start) => Ok(Span { start, end: start + clock.unit }),
				None => Err(EngineError::bad_request(format!(
					"Bad time {}-{:02}-{:02} {:02}:{:02}.",
					day.year, day.month, day.day, clock.hour, clock.minute
				))),
			}
		}
		Bound::Clock { clock } => {
			let Some(day) = session else {
				return Err(EngineError::bad_request(
					"The session has no timestamped events; give since/until with a date (YYYY-MM-DD…).",
				));
			};
			let start = time::local_ms(
				js_year(day.year),
				day.month,
				day.day,
				clock.hour,
				clock.minute,
				clock.second,
				clock.milli,
			);
			match start {
				Some(start) => Ok(Span { start, end: start + clock.unit }),
				None => Err(EngineError::bad_request(format!(
					"Bad time {:02}:{:02}.",
					clock.hour, clock.minute
				))),
			}
		}
	}
}

/// Resolves written bounds against the session's event timestamps (clock-only
/// bounds) and `now_ms` (relative bounds).
pub fn resolve_window(spec: &WindowSpec, timestamps: &mut dyn Iterator<Item = &str>, now_ms: i64) -> Result<TimeWindow> {
	// A bare clock bound takes its date from the session's events; compute it
	// once so both bounds can share it.
	let session = if spec.has_clock() { Some(session_day(timestamps)?) } else { None };
	let start = match &spec.since {
		None => None,
		Some(bound) => Some(span(bound, session, now_ms)?.start),
	};
	let end = match &spec.until {
		None => None,
		Some(bound) => Some(span(bound, session, now_ms)?.end),
	};
	if let (Some(start), Some(end)) = (start, end)
		&& start >= end
	{
		return Err(EngineError::bad_request(format!(
			"since ({}) is not before until ({}).",
			time::iso_utc(start),
			time::iso_utc(end)
		)));
	}
	Ok(TimeWindow { start, end })
}

impl WindowSpec {
	/// Whether a bound is a bare wall time, which takes its date from the session's events.
	pub fn has_clock(&self) -> bool {
		matches!(self.since, Some(Bound::Clock { .. })) || matches!(self.until, Some(Bound::Clock { .. }))
	}
}

/// Whether timestamp `t` falls in the window; an invalid timestamp never does.
pub fn within(window: &TimeWindow, t: &str) -> bool {
	let Some(at) = time::parse_ms(t) else { return false };
	window.start.is_none_or(|start| at >= start) && window.end.is_none_or(|end| at < end)
}

/// `encodeURIComponent`: keep `A-Za-z0-9-_.!~*'()`, percent-encode the rest as
/// UTF-8 with uppercase hex.
fn encode_uri_component(text: &str) -> String {
	let mut out = String::with_capacity(text.len());
	for &byte in text.as_bytes() {
		match byte {
			b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')' => {
				out.push(char::from(byte));
			}
			_ => {
				out.push('%');
				out.push_str(&format!("{byte:02X}"));
			}
		}
	}
	out
}

/// The window as absolute `since`/`until` query parameters (UTC, millisecond units).
pub fn window_query(window: &TimeWindow) -> String {
	let mut parts: Vec<String> = Vec::new();
	if let Some(start) = window.start {
		parts.push(format!("since={}", encode_uri_component(&time::iso_utc(start))));
	}
	if let Some(end) = window.end {
		parts.push(format!("until={}", encode_uri_component(&time::iso_utc(end - 1))));
	}
	parts.join("&")
}

#[cfg(test)]
mod tests {
	use super::*;

	fn spec(since: Option<&str>, until: Option<&str>) -> WindowSpec {
		parse_window(since, until).unwrap().unwrap()
	}

	#[test]
	fn parses_ago() {
		let window = spec(Some("30m"), None);
		assert!(matches!(window.since, Some(Bound::Ago { ms }) if ms == 30.0 * 60_000.0));
		assert!(matches!(spec(Some("2H"), None).since, Some(Bound::Ago { ms }) if ms == 2.0 * 3_600_000.0));
		assert!(matches!(spec(Some("3d"), None).since, Some(Bound::Ago { ms }) if ms == 3.0 * 86_400_000.0));
		assert!(!window.has_clock());
	}

	#[test]
	fn parses_day_and_moments() {
		assert!(matches!(spec(Some("2026-10-28"), None).since, Some(Bound::Day { .. })));
		assert!(matches!(
			spec(Some("2026-10-28T1430"), None).since,
			Some(Bound::Moment { zone: None, clock: Clock { hour: 14, minute: 30, second: 0, milli: 0, unit: 60_000 }, .. })
		));
		assert!(matches!(
			spec(Some("2026-10-28 14:30:15.25"), None).since,
			Some(Bound::Moment { clock: Clock { second: 15, milli: 250, unit: 10, .. }, .. })
		));
		assert!(matches!(
			spec(Some("2026-10-28T14:30Z"), None).since,
			Some(Bound::Moment { zone: Some(0), .. })
		));
		assert!(matches!(
			spec(Some("2026-10-28T1430+0800"), None).since,
			Some(Bound::Moment { zone: Some(480), .. })
		));
		assert!(matches!(
			spec(Some("2026-10-28T1430-05:30"), None).since,
			Some(Bound::Moment { zone: Some(-330), .. })
		));
	}

	#[test]
	fn parses_bare_clock() {
		let window = spec(None, Some("1430"));
		assert!(matches!(
			window.until,
			Some(Bound::Clock { clock: Clock { hour: 14, minute: 30, unit: 60_000, .. } })
		));
		assert!(window.has_clock());
		assert!(matches!(
			spec(Some("14:30:15"), None).since,
			Some(Bound::Clock { clock: Clock { second: 15, unit: 1000, .. } })
		));
	}

	#[test]
	fn rejects_bad_values() {
		for bad in ["x", "2026-13-01", "2026-02-30", "2460", "14:61", "2026-10-28T1430+1500"] {
			assert!(parse_window(Some(bad), None).is_err(), "{bad} should fail");
		}
	}

	#[test]
	fn truncated_shape_has_its_own_message() {
		let err = parse_window(None, Some("14")).unwrap_err();
		assert!(err.message.contains("until=14 is cut short"), "{}", err.message);
		assert!(err.message.contains("14%3A30"), "{}", err.message);
		assert!(parse_window(Some("2026-10-28T14"), None).is_err());
	}

	#[test]
	fn resolves_against_now_and_session_day() {
		let now = 1_800_000_000_000_i64;
		let window = resolve_window(&spec(Some("30m"), None), &mut std::iter::empty(), now).unwrap();
		assert_eq!(window.start, Some(now - 30 * 60_000));
		assert_eq!(window.end, None);

		// One local day for both events (zone-less times parse as local): a
		// bare clock resolves on it.
		let ts = ["2026-10-28T14:00:00", "2026-10-28T16:00:00"];
		let window = resolve_window(&spec(Some("1430"), None), &mut ts.iter().copied(), now).unwrap();
		let expected = time::local_ms(2026, 10, 28, 14, 30, 0, 0).unwrap();
		assert_eq!(window.start, Some(expected));
	}

	#[test]
	fn resolve_errors() {
		let now = 1_800_000_000_000_i64;
		// No timestamped events.
		assert!(
			resolve_window(&spec(Some("1430"), None), &mut std::iter::empty(), now)
				.unwrap_err()
				.message
				.contains("no timestamped events")
		);
		// Two local days: ambiguous.
		let ts = ["2026-10-28T14:00:00", "2026-10-29T10:00:00"];
		let err = resolve_window(&spec(Some("1430"), None), &mut ts.iter().copied(), now).unwrap_err();
		assert!(err.message.contains("A bare time is ambiguous"), "{}", err.message);
		// since not before until.
		let err = resolve_window(&spec(Some("2026-10-29"), Some("2026-10-28")), &mut std::iter::empty(), now)
			.unwrap_err();
		assert!(err.message.contains("is not before until"), "{}", err.message);
	}

	#[test]
	fn within_checks_half_open() {
		let window = TimeWindow { start: Some(1000), end: Some(2000) };
		assert!(!within(&window, "1970-01-01T00:00:00.999Z"));
		assert!(within(&window, "1970-01-01T00:00:01.000Z"));
		assert!(within(&window, "1970-01-01T00:00:01.999Z"));
		assert!(!within(&window, "1970-01-01T00:00:02.000Z"));
		assert!(!within(&window, "not a date"));
		assert!(within(&TimeWindow { start: None, end: None }, "1970-01-01T00:00:00Z"));
	}

	#[test]
	fn window_query_encodes() {
		let window = TimeWindow { start: Some(0), end: Some(1500) };
		assert_eq!(window_query(&window), "since=1970-01-01T00%3A00%3A00.000Z&until=1970-01-01T00%3A00%3A01.499Z");
		assert_eq!(window_query(&TimeWindow { start: None, end: None }), "");
	}
}
