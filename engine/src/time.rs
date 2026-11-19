//! JavaScript `Date` semantics the renderers rely on: parsing entry
//! timestamps, local calendar fields in the system time zone, and local wall
//! times built from fields (with JavaScript's overflow of out-of-range fields
//! and its handling of DST gaps and folds).

use jiff::civil::{Date, DateTime};
use jiff::tz::TimeZone;
use jiff::{Span, Timestamp, ZonedDateTime};

/// Local calendar and clock fields of one instant, plus the zone's offset there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LocalParts {
	pub year: i16,
	pub month: i8,
	pub day: i8,
	pub hour: i8,
	pub minute: i8,
	pub second: i8,
	/// Minutes east of UTC (the negation of `getTimezoneOffset()`).
	pub offset_minutes: i32,
}

fn system_zone() -> TimeZone {
	TimeZone::system()
}

/// `Date.parse` of the timestamp forms session files carry: RFC 3339 with a
/// zone (`…Z`, `…+09:00`), a date-time without a zone (local time), or a bare
/// date (UTC midnight). None when JavaScript would give NaN for these forms.
pub fn parse_ms(text: &str) -> Option<i64> {
	if let Ok(at) = text.parse::<Timestamp>() {
		return Some(at.as_millisecond());
	}
	if text.len() > 10
		&& let Ok(local) = text.parse::<DateTime>()
	{
		return local.to_zoned_datetime(system_zone()).ok().map(|zoned| zoned.timestamp().as_millisecond());
	}
	if let Ok(date) = text.parse::<Date>() {
		return date.to_zoned_datetime(TimeZone::UTC).ok().map(|zoned| zoned.timestamp().as_millisecond());
	}
	None
}

fn zoned(ms: i64) -> Option<ZonedDateTime> {
	Timestamp::from_millisecond(ms).ok().map(|at| at.to_zoned_datetime(system_zone()))
}

/// The local fields of `ms` (the `get*` accessors of `new Date(ms)`).
pub fn local_parts(ms: i64) -> Option<LocalParts> {
	let at = zoned(ms)?;
	Some(LocalParts {
		year: at.year(),
		month: at.month(),
		day: at.day(),
		hour: at.hour(),
		minute: at.minute(),
		second: at.second(),
		offset_minutes: at.offset().seconds() / 60,
	})
}

/// Local calendar date `YYYY-MM-DD` of `ms`.
pub fn local_date(ms: i64) -> Option<String> {
	local_parts(ms).map(|p| format!("{:04}-{:02}-{:02}", p.year, p.month, p.day))
}

/// `new Date(ms).toISOString()`: `YYYY-MM-DDTHH:mm:ss.sssZ`.
pub fn iso_utc(ms: i64) -> String {
	match Timestamp::from_millisecond(ms) {
		Ok(at) => at.strftime("%Y-%m-%dT%H:%M:%S%.3fZ").to_string(),
		Err(_) => "Invalid Date".to_string(),
	}
}

/// A civil date-time from fields, normalizing out-of-range month, day and time
/// fields the way `new Date(year, month - 1, day, …)` does.
fn civil(year: i32, month: i32, day: i32, hour: i32, minute: i32, second: i32, milli: i32) -> Option<DateTime> {
	let base = Date::new(i16::try_from(year).ok()?, 1, 1).ok()?.at(0, 0, 0, 0);
	let span = Span::new()
		.try_months(i64::from(month) - 1)
		.ok()?
		.try_days(i64::from(day) - 1)
		.ok()?
		.try_hours(i64::from(hour))
		.ok()?
		.try_minutes(i64::from(minute))
		.ok()?
		.try_seconds(i64::from(second))
		.ok()?
		.try_milliseconds(i64::from(milli))
		.ok()?;
	base.checked_add(span).ok()
}

/// `new Date(year, month - 1, day, hour, minute, second, milli).getTime()`
/// in the system time zone (`month` is 1-based). A wall time in a DST gap moves
/// forward and one in a fold takes the earlier instant, as JavaScript does.
pub fn local_ms(year: i32, month: i32, day: i32, hour: i32, minute: i32, second: i32, milli: i32) -> Option<i64> {
	let local = civil(year, month, day, hour, minute, second, milli)?;
	local.to_zoned_datetime(system_zone()).ok().map(|at| at.timestamp().as_millisecond())
}

/// `Date.UTC(year, month - 1, day, hour, minute, second, milli)` (`month` is 1-based).
pub fn utc_ms(year: i32, month: i32, day: i32, hour: i32, minute: i32, second: i32, milli: i32) -> Option<i64> {
	let local = civil(year, month, day, hour, minute, second, milli)?;
	local.to_zoned_datetime(TimeZone::UTC).ok().map(|at| at.timestamp().as_millisecond())
}

/// Milliseconds since the epoch now (`Date.now()`).
pub fn now_ms() -> i64 {
	Timestamp::now().as_millisecond()
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn parses_zoned_timestamps() {
		assert_eq!(parse_ms("2026-11-09T12:53:54.777Z"), Some(1_794_228_834_777));
		assert_eq!(parse_ms("2026-11-09T21:53:54.777+09:00"), Some(1_794_228_834_777));
		assert_eq!(parse_ms("2026-11-09"), Some(1_794_182_400_000));
		assert_eq!(parse_ms("not a time"), None);
		assert_eq!(parse_ms(""), None);
	}

	#[test]
	fn iso_round_trip() {
		assert_eq!(iso_utc(1_794_228_834_777), "2026-11-09T12:53:54.777Z");
	}

	#[test]
	fn utc_fields_overflow_like_javascript() {
		assert_eq!(utc_ms(2026, 1, 32, 0, 0, 0, 0), utc_ms(2026, 2, 1, 0, 0, 0, 0));
		assert_eq!(utc_ms(2026, 13, 1, 0, 0, 0, 0), utc_ms(2027, 1, 1, 0, 0, 0, 0));
	}
}
