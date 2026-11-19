//! jq over JSONL resources (`?q=…`), with omp's selector parsing, paging and
//! continuation hints (omp `tools/read-json.ts` `parseJsonSelector`,
//! `executeJsonQuery`), evaluated by the jaq interpreter over the records as
//! jq's input stream.
//!
//! omp runs its `jq` builtin — jaq patched to jq 1.8.2's semantics — over the
//! file. This module evaluates the same filters with the published jaq crates
//! and reproduces the front end's behavior in-process: jq's output bytes for
//! each value, jq's stderr reports, the capture cap and the page lookahead.
//! Remaining differences from omp's patched jq are in the filter language
//! itself (grammar, error wording, `|=` semantics, regex engine, literal
//! number canonicalization); they are documented on [`execute`].

use std::cell::{Cell, RefCell};
use std::fmt::Write as _;
use std::rc::Rc;

use jaq_core::{
	Ctx, DataT, Error as CoreError, Exn, RunPtr, Vars, box_iter::box_once, compile, data, load,
	native,
};
use jaq_json::{Num, Val};
use jaq_std::ValT as _;
use jaq_std::input::RcIter;

use crate::error::{EngineError, Result};
use crate::json::Value;

const DEFAULT_JSON_QUERY_LIMIT: usize = 100;
const MAX_JSON_QUERY_LIMIT: usize = 1000;
const MAX_QUERY_CAPTURE_CHARS: usize = 5 * 1024 * 1024;
/// jq interpolates the offending value into a diagnostic; middle-eliding each
/// line keeps its reason and key (read-json.ts `JSON_QUERY_ERROR_LINE_MAX_BYTES`).
const JSON_QUERY_ERROR_LINE_MAX_BYTES: usize = 512;
/// Bounds the jq stderr quoted in a result; the tail keeps the error after
/// any `debug` lines (read-json.ts `JSON_QUERY_ERROR_MAX_BYTES`).
const JSON_QUERY_ERROR_MAX_BYTES: usize = 8 * 1024;
/// `truncateMiddle`'s default line budget (pi-tui `DEFAULT_MAX_LINES`).
const TRUNCATION_MAX_LINES: usize = 3000;
/// jq's default pretty-print indent.
const PRETTY_INDENT: &str = "  ";
/// The input stream's name in jq's error positions.
const INPUT_NAME: &str = "<stdin>";
/// jq's `--build-configuration` value (the reference build, as omp reports).
const BUILD_CONFIGURATION: &str = "--disable-debug --disable-dependency-tracking --prefix=/opt/homebrew/Cellar/jq/1.8.2 --libdir=/opt/homebrew/Cellar/jq/1.8.2/lib --disable-silent-rules --disable-maintainer-mode CC=clang";

/// One page of query results requested via `offset`/`limit`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JsonPage {
	pub offset: usize,
	pub limit: usize,
}

/// Options parsed from a `?q=<filter>` query string.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JsonSelector {
	pub query: String,
	pub raw: bool,
	pub compact: bool,
	/// Present when `offset` or `limit` asks for a page instead of the whole output.
	pub page: Option<JsonPage>,
}

/// Why output capture ended before evaluation finished on its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum JqStop {
	CaptureCap,
	PageFull,
}

/// How one output value is rendered.
enum Render<'a> {
	/// Non-page mode: the selector's flags, like `jq [-r] [-c]`.
	Selector(&'a JsonSelector),
	/// Page mode: jq runs with `-c`, one compact value per line.
	Compact,
}

// ============================================================================
// Selector parsing (read-json.ts `parseJsonSelector` and helpers)
// ============================================================================

/// Parses `q`/`query`, `raw`, `compact`, `offset` and `limit` from a query
/// string without its leading `?`; None when none of them is present.
///
/// As omp: `q` is percent-decoded but never form-decoded, so jq's `+`
/// survives; a filter with a bare `%` (jq modulo) is not valid
/// percent-encoding and is taken verbatim; modifiers or an empty filter imply
/// `.`; unknown-only query strings return None.
pub fn parse_selector(query_string: &str) -> Option<JsonSelector> {
	let encoded_query = first_query_param(query_string);
	let flag = |name: &str| -> Option<bool> {
		let value = param_get(query_string, name)?;
		// `params.get(name)?.toLowerCase()` against "true"/"1"
		Some(value == "1" || value.eq_ignore_ascii_case("true"))
	};
	let raw = flag("raw");
	let compact = flag("compact");
	let offset = param_get(query_string, "offset").map(|v| parse_count(&v, 0));
	let limit = param_get(query_string, "limit").map(|v| parse_count(&v, 1));
	let seen = raw.is_some() || compact.is_some() || offset.is_some() || limit.is_some();
	if encoded_query.is_none() && !seen {
		return None;
	}
	let encoded = encoded_query.unwrap_or(".");
	// A filter with a bare `%` (jq modulo) is not valid percent-encoding;
	// take it verbatim.
	let query = decode_uri_component(encoded).unwrap_or_else(|| encoded.to_string());
	let offset = offset.flatten();
	let limit = limit.flatten();
	Some(JsonSelector {
		query,
		raw: raw.unwrap_or(false),
		compact: compact.unwrap_or(false),
		page: if offset.is_none() && limit.is_none() {
			None
		} else {
			Some(JsonPage {
				offset: offset.unwrap_or(0),
				limit: limit.unwrap_or(DEFAULT_JSON_QUERY_LIMIT).min(MAX_JSON_QUERY_LIMIT),
			})
		},
	})
}

/// The raw value of the first `q=` or `query=` parameter, as omp's regex
/// `/(?:^|&)(?:q|query)=([^&]*)/` finds it: not percent-decoded.
fn first_query_param(query_string: &str) -> Option<&str> {
	for segment in query_string.split('&') {
		if let Some(value) = segment.strip_prefix("q=").or_else(|| segment.strip_prefix("query=")) {
			return Some(value);
		}
	}
	None
}

/// `URLSearchParams.get(name)`: the first decoded value of `name`, None when
/// absent. Values are form-decoded (`+` is a space, invalid escapes are
/// literal).
fn param_get(query_string: &str, name: &str) -> Option<String> {
	for segment in query_string.split('&') {
		let (key, value) = segment.split_once('=').unwrap_or((segment, ""));
		if form_decode(key) == name {
			return Some(form_decode(value));
		}
	}
	None
}

/// `URLSearchParams` decoding: `+` becomes a space, valid percent triplets
/// become their UTF-8 text, everything else stays literal.
fn form_decode(encoded: &str) -> String {
	let bytes = encoded.as_bytes();
	let mut decoded = Vec::with_capacity(bytes.len());
	let mut i = 0;
	while i < bytes.len() {
		match bytes[i] {
			b'+' => {
				decoded.push(b' ');
				i += 1;
			}
			b'%' if i + 2 < bytes.len() => {
				if let (Some(hi), Some(lo)) = (hex_digit(bytes[i + 1]), hex_digit(bytes[i + 2])) {
					decoded.push(hi * 16 + lo);
					i += 3;
				} else {
					decoded.push(b'%');
					i += 1;
				}
			}
			b => {
				decoded.push(b);
				i += 1;
			}
		}
	}
	String::from_utf8_lossy(&decoded).into_owned()
}

fn hex_digit(b: u8) -> Option<u8> {
	match b {
		b'0'..=b'9' => Some(b - b'0'),
		b'a'..=b'f' => Some(b - b'a' + 10),
		b'A'..=b'F' => Some(b - b'A' + 10),
		_ => None,
	}
}

/// `decodeURIComponent`: every percent triplet must form valid UTF-8, else
/// the whole input is left to the caller's fallback. `+` stays `+`.
fn decode_uri_component(encoded: &str) -> Option<String> {
	let bytes = encoded.as_bytes();
	let mut decoded = Vec::with_capacity(bytes.len());
	let mut i = 0;
	while i < bytes.len() {
		if bytes[i] == b'%' {
			if i + 2 >= bytes.len() {
				return None;
			}
			let hi = hex_digit(bytes[i + 1])?;
			let lo = hex_digit(bytes[i + 2])?;
			decoded.push(hi * 16 + lo);
			i += 3;
		} else {
			decoded.push(bytes[i]);
			i += 1;
		}
	}
	String::from_utf8(decoded).ok()
}

/// `parseInt(value, 10)` kept within `min`: the leading decimal digits of the
/// trimmed text, else None.
fn parse_count(value: &str, min: usize) -> Option<usize> {
	let text = value.trim_matches(is_js_whitespace);
	if !text.bytes().next().is_some_and(|b| b.is_ascii_digit()) {
		return None;
	}
	let digits = text
		.bytes()
		.take_while(u8::is_ascii_digit)
		.fold(0usize, |n, b| n.saturating_mul(10).saturating_add(usize::from(b - b'0')));
	(digits >= min).then_some(digits)
}

// ============================================================================
// Continuation hints and renderers (read-json.ts)
// ============================================================================

/// A followable `?q=…&offset=` suffix for the next page; `remaining` is known
/// only for array pages.
fn continuation_hint(selector: &JsonSelector, page: &JsonPage, next_offset: usize, remaining: Option<usize>) -> String {
	let mut flags = String::new();
	if selector.raw {
		flags.push_str("&raw=true");
	}
	if selector.compact {
		flags.push_str("&compact=true");
	}
	let count = match remaining {
		Some(remaining) => format!("{remaining} more items"),
		None => "more results".to_string(),
	};
	// Inverse of the `q` decoding in parse_selector: escaping only `%` and
	// `&` round-trips any filter.
	let query = selector.query.replace('%', "%25").replace('&', "%26");
	format!(
		"[{count}; append ?q={query}{flags}&limit={}&offset={next_offset} to continue]",
		page.limit
	)
}

/// Index of the quote closing the JSON string that opens at `open`.
fn string_end(text: &str, open: usize) -> usize {
	let bytes = text.as_bytes();
	let mut i = open + 1;
	while i < bytes.len() {
		if bytes[i] == b'\\' {
			i += 1;
		} else if bytes[i] == b'"' {
			return i;
		}
		i += 1;
	}
	bytes.len() - 1
}

/// Lays out one compact JSON value the way jq pretty-prints it. Works on the
/// text, so number literals beyond f64 precision stay verbatim.
fn pretty_json(compact: &str) -> String {
	let mut out = String::with_capacity(compact.len() + 16);
	let mut depth = 0usize;
	let bytes = compact.as_bytes();
	let mut i = 0;
	while i < bytes.len() {
		match bytes[i] {
			b'"' => {
				let end = string_end(compact, i);
				out.push_str(&compact[i..=end]);
				i = end;
			}
			b @ (b'{' | b'[') => {
				if bytes.get(i + 1) == Some(&(if b == b'{' { b'}' } else { b']' })) {
					out.push(b as char);
					i += 1;
					out.push(bytes[i] as char);
				} else {
					depth += 1;
					out.push(b as char);
					out.push('\n');
					out.push_str(&PRETTY_INDENT.repeat(depth));
				}
			}
			b @ (b'}' | b']') => {
				depth = depth.saturating_sub(1);
				out.push('\n');
				out.push_str(&PRETTY_INDENT.repeat(depth));
				out.push(b as char);
			}
			b',' => {
				out.push(',');
				out.push('\n');
				out.push_str(&PRETTY_INDENT.repeat(depth));
			}
			b':' => out.push_str(": "),
			_ => {
				// Bytes not covered above are not ASCII; copy the char whole.
				match compact[i..].chars().next() {
					Some(ch) => {
						out.push(ch);
						i += ch.len_utf8() - 1;
					}
					None => break,
				}
			}
		}
		i += 1;
	}
	out
}

/// Splits a compact JSON array into the compact text of each element.
fn split_compact_array(compact: &str) -> Vec<&str> {
	let mut items = Vec::new();
	let mut depth = 0usize;
	let mut start = 1usize;
	let bytes = compact.as_bytes();
	let mut i = 0;
	while i < bytes.len() {
		match bytes[i] {
			b'"' => i = string_end(compact, i),
			b'[' | b'{' => depth += 1,
			b']' | b'}' => {
				depth = depth.saturating_sub(1);
				if depth == 0 && i > start {
					items.push(&compact[start..i]);
				}
			}
			b',' if depth == 1 => {
				items.push(&compact[start..i]);
				start = i + 1;
			}
			_ => {}
		}
		i += 1;
	}
	items
}

/// Renders one compact jq value as `jq` prints it under the selector's
/// `-r`/`-c` flags.
fn render_value(value: &str, selector: &JsonSelector) -> Result<String> {
	if selector.raw && value.starts_with('"') {
		return parse_json_string(value);
	}
	Ok(if selector.compact {
		value.to_string()
	} else {
		pretty_json(value)
	})
}

/// `JSON.parse` of a JSON string literal: `-r` prints strings unquoted.
fn parse_json_string(literal: &str) -> Result<String> {
	let invalid = || EngineError::bad_request("Failed to execute JSON query: invalid JSON string");
	let bytes = literal.as_bytes();
	let mut out = String::with_capacity(literal.len());
	let mut i = 1;
	while i < bytes.len() && bytes[i] != b'"' {
		if bytes[i] != b'\\' {
			match literal[i..].chars().next() {
				Some(ch) => {
					out.push(ch);
					i += ch.len_utf8();
					continue;
				}
				None => break,
			}
		}
		i += 1;
		if i >= bytes.len() {
			return Err(invalid());
		}
		match bytes[i] {
			b'"' => out.push('"'),
			b'\\' => out.push('\\'),
			b'/' => out.push('/'),
			b'b' => out.push('\u{8}'),
			b'f' => out.push('\u{c}'),
			b'n' => out.push('\n'),
			b'r' => out.push('\r'),
			b't' => out.push('\t'),
			b'u' => {
				if i + 4 >= bytes.len() {
					return Err(invalid());
				}
				let unit = u32::from_str_radix(&literal[i + 1..i + 5], 16).map_err(|_| invalid())?;
				i += 4;
				if (0xD800..0xDC00).contains(&unit)
					&& literal[i + 1..].starts_with("\\u")
					&& i + 7 <= bytes.len()
					&& let Ok(low) = u32::from_str_radix(&literal[i + 3..i + 7], 16)
					&& (0xDC00..0xE000).contains(&low)
				{
					i += 6;
					let cp = 0x10000 + ((unit - 0xD800) << 10) + (low - 0xDC00);
					out.push(char::from_u32(cp).unwrap_or('\u{FFFD}'));
					continue;
				}
				out.push(char::from_u32(unit).unwrap_or('\u{FFFD}'));
			}
			_ => return Err(invalid()),
		}
		i += 1;
	}
	Ok(out)
}

/// Pages the elements of a query's single array result; the page stays one
/// array.
fn page_array(array: &str, selector: &JsonSelector, page: &JsonPage) -> String {
	let items = split_compact_array(array);
	let slice: Vec<&str> = items.iter().skip(page.offset).take(page.limit).copied().collect();
	let compact = format!("[{}]", slice.join(","));
	let text = if selector.compact { compact } else { pretty_json(&compact) };
	let remaining = items.len().saturating_sub(page.offset + slice.len());
	if remaining > 0 {
		format!(
			"{text}\n{}",
			continuation_hint(selector, page, page.offset + slice.len(), Some(remaining))
		)
	} else {
		text
	}
}

/// Pages a stream of query results, one value per unit.
fn page_stream(values: &[String], selector: &JsonSelector, page: &JsonPage, more: bool) -> Result<String> {
	let mut lines = Vec::new();
	for value in values.iter().skip(page.offset).take(page.limit) {
		lines.push(render_value(value, selector)?);
	}
	if more {
		lines.push(continuation_hint(selector, page, page.offset + lines.len(), None));
	} else if lines.is_empty() && page.offset > 0 {
		lines.push(format!(
			"[offset {} is past the last result ({} total)]",
			page.offset,
			values.len()
		));
	}
	Ok(lines.join("\n"))
}

/// Puts what jq wrote to stderr, such as an input that failed, ahead of the
/// results.
fn with_stderr(text: String, stderr: &str) -> String {
	if stderr.is_empty() {
		return text;
	}
	let notice = format!("[jq stderr: {stderr}]");
	if text.is_empty() {
		notice
	} else {
		format!("{notice}\n{text}")
	}
}

// ============================================================================
// Text truncation (pi-tui streaming-output.ts, as used by boundJqStderr)
// ============================================================================

/// JavaScript's whitespace set plus U+FEFF, for `String.prototype.trim`.
fn is_js_whitespace(c: char) -> bool {
	matches!(
		c,
		'\t' | '\n'
			| '\u{b}' | '\u{c}'
			| '\r' | ' '
			| '\u{a0}' | '\u{1680}'
			| '\u{2000}'..='\u{200a}'
			| '\u{2028}' | '\u{2029}'
			| '\u{202f}' | '\u{205f}'
			| '\u{3000}' | '\u{feff}'
	)
}

/// `content.trim()`.
fn js_trim(s: &str) -> &str {
	s.trim_matches(is_js_whitespace)
}

/// `s.chars().map(len_utf16).sum()` — JavaScript's `length`.
fn utf16_units(s: &str) -> usize {
	s.chars().map(char::len_utf16).sum()
}

/// The UTF-16 code units of `s` between `from` and `to` (clamped), written
/// the way `Buffer.from(s.substring(from, to), "utf-8")` encodes them: a cut
/// inside a surrogate pair keeps its half, which UTF-8 encoding renders as
/// U+FFFD.
fn js_utf16_slice(s: &str, from: usize, to: usize, out: &mut String) {
	let mut units = 0usize;
	for ch in s.chars() {
		let end = units + ch.len_utf16();
		if end <= from {
			units = end;
			continue;
		}
		if units >= to {
			break;
		}
		if units < from || end > to {
			out.push('\u{FFFD}');
		} else {
			out.push(ch);
		}
		units = end;
	}
}

/// `truncateHeadBytes(text, max)` on a string: the first `max` UTF-16 units,
/// backed off to a UTF-8 boundary under `max` UTF-8 bytes.
fn truncate_head_bytes(text: &str, max: usize) -> (String, usize) {
	if max == 0 {
		return (String::new(), 0);
	}
	if utf16_units(text) <= max && text.len() <= max {
		return (text.to_string(), text.len());
	}
	let mut window = String::new();
	js_utf16_slice(text, 0, max, &mut window);
	if window.len() == utf16_units(&window) || window.len() <= max {
		return (window.clone(), window.len());
	}
	let mut end = max.min(window.len());
	while end > 0 && !window.is_char_boundary(end) {
		end -= 1;
	}
	if end == 0 {
		return (String::new(), 0);
	}
	(window[..end].to_string(), end)
}

/// `truncateTailBytes(text, max)` on a string: the last `max` UTF-16 units,
/// advanced to a UTF-8 boundary so at most `max` UTF-8 bytes remain.
fn truncate_tail_bytes(text: &str, max: usize) -> (String, usize) {
	if max == 0 {
		return (String::new(), 0);
	}
	let units = utf16_units(text);
	if units <= max && text.len() <= max {
		return (text.to_string(), text.len());
	}
	let mut window = String::new();
	js_utf16_slice(text, units.saturating_sub(max), units, &mut window);
	if window.len() == utf16_units(&window) || window.len() <= max {
		return (window.clone(), window.len());
	}
	let mut start = window.len() - max;
	while start < window.len() && !window.is_char_boundary(start) {
		start += 1;
	}
	(window[start..].to_string(), window.len() - start)
}

/// `formatMiddleElisionMarker`.
fn middle_elision_marker(elided_lines: usize, elided_bytes: usize) -> String {
	if elided_lines <= 1 {
		format!("[…{elided_bytes}B elided…]")
	} else {
		format!("[…{elided_lines}ln elided…]")
	}
}

/// `truncateMiddle` on a single line with only a byte limit: a line that does
/// not fit takes the byte-window branch and renders
/// `<head>\n<marker>\n<tail>`; anything else is unchanged.
fn truncate_middle_line(content: &str, max_bytes: usize) -> String {
	if content.len() <= max_bytes {
		return content.to_string();
	}
	let head_bytes = max_bytes / 2;
	let tail_bytes = max_bytes - head_bytes;
	let (head, head_len) = truncate_head_bytes(content, head_bytes.min(content.len()));
	let remaining = content.len().saturating_sub(head_len);
	let (tail, tail_len) = truncate_tail_bytes(content, tail_bytes.min(remaining));
	let elided_bytes = content.len().saturating_sub(head_len + tail_len);
	if elided_bytes == 0 {
		return content.to_string();
	}
	format!("{head}\n{}\n{tail}", middle_elision_marker(0, elided_bytes))
}

/// `truncateTail(content, { maxBytes })`: keep the last whole lines under the
/// byte and line budgets; a line that alone exceeds the budget contributes
/// its byte tail.
fn truncate_tail(content: &str, max_bytes: usize, max_lines: usize) -> String {
	let total_lines = content.bytes().filter(|b| *b == b'\n').count() + 1;
	if total_lines <= max_lines && content.len() <= max_bytes {
		return content.to_string();
	}
	// Line spans in UTF-16 units, as JS indexes strings.
	let mut spans: Vec<(usize, usize)> = Vec::new();
	{
		let mut start = 0usize;
		let mut units = 0usize;
		for ch in content.chars() {
			if ch == '\n' {
				spans.push((start, units));
				start = units + 1;
			}
			units += ch.len_utf16();
		}
		spans.push((start, units));
	}
	let mut bytes_used = 0usize;
	let mut kept: Vec<(usize, usize)> = Vec::new();
	for span in spans.iter().rev() {
		if kept.len() >= max_lines {
			break;
		}
		let sep = i64::from(!kept.is_empty());
		let remaining = max_bytes as i64 - bytes_used as i64 - sep;
		if remaining < 0 {
			break;
		}
		let mut line = String::new();
		js_utf16_slice(content, span.0, span.1, &mut line);
		if line.len() as i64 <= remaining {
			bytes_used += sep as usize + line.len();
			kept.push(*span);
			continue;
		}
		// The line does not fit whole: its byte tail, when it alone exceeds
		// the budget or nothing was kept yet (truncateTail's partial line).
		let line_units = span.1 - span.0;
		let (text, bytes) = if line_units > max_bytes || line.len() > max_bytes {
			truncate_tail_bytes(&line, remaining.max(0) as usize)
		} else {
			(String::new(), 0)
		};
		if bytes > 0 || kept.is_empty() {
			let separator = if !kept.is_empty() && bytes > 0 { "\n" } else { "" };
			let complete_tail = if kept.is_empty() {
				String::new()
			} else {
				let mut tail = String::new();
				for (i, span) in kept.iter().rev().enumerate() {
					if i > 0 {
						tail.push('\n');
					}
					js_utf16_slice(content, span.0, span.1, &mut tail);
				}
				tail
			};
			return format!("{text}{separator}{complete_tail}");
		}
		break;
	}
	let mut out = String::new();
	for (i, span) in kept.iter().rev().enumerate() {
		if i > 0 {
			out.push('\n');
		}
		js_utf16_slice(content, span.0, span.1, &mut out);
	}
	out
}

// ============================================================================
// stderr bounding and sanitizing (read-json.ts boundJqStderr + sanitizeText)
// ============================================================================

/// omp's `boundJqStderr`: trimmed lines middle-elided to
/// [`JSON_QUERY_ERROR_LINE_MAX_BYTES`], the whole to
/// [`JSON_QUERY_ERROR_MAX_BYTES`] from the tail.
fn bound_jq_stderr(stderr: &str) -> String {
	let lines: Vec<String> = js_trim(stderr)
		.split('\n')
		.map(|line| truncate_middle_line(line, JSON_QUERY_ERROR_LINE_MAX_BYTES))
		.collect();
	truncate_tail(&lines.join("\n"), JSON_QUERY_ERROR_MAX_BYTES, TRUNCATION_MAX_LINES)
}

/// omp's `sanitizeText`: ANSI escape sequences stripped, control characters
/// and C1 removed (NUL–BS, VT–US, DEL–9F; tab and newline survive).
fn sanitize_text(text: &str) -> String {
	let mut out = String::with_capacity(text.len());
	let bytes = text.as_bytes();
	let mut i = 0;
	while i < bytes.len() {
		let b = bytes[i];
		if b == 0x1b {
			// ANSI: CSI (ESC [ … final byte), OSC (ESC ] … BEL or ESC \),
			// or a two-byte sequence (ESC plus one byte).
			match bytes.get(i + 1) {
				Some(b'[') => {
					i += 2;
					while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
						i += 1;
					}
					i += usize::from(i < bytes.len());
				}
				Some(b']') => {
					i += 2;
					while i < bytes.len() {
						if bytes[i] == 0x07 {
							i += 1;
							break;
						}
						if bytes[i] == 0x1b && bytes.get(i + 1) == Some(&b'\\') {
							i += 2;
							break;
						}
						i += 1;
					}
				}
				_ => i += 2,
			}
			continue;
		}
		if (0x00..=0x1f).contains(&b) && b != b'\t' && b != b'\n' || (0x7f..=0x9f).contains(&b) {
			i += 1;
			continue;
		}
		match text[i..].chars().next() {
			Some(ch) => {
				out.push(ch);
				i += ch.len_utf8();
			}
			None => break,
		}
	}
	out
}

// ============================================================================
// jq value dumping (jq's jv_dump_term, as omp's vendored jaq-json writes it)
// ============================================================================

/// Why a value could not be written.
enum DumpError {
	/// An object key that is not a string, which JSON cannot express.
	Key(Val),
}

/// `v` as jq writes it compactly.
fn dump(v: &Val) -> std::result::Result<String, DumpError> {
	let mut out = String::new();
	write_val(&mut out, v).map(|()| out)
}

fn write_val(out: &mut String, v: &Val) -> std::result::Result<(), DumpError> {
	match v {
		Val::Null => out.push_str("null"),
		Val::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
		Val::Num(n) => write_num(out, n),
		Val::TStr(s) | Val::BStr(s) => write_json_string(out, s),
		Val::Arr(a) => {
			out.push('[');
			for (i, x) in a.iter().enumerate() {
				if i != 0 {
					out.push(',');
				}
				write_val(out, x)?;
			}
			out.push(']');
		}
		Val::Obj(o) => {
			out.push('{');
			for (i, (k, v)) in o.iter().enumerate() {
				if i != 0 {
					out.push(',');
				}
				let (Val::TStr(k) | Val::BStr(k)) = k else {
					return Err(DumpError::Key(k.clone()));
				};
				write_json_string(out, k);
				out.push(':');
				write_val(out, v)?;
			}
			out.push('}');
		}
	}
	Ok(())
}

/// A string as jq's `jvp_dump_string` writes it: `"` `\` `\b` `\t` `\r` `\n`
/// `\f` get backslash escapes, other ASCII controls and DEL `\u00XX`, invalid
/// UTF-8 becomes U+FFFD.
fn write_json_string(out: &mut String, s: &[u8]) {
	out.push('"');
	let mut start = 0;
	let mut i = 0;
	while i < s.len() {
		let c = s[i];
		if (0x20..0x7f).contains(&c) && c != b'"' && c != b'\\' {
			i += 1;
			continue;
		}
		out.push_str(std::str::from_utf8(&s[start..i]).unwrap_or("\u{FFFD}"));
		if c < 0x80 {
			match c {
				b'"' => out.push_str("\\\""),
				b'\\' => out.push_str("\\\\"),
				0x08 => out.push_str("\\b"),
				b'\t' => out.push_str("\\t"),
				b'\r' => out.push_str("\\r"),
				b'\n' => out.push_str("\\n"),
				0x0c => out.push_str("\\f"),
				_ => {
					let _ = write!(out, "\\u{c:04x}");
				}
			}
			i += 1;
		} else {
			let (valid, len) = utf8_next(&s[i..]);
			if valid {
				out.push_str(std::str::from_utf8(&s[i..i + len]).unwrap_or("\u{FFFD}"));
			} else {
				out.push('\u{FFFD}');
			}
			i += len;
		}
		start = i;
	}
	out.push_str(std::str::from_utf8(&s[start..]).unwrap_or("\u{FFFD}"));
	out.push('"');
}

/// Decode the UTF-8 sequence at the start of non-empty `b` as jq's
/// `jvp_utf8_next`: whether it is valid and its length (invalid sequences
/// span the bytes jq skips over at once).
fn utf8_next(b: &[u8]) -> (bool, usize) {
	let first = b[0];
	let len = match first {
		0x00..=0x7f => return (true, 1),
		0xc2..=0xdf => 2,
		0xe0..=0xef => 3,
		0xf0..=0xf4 => 4,
		_ => return (false, 1),
	};
	if len > b.len() {
		return (false, b.len());
	}
	let mut c = u32::from(first) & (0x7f >> len);
	for (i, &ch) in b[1..len].iter().enumerate() {
		if ch & 0xc0 != 0x80 {
			return (false, i + 1);
		}
		c = (c << 6) | u32::from(ch & 0x3f);
	}
	let min = [0, 0, 0x80, 0x800, 0x10000][len];
	(c >= min && !(0xd800..=0xdfff).contains(&c) && c <= 0x10ffff, len)
}

/// `n` as jq writes it (`jv_dump_term`): integer and decimal forms as
/// upstream jaq keeps them; floats as `jvp_dtoa_fmt`.
fn write_num(out: &mut String, n: &Num) {
	match n {
		Num::Int(i) => {
			let _ = write!(out, "{i}");
		}
		Num::BigInt(i) => {
			let _ = write!(out, "{i}");
		}
		Num::Dec(d) => out.push_str(d),
		Num::Float(x) => write_f64(out, *x),
	}
}

/// A double as jq's `jvp_dtoa_fmt` writes it: the shortest digits that
/// round-trip, in exponential notation (`1e+17`, `1e-05`) when the decimal
/// point is more than 15 places after the digits or 4 or more places before
/// them; NaN is `null`, infinities clamp.
fn write_f64(out: &mut String, x: f64) {
	if x.is_nan() {
		out.push_str("null");
		return;
	}
	let x = x.clamp(-f64::MAX, f64::MAX);
	if x.is_sign_negative() {
		out.push('-');
	}
	if x == 0.0 {
		out.push('0');
		return;
	}
	let mut buf = ryu_js::Buffer::new();
	let s = buf.format_finite(x.abs());
	// Shortest digits and the position of the decimal point, as vendored
	// fmt_f64 derives them from ryu's `d.ddd e±nn`.
	let (mant, exp) = match s.split_once('e') {
		Some((m, e)) => (m, e.parse::<i32>().unwrap_or(0)),
		None => (s, 0),
	};
	let (int, frac) = mant.split_once('.').unwrap_or((mant, ""));
	let mut decpt = int.len() as i32 + exp;
	let mut digits: Vec<u8> = Vec::with_capacity(20);
	for c in int.bytes().chain(frac.bytes()) {
		if digits.is_empty() && c == b'0' {
			decpt -= 1;
		} else {
			digits.push(c);
		}
	}
	while digits.last() == Some(&b'0') {
		digits.pop();
	}
	let digits = String::from_utf8(digits).unwrap_or_default();
	let n = digits.len() as i32;
	if decpt <= -4 || decpt > n + 15 {
		let (first, rest) = digits.split_at(1);
		out.push_str(first);
		if !rest.is_empty() {
			let _ = write!(out, ".{rest}");
		}
		let e = decpt - 1;
		let _ = write!(out, "e{}{:02}", if e < 0 { '-' } else { '+' }, e.unsigned_abs());
	} else if decpt <= 0 {
		out.push_str("0.");
		for _ in 0..-decpt {
			out.push('0');
		}
		out.push_str(&digits);
	} else if decpt < n {
		let (int, frac) = digits.split_at(decpt as usize);
		let _ = write!(out, "{int}.{frac}");
	} else {
		out.push_str(&digits);
		for _ in 0..decpt - n {
			out.push('0');
		}
	}
}

/// jq's `jv_kind_name`.
fn kind_name(v: &Val) -> &'static str {
	match v {
		Val::Null => "null",
		Val::Bool(_) => "boolean",
		Val::Num(_) => "number",
		Val::TStr(_) | Val::BStr(_) => "string",
		Val::Arr(_) => "array",
		Val::Obj(_) => "object",
	}
}

/// The buffer jq abbreviates a value into for its messages
/// (`char errbuf[30]`), including the C string's NUL.
const ERRBUF: usize = 30;

/// `v` dumped compactly and cut to fit a C buffer of `bufsize` bytes, ending
/// in `...` and the closing delimiter when cut (`jv_dump_string_trunc`).
fn dump_trunc(v: &Val, bufsize: usize) -> String {
	// a long string is cut to its first `bufsize` codepoints before dumping,
	// as jq does, so that dumping never escapes more than can be shown
	let dumped = match v {
		Val::TStr(s) | Val::BStr(s) if s.len() > bufsize => {
			let mut end = 0usize;
			let mut count = 0usize;
			while end < s.len() && count < bufsize {
				end += utf8_next(&s[end..]).1;
				count += 1;
			}
			dump(&Val::utf8_str(s[..end].to_vec())).unwrap_or_default()
		}
		v => dump(v).unwrap_or_default(),
	};
	if dumped.len() < bufsize {
		return dumped;
	}
	if bufsize < 8 {
		return String::from_utf8_lossy(&dumped.as_bytes()[..bufsize - 1]).into_owned();
	}
	let delim = match dumped.as_bytes()[0] {
		b'"' => Some('"'),
		b'[' => Some(']'),
		b'{' => Some('}'),
		_ => None,
	};
	let mut l = bufsize - if delim.is_some() { 5 } else { 4 };
	while l > 0 && !dumped.is_char_boundary(l) {
		l -= 1;
	}
	let mut out = String::from(&dumped[..l]);
	out.push_str("...");
	if let Some(d) = delim {
		out.push(d);
	}
	out
}

/// `v` as jq's messages show it: `<kind> (<abbreviated dump>)`.
fn describe(v: &Val) -> String {
	format!("{} ({})", kind_name(v), dump_trunc(v, ERRBUF))
}

// ============================================================================
// Filter compilation (omp's filter::parse_compile, reduced)
// ============================================================================

/// Data kind of the engine's filters: jaq-json values, with natives reaching
/// the run state through [`Data`].
struct Kind;

impl DataT for Kind {
	type Data<'a> = &'a Data<'a>;
	type V<'a> = Val;
}

/// What natives see while a filter runs.
struct Data<'a> {
	lut: &'a jaq_core::Lut<Kind>,
	inputs: jaq_std::input::Inputs<'a, Val>,
	session: &'a Session,
}

impl<'a> data::HasLut<'a, Kind> for &'a Data<'a> {
	fn lut(&self) -> &'a jaq_core::Lut<Kind> {
		self.lut
	}
}

impl<'a> jaq_std::input::HasInputs<'a, Val> for &'a Data<'a> {
	fn inputs(&self) -> jaq_std::input::Inputs<'a, Val> {
		self.inputs
	}
}

/// jq builtins that write to the result's stderr, report the input position,
/// or format as jq's `f_format` does. Registered first: the compiler binds a
/// call to the first native with a matching name and arity, and these names
/// are shadowed by definitions in [`DEFS`].
fn natives() -> [native::Filter<RunPtr<Kind>>; 8] {
	[
		("env", native::v(0), |_cv| {
			let vars = std::env::vars_os().map(|(k, v)| {
				(
					Val::from(k.to_string_lossy().into_owned()),
					Val::from(v.to_string_lossy().into_owned()),
				)
			});
			box_once(Ok(Val::obj(vars.collect())))
		}),
		("stderr_", native::v(0), |cv| {
			let session = cv.0.data().session;
			let done = match &cv.1 {
				Val::TStr(s) | Val::BStr(s) => {
					session.write_stderr(&String::from_utf8_lossy(&s[..]));
					Ok(())
				}
				v => message(v).map(|json| session.write_stderr(&json)),
			};
			box_once(done.map(|()| cv.1).map_err(Exn::from))
		}),
		("halt_error_", native::v(1), |mut cv| {
			let session = cv.0.data().session;
			let code = cv.0.pop_var();
			let Some(code) = code.as_f64() else {
				let e = CoreError::str(format!("{} halt_error/1: number required", describe(&code)));
				return box_once(Err(Exn::from(e)));
			};
			let done = match &cv.1 {
				Val::Null => Ok(()),
				Val::TStr(s) | Val::BStr(s) => {
					session.write_stderr(&String::from_utf8_lossy(&s[..]));
					Ok(())
				}
				v => message(v).map(|mut json| {
					json.push('\n');
					session.write_stderr(&json);
				}),
			};
			box_once(Err(done.map_or_else(Exn::from, |()| Exn::halt(code as i32))))
		}),
		("input_filename", native::v(0), |_cv| {
			box_once(Ok(Val::from(INPUT_NAME.to_string())))
		}),
		("input_line_number", native::v(0), |cv| {
			box_once(Ok(Val::from(cv.0.data().session.position.get())))
		}),
		("tonumber_", native::v(0), |cv| box_once(tonumber(cv.1).map_err(Exn::from))),
		("@csv", native::v(0), |cv| box_once(table_row(&cv.1, true))),
		("@tsv", native::v(0), |cv| box_once(table_row(&cv.1, false))),
	]
}

/// `v` as jq writes it in messages: compact JSON.
fn message(v: &Val) -> std::result::Result<String, CoreError<Val>> {
	dump(v).map_err(|e| match e {
		DumpError::Key(key) => object_key(&key),
	})
}

/// The error about a non-string object key `k` (jq's `jv_object_set` paths).
fn object_key(k: &Val) -> CoreError<Val> {
	CoreError::str(format!("Cannot use {} as object key", describe(k)))
}

/// jq's `f_tonumber`: numbers pass through; a string must be exactly one
/// number literal (no whitespace, no NUL).
fn tonumber(v: Val) -> std::result::Result<Val, CoreError<Val>> {
	let num = match &v {
		Val::Num(_) => return Ok(v.clone()),
		Val::TStr(s) | Val::BStr(s) if !s.contains(&0) => jaq_json::read::parse_single_num(s),
		_ => None,
	};
	num.map(Val::Num)
		.ok_or_else(|| CoreError::str(format!("{} cannot be parsed as a number", describe(&v))))
}

/// jq's `@csv` and `@tsv` (`f_format`): arrays of scalars; null and NaN are
/// empty fields.
fn table_row(v: &Val, csv: bool) -> jaq_core::ValX<'static, Val> {
	let Val::Arr(row) = v else {
		let msg = if csv {
			"cannot be csv-formatted, only array"
		} else {
			"cannot be tsv-formatted, only array"
		};
		return Err(CoreError::str(format!("{} {msg}", describe(v))).into());
	};
	let mut buf = Vec::new();
	for (i, field) in row.iter().enumerate() {
		if i != 0 {
			buf.push(if csv { b',' } else { b'\t' });
		}
		match field {
			Val::Null => {}
			Val::Num(_) if field.as_f64().is_some_and(f64::is_nan) => {}
			Val::TStr(s) | Val::BStr(s) => {
				// jq's `escape_string`, which also writes NUL as `\0`
				if csv {
					buf.push(b'"');
				}
				for byte in s.iter() {
					match byte {
						0 => buf.extend_from_slice(b"\\0"),
						b'"' if csv => buf.extend_from_slice(b"\"\""),
						b'\t' if !csv => buf.extend_from_slice(b"\\t"),
						b'\n' if !csv => buf.extend_from_slice(b"\\n"),
						b'\r' if !csv => buf.extend_from_slice(b"\\r"),
						b'\\' if !csv => buf.extend_from_slice(b"\\\\"),
						byte => buf.push(*byte),
					}
				}
				if csv {
					buf.push(b'"');
				}
			}
			Val::Bool(_) | Val::Num(_) => buf.extend(message(field)?.into_bytes()),
			Val::Arr(_) | Val::Obj(_) => {
				return Err(CoreError::str(format!("{} is not valid in a csv row", describe(field))).into());
			}
		}
	}
	Ok(Val::utf8_str(buf))
}

/// jq builtins jaq lacks or defines differently, appended to the prelude so
/// they shadow jaq's definitions: `stderr` and `debug` feed the collected
/// stderr, `halt_error` halts with jq's exit status, `tonumber` accepts
/// exactly one number literal, `builtins` lists jq's names.
const DEFS: &str = r#"
def stderr: stderr_;
def debug: (["DEBUG:", .] | stderr_ | empty), .;
def debug(msgs): (msgs | debug | empty), .;
def halt_error($exit_code): (stderr_ | empty), halt_error_($exit_code), empty;
def halt_error: halt_error(5);
def tonumber: tonumber_;
"#;

/// jq's `builtins` (the list jq 1.8.2 itself reports, verbatim).
const JQ_BUILTINS: &str = r#"["have_literal_numbers/0","have_decnum/0","input_line_number/0","input_filename/0","now/0","localtime/0","gmtime/0","mktime/0","strflocaltime/1","strftime/1","strptime/1","stderr/0","debug/0","input/0","modulemeta/0","get_jq_origin/0","get_prog_origin/0","get_search_list/0","halt_error/1","halt/0","env/0","format/1","error/0","max/0","min/0","bsearch/1","unique/0","sort/0","nan/0","infinite/0","isnormal/0","isnan/0","isinfinite/0","type/0","utf8bytelength/0","length/0","contains/1","has/1","delpaths/1","getpath/1","setpath/2","rtrim/0","ltrim/0","trim/0","implode/0","explode/0","split/1","endswith/1","startswith/1","keys_unsorted/0","keys/0","tostring/0","toboolean/0","tonumber/0","fromjson/0","tojson/0","lgamma_r/0","frexp/0","modf/0","ldexp/2","trunc/0","significand/0","scalbln/2","scalb/2","round/0","rint/0","nexttoward/2","nextafter/2","nearbyint/0","logb/0","log1p/0","lgamma/0","gamma/0","fmod/2","fmin/2","fmax/2","fma/3","fdim/2","fabs/0","expm1/0","exp10/0","erfc/0","erf/0","drem/2","copysign/2","ceil/0","yn/2","jn/2","y1/0","y0/0","tgamma/0","tanh/0","tan/0","sqrt/0","sinh/0","sin/0","remainder/2","pow/2","log2/0","log10/0","log/0","j1/0","j0/0","hypot/2","floor/0","exp2/0","exp/0","cosh/0","cos/0","cbrt/0","atanh/0","atan2/2","atan/0","asinh/0","asin/0","acosh/0","acos/0","empty/0","not/0","path/1","last/1","range/2","halt_error/0","error/1","map/1","select/1","sort_by/1","group_by/1","unique_by/1","max_by/1","min_by/1","add/1","add/0","del/1","abs/0","map_values/1","recurse/1","recurse/2","recurse/0","to_entries/0","from_entries/0","with_entries/1","reverse/0","indices/1","index/1","rindex/1","paths/0","paths/1","isfinite/0","arrays/0","objects/0","iterables/0","booleans/0","numbers/0","normals/0","finites/0","strings/0","nulls/0","values/0","scalars/0","join/1","flatten/1","flatten/0","range/1","fromdateiso8601/0","todateiso8601/0","fromdate/0","todate/0","ltrimstr/1","rtrimstr/1","trimstr/1","match/2","match/1","test/2","test/1","capture/2","capture/1","scan/2","scan/1","splits/2","splits/1","split/2","sub/3","sub/2","gsub/3","gsub/2","while/2","until/2","limit/2","skip/2","range/3","first/1","isempty/1","all/2","any/2","all/1","any/1","all/0","any/0","nth/2","first/0","last/0","nth/1","combinations/0","combinations/1","transpose/0","in/1","inside/1","repeat/1","inputs/0","ascii_downcase/0","ascii_upcase/0","truncate_stream/1","fromstream/1","tostream/0","walk/1","pick/1","debug/1","INDEX/2","INDEX/1","JOIN/2","JOIN/3","JOIN/4","IN/1","IN/2","builtins/0"]"#;

/// What escaped a filter: a jq error value, or a halt code.
enum RunError {
	Jq(CoreError<Val>),
	Halt(i32),
}

impl From<jaq_core::Exn<'_, Val>> for RunError {
	fn from(exn: jaq_core::Exn<'_, Val>) -> Self {
		match exn.get_err() {
			Ok(e) => Self::Jq(e),
			Err(exn) => Self::Halt(exn.get_halt().unwrap_or(0)),
		}
	}
}

/// Per-invocation state natives and the run loop share: collected stderr and
/// the position of the current input.
struct Session {
	stderr: RefCell<String>,
	/// Newline-terminated records consumed from the input stream so far.
	position: Cell<usize>,
}

impl Session {
	fn write_stderr(&self, text: &str) {
		self.stderr.borrow_mut().push_str(text);
	}

	/// Reports an uncaught error as jq's `process` does.
	fn report_uncaught(&self, error: CoreError<Val>) {
		let at = format!("{INPUT_NAME}:{}", self.position.get());
		let line = match error.into_val() {
			Val::TStr(msg) | Val::BStr(msg) => {
				// printed with `%s`: up to a NUL
				let msg = msg.split(|b| *b == 0).next().unwrap_or_default();
				format!("jq: error (at {at}): {}\n", String::from_utf8_lossy(msg))
			}
			v => format!(
				"jq: error (at {at}) (not a string): {}\n",
				dump(&v).unwrap_or_default()
			),
		};
		self.write_stderr(&line);
	}
}

/// One report in a compile-error listing: the message, its span in the
/// program, and whether a blank line follows.
struct Report {
	message: String,
	span: Option<core::ops::Range<usize>>,
	blank: bool,
}

/// The compile errors of one file, displayed as jq reports them, one
/// `jq: error: ...` message each, without the closing error count.
struct FileReports {
	code: String,
	reports: Vec<Report>,
}

impl std::fmt::Display for FileReports {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		for report in &self.reports {
			let Some(range) = &report.span else {
				if report.blank && report.message.starts_with("jq: ") {
					writeln!(f, "{}", report.message)?;
				} else {
					writeln!(f, "jq: error: {}", report.message)?;
				}
				if report.blank {
					writeln!(f)?;
				}
				continue;
			};
			// as jq's `locfile_locate`
			let code = self.code.as_bytes();
			let start = range.start.min(code.len());
			let offset = memchr::memrchr(b'\n', &code[..start]).map_or(0, |i| i + 1);
			let line_end = memchr::memchr(b'\n', &code[start..]).map_or(code.len(), |i| start + i);
			let line = memchr::memchr_iter(b'\n', &code[..start]).count() + 1;
			let end = range.end.min(line_end.max(start + 1)).max(start);
			let text = String::from_utf8_lossy(&code[offset..line_end]);
			writeln!(
				f,
				"jq: error: {} at <top-level>, line {line}, column {}:\n    {text}\n    {}{}",
				report.message,
				start - offset + 1,
				" ".repeat(start - offset),
				"^".repeat(end - start),
			)?;
		}
		Ok(())
	}
}

fn load_errors(errs: load::Errors<&str, ()>) -> Vec<FileReports> {
	errs
		.into_iter()
		.map(|(file, err)| {
			let reports = match err {
				load::Error::Io(errs) => errs
					.into_iter()
					.map(|(_, message)| Report {
						message,
						span: None,
						blank: true,
					})
					.collect(),
				load::Error::Lex(errs) => errs
					.into_iter()
					.map(|(expected, found)| Report {
						message: format!("expected {}", expected.as_str()),
						span: Some(load::span(file.code, found)),
						blank: false,
					})
					.collect(),
				load::Error::Parse(errs) => errs
					.into_iter()
					.map(|(expected, found_text)| {
						let message = if found_text.is_empty() {
							format!("expected {}", expected.as_str())
						} else {
							format!("expected {}, found {}", expected.as_str(), found_text)
						};
						Report {
							message,
							span: Some(load::span(file.code, found_text)),
							blank: false,
						}
					})
					.collect(),
			};
			FileReports {
				code: file.code.to_string(),
				reports,
			}
		})
		.collect()
}

fn compile_errors(errs: compile::Errors<&str, ()>) -> Vec<FileReports> {
	errs
		.into_iter()
		.map(|(file, errs)| {
			let reports = errs
				.into_iter()
				.map(|(name, undefined)| {
					let message = match undefined {
						compile::Undefined::Filter(arity) => format!("{name}/{arity} is not defined"),
						_ => format!("{name} is not defined"),
					};
					Report {
						message,
						span: Some(load::span(file.code, name)),
						blank: false,
					}
				})
				.collect();
			FileReports {
				code: file.code.to_string(),
				reports,
			}
		})
		.collect()
}

/// Compile errors as jq reports them: each, then their count.
fn report_compile_errors(reports: &[FileReports]) -> String {
	let mut text: String = reports.iter().map(ToString::to_string).collect();
	let n = text.lines().filter(|line| line.starts_with("jq: error")).count();
	let s = if n == 1 { "" } else { "s" };
	let _ = writeln!(text, "jq: {n} compile error{s}");
	text
}

/// Compile `query` as jq's `main` does: prelude defs, our natives, then
/// `jaq_core`/`jaq_std`/`jaq_json`, with `$JQ_BUILD_CONFIGURATION`, `$ARGS`
/// and `$ENV` bound.
fn compile_filter(query: &str) -> std::result::Result<(jaq_core::Filter<Kind>, Vec<Val>), Vec<FileReports>> {
	use load::{Arena, File, Loader};

	let mut defs: Vec<_> = jaq_core::defs()
		.chain(jaq_std::defs())
		.chain(jaq_json::defs())
		.collect();
	// `builtins` reports jq's own list, not what jaq provides.
	let defs_text = format!("{DEFS}\ndef builtins: {JQ_BUILTINS};");
	defs.extend(load::parse(&defs_text, |p| p.defs()).unwrap_or_default());

	let funs: Vec<_> = natives()
		.into_iter()
		.map(native::run::<Kind>)
		.chain(jaq_core::funs())
		.chain(jaq_std::funs())
		.chain(jaq_json::funs())
		.chain(jaq_std::input::funs::<Kind>().into_vec().into_iter().map(native::run::<Kind>))
		.collect();

	let arena = Arena::default();
	let loader = Loader::new(defs);
	let main = File { path: (), code: query };
	let modules = loader.load(&arena, main).map_err(load_errors)?;

	let compiler = compile::Compiler::default()
		.with_funs(funs)
		.with_global_vars(["$JQ_BUILD_CONFIGURATION", "$ARGS", "$ENV"]);
	let filter = compiler.compile(modules).map_err(compile_errors)?;

	let env = Val::obj(
		std::env::vars_os()
			.map(|(k, v)| {
				(
					Val::from(k.to_string_lossy().into_owned()),
					Val::from(v.to_string_lossy().into_owned()),
				)
			})
			.collect(),
	);
	let named_obj = Val::obj(
		[(
			Val::from("JQ_BUILD_CONFIGURATION".to_string()),
			Val::from(BUILD_CONFIGURATION.to_string()),
		)]
		.into_iter()
		.collect(),
	);
	let args = Val::obj(
		[
			(Val::from("positional".to_string()), Val::Arr(Rc::new(Vec::new()))),
			(Val::from("named".to_string()), named_obj),
		]
		.into_iter()
		.collect(),
	);
	let vars = vec![Val::from(BUILD_CONFIGURATION.to_string()), args, env];
	Ok((filter, vars))
}

// ============================================================================
// Execution (read-json.ts runJq + executeJsonQuery)
// ============================================================================

/// Convert one projected record into jaq's value type: f64 numbers and
/// clipped strings as marked text.
fn to_val(v: &Value) -> Val {
	match v {
		Value::Null => Val::Null,
		Value::Bool(b) => Val::Bool(*b),
		Value::Num(n) => Val::Num(Num::Float(*n)),
		Value::Str(s) => Val::utf8_str(s.clone()),
		Value::Clipped(c) => Val::utf8_str(c.marked()),
		Value::Arr(a) => Val::Arr(Rc::new(a.iter().map(to_val).collect())),
		Value::Obj(o) => Val::obj(
			o.iter()
				.map(|(k, v)| (Val::utf8_str(k.to_string()), to_val(v)))
				.collect(),
		),
	}
}

/// The output of evaluating one filter over the records: captured text (as
/// the TS captures jq's stdout, capped at the capture cap), whether capture
/// stopped early, the collected stderr, and jq's exit code.
struct RunOutput {
	output: String,
	stopped: Option<JqStop>,
	stderr: String,
	code: i32,
}

/// Render `v` the way jq prints it under `render`'s flags (one output,
/// without its trailing newline).
fn render_output(v: &Val, render: &Render<'_>) -> std::result::Result<String, DumpError> {
	match render {
		Render::Compact => dump(v),
		Render::Selector(selector) => {
			let text = dump(v)?;
			Ok(match (selector.raw, selector.compact, &v) {
				(true, _, Val::TStr(s) | Val::BStr(s)) => String::from_utf8_lossy(&s[..]).into_owned(),
				(_, true, _) => text,
				_ => pretty_json(&text),
			})
		}
	}
}

/// Run `filter` over `records` as jq's input stream, capturing output as
/// read-json.ts does: `stop_after_lines` ends capture once the page plus one
/// lookahead line is in (page-full wins over the capture cap), and both modes
/// stop at the capture cap.
fn run_jq(
	records: &[Value],
	filter: &jaq_core::Filter<Kind>,
	vars: Vec<Val>,
	render: Render<'_>,
	stop_after_lines: Option<usize>,
) -> RunOutput {
	let session = Session {
		stderr: RefCell::new(String::new()),
		position: Cell::new(0),
	};
	// The input stream both the main loop and `input`/`inputs` draw from;
	// each record consumed advances the error position like jq's line count.
	let mut at = 0usize;
	let position = &session.position;
	let iter = core::iter::from_fn(move || {
		if at >= records.len() {
			return None;
		}
		let v = to_val(&records[at]);
		at += 1;
		position.set(at);
		Some(Ok(v))
	});
	let iter = RcIter::new(iter);
	let data = Data {
		lut: &filter.lut,
		inputs: &iter,
		session: &session,
	};
	let ctx = Ctx::<Kind>::new(&data, Vars::new(vars));

	// jq's process statuses (main.c): 0 ok, -1 the last output was null or
	// false, -4 no output yet, 5 after an error, halt codes as given.
	let mut ret = -4i32;
	let mut output = String::new();
	let mut output_units = 0usize;
	let mut lines = 0usize;
	let mut stopped = None;
	let stop_after_lines = stop_after_lines.unwrap_or(usize::MAX);

	'run: for input in &iter {
		let Ok(input) = input else {
			// The record stream cannot fail to parse, so jq's parse-error
			// path is unreachable here.
			continue;
		};
		let mut outputs = filter.id.run((ctx.clone(), input));
		let mut status = -4i32;
		for output_val in &mut outputs {
			let rendered = match output_val {
				Ok(v) => match render_output(&v, &render) {
					Ok(text) => {
						status = if matches!(v, Val::Null | Val::Bool(false)) { -1 } else { 0 };
						text
					}
					Err(DumpError::Key(key)) => {
						session.report_uncaught(object_key(&key));
						status = 5;
						break;
					}
				},
				Err(exn) => match RunError::from(exn) {
					RunError::Jq(e) => {
						session.report_uncaught(e);
						status = 5;
						break;
					}
					RunError::Halt(code) => {
						ret = code;
						break 'run;
					}
				},
			};
			// read-json.ts's capture callback: the chunk (here the rendered
			// value plus its newline) is cut to the room left, then
			// page-full wins over the capture cap.
			let room = MAX_QUERY_CAPTURE_CHARS.saturating_sub(output_units);
			let mut piece = String::new();
			js_utf16_slice(&rendered, 0, room, &mut piece);
			let piece_units = utf16_units(&piece);
			output.push_str(&piece);
			output_units += piece_units;
			if room > piece_units {
				output.push('\n');
				output_units += 1;
				if stop_after_lines != usize::MAX {
					lines += 1;
					if lines >= stop_after_lines {
						stopped = Some(JqStop::PageFull);
						break 'run;
					}
				}
			}
			if output_units >= MAX_QUERY_CAPTURE_CHARS {
				stopped = Some(JqStop::CaptureCap);
				break 'run;
			}
		}
		ret = status;
	}

	RunOutput {
		output,
		stopped,
		stderr: session.stderr.take(),
		code: ret.max(0) & 0xff,
	}
}

/// Runs `selector` over `records` as jq's input stream and renders the output
/// as omp's read does.
///
/// Without a page, returns jq's output verbatim (minus the final newline).
/// With one, values are captured compact so each line is exactly one value,
/// stopping once the page plus one lookahead value is captured; a single
/// array result pages its elements instead. Anything jq wrote to stderr
/// leads the result.
///
/// ## Differences from omp's patched jaq (its `jq` builtin)
///
/// The interpreter is crates.io jaq, so inside the filter language some
/// behaviors follow jaq rather than jq 1.8.2: parse and type-error wording,
/// operator and `|=` semantics, the regex engine, module imports, and
/// number-literal canonicalization (`1e3` prints `1e3`, not `1E+3`; a literal
/// too large prints verbatim rather than clamping). `input` at end of stream
/// yields nothing instead of jq's `"break"` error. Input records are already
/// parsed `Value`s, so number literals in the data are f64-rounded (a literal
/// `1.500` prints `1.5`), which omp's jq preserves. There is no query
/// timeout: a diverging filter runs forever, where omp's jq is killed after
/// 30 s.
pub fn execute(records: &[Value], selector: &JsonSelector) -> Result<String> {
	let (filter, vars) = compile_filter(&selector.query).map_err(|reports| {
		let text = report_compile_errors(&reports);
		EngineError::bad_request(failed_query(&text, 3))
	})?;

	match selector.page {
		None => {
			let run = run_jq(records, &filter, vars, Render::Selector(selector), None);
			if run.stopped.is_none() && run.code != 0 {
				return Err(EngineError::bad_request(failed_query(&run.stderr, run.code)));
			}
			let stderr = bound_jq_stderr(&sanitize_text(&run.stderr));
			let output = run.output.strip_suffix('\n').unwrap_or(&run.output).to_string();
			Ok(with_stderr(output, &stderr))
		}
		Some(page) => {
			// jq runs compact so each line is exactly one value, and stops
			// once the page plus one lookahead value is captured.
			let run = run_jq(records, &filter, vars, Render::Compact, Some(page.offset + page.limit + 1));
			if run.stopped.is_none() && run.code != 0 {
				return Err(EngineError::bad_request(failed_query(&run.stderr, run.code)));
			}
			let stderr = bound_jq_stderr(&sanitize_text(&run.stderr));
			// jq ends every compact value with a newline; text after the
			// last one is a cut-off value.
			let mut values: Vec<String> = run.output.split('\n').map(str::to_string).collect();
			let tail = values.pop().unwrap_or_default();
			if !tail.is_empty() && run.stopped.is_none() {
				values.push(tail);
			}
			if run.stopped == Some(JqStop::CaptureCap) && values.len() <= page.offset {
				return Err(EngineError::bad_request(format!(
					"JSON query output before the requested page exceeds {} MB; narrow the filter or stream large arrays with .[]",
					MAX_QUERY_CAPTURE_CHARS / (1024 * 1024),
				)));
			}
			let text = if run.stopped.is_none() && values.len() == 1 && values[0].starts_with('[') {
				page_array(&values[0], selector, &page)
			} else {
				page_stream(
					&values,
					selector,
					&page,
					run.stopped.is_some() || values.len() > page.offset + page.limit,
				)?
			};
			Ok(with_stderr(text, &stderr))
		}
	}
}

/// `Failed to execute JSON query: <bounded stderr>`, or jq's exit code when
/// stderr stayed empty (read-json.ts's ToolError text).
fn failed_query(stderr: &str, code: i32) -> String {
	let bounded = bound_jq_stderr(&sanitize_text(stderr));
	if bounded.is_empty() {
		format!("Failed to execute JSON query: jq exited with code {code}")
	} else {
		format!("Failed to execute JSON query: {bounded}")
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::json;

	fn sel(query_string: &str) -> JsonSelector {
		parse_selector(query_string).unwrap()
	}

	fn records(jsonl: &str) -> Vec<Value> {
		jsonl
			.lines()
			.map(|line| from_serde(serde_json::from_str::<serde_json::Value>(line).unwrap()))
			.collect()
	}

	fn from_serde(v: serde_json::Value) -> Value {
		match v {
			serde_json::Value::Null => Value::Null,
			serde_json::Value::Bool(b) => Value::Bool(b),
			serde_json::Value::Number(n) => Value::Num(n.as_f64().unwrap_or(0.0)),
			serde_json::Value::String(s) => Value::Str(s),
			serde_json::Value::Array(a) => Value::Arr(a.into_iter().map(from_serde).collect()),
			serde_json::Value::Object(o) => {
				let mut obj = json::Obj::new();
				for (k, v) in o {
					obj.insert(k, from_serde(v));
				}
				Value::Obj(obj)
			}
		}
	}

	#[test]
	fn selector_q_and_query() {
		let s = sel("q=.name&raw=true");
		assert_eq!(s.query, ".name");
		assert!(s.raw);
		assert_eq!(sel("query=.name").query, ".name");
		assert!(parse_selector("x=1").is_none());
		// modifiers alone imply `.`
		assert_eq!(sel("raw=true").query, ".");
		assert!(sel("raw=true").raw);
	}

	#[test]
	fn selector_plus_and_bare_percent() {
		// `+` is never form-decoded in `q`
		assert_eq!(sel("q=.a+.b").query, ".a+.b");
		assert_eq!(sel("q=a+b").query, "a+b");
		// percent-encoded input decodes
		assert_eq!(sel("q=.%25.x").query, ".%.x");
		// a bare `%` is not valid percent-encoding: taken verbatim
		assert_eq!(sel("q=.%x").query, ".%x");
		assert_eq!(sel("q=.%25").query, ".%");
	}

	#[test]
	fn selector_flags_and_bounds() {
		let s = sel("q=.&compact=1&offset=2&limit=10");
		assert!(s.compact);
		assert_eq!(s.page, Some(JsonPage { offset: 2, limit: 10 }));
		// limit bounded at 1000, offset defaults to 0
		assert_eq!(sel("q=.&limit=9999").page, Some(JsonPage { offset: 0, limit: 1000 }));
		// an invalid limit parses to undefined: no page at all
		assert_eq!(sel("q=.&limit=x").page, None);
		// offset below its minimum likewise
		assert_eq!(sel("q=.&offset=-1").page, None);
		assert_eq!(sel("q=.&offset=-1&limit=5").page, Some(JsonPage { offset: 0, limit: 5 }));
		// flags are case-insensitive
		assert!(sel("q=.&raw=TRUE").raw);
		assert!(!sel("q=.&raw=yes").raw);
	}

	#[test]
	fn execute_compact_stream() {
		let out = execute(&records("{\"a\":1}\n{\"a\":2}"), &sel("q=.a")).unwrap();
		assert_eq!(out, "1\n2");
		let out = execute(&records("{\"a\":1}\n{\"a\":2}"), &sel("q=.&compact=true")).unwrap();
		assert_eq!(out, "{\"a\":1}\n{\"a\":2}");
	}

	#[test]
	fn execute_pretty_and_raw() {
		let r = &records("{\"a\":{\"b\":1}}");
		assert_eq!(execute(r, &sel("q=.")).unwrap(), "{\n  \"a\": {\n    \"b\": 1\n  }\n}");
		assert_eq!(execute(&records("\"hi\""), &sel("q=.&raw=true")).unwrap(), "hi");
		assert_eq!(execute(r, &sel("q=.&compact=true")).unwrap(), "{\"a\":{\"b\":1}}");
	}

	#[test]
	fn execute_inputs() {
		// `[., inputs]` drains the rest of the stream on the first record
		let out = execute(&records("1\n2\n3"), &sel("q=[., inputs]")).unwrap();
		assert_eq!(out, "[\n  1,\n  2,\n  3\n]");
	}

	#[test]
	fn page_stream_lookahead() {
		let r = &records("1\n2\n3\n4\n5");
		let out = execute(r, &sel("q=.&offset=1&limit=2")).unwrap();
		assert_eq!(out, "2\n3\n[more results; append ?q=.&limit=2&offset=3 to continue]");
	}

	#[test]
	fn page_array_remaining() {
		let r = &records("[1,2,3,4,5]");
		let out = execute(r, &sel("q=.&limit=2")).unwrap();
		assert_eq!(
			out,
			"[\n  1,\n  2\n]\n[3 more items; append ?q=.&limit=2&offset=2 to continue]"
		);
	}

	#[test]
	fn page_past_end() {
		let out = execute(&records("1\n2"), &sel("q=.&offset=5&limit=2")).unwrap();
		assert_eq!(out, "[offset 5 is past the last result (2 total)]");
	}

	#[test]
	fn compile_error() {
		let err = execute(&records("1"), &sel("q=nonsense(")).unwrap_err();
		assert!(
			err.message.starts_with("Failed to execute JSON query: jq: error"),
			"{}",
			err.message
		);
	}

	#[test]
	fn runtime_error_leads() {
		let r = &records("{\"a\":1}\n{\"a\":2}\n{\"a\":3}");
		// the last input's failure fails the call with the stderr text
		let err = execute(r, &sel("q=error(%22boom%22)")).unwrap_err();
		assert_eq!(
			err.message,
			"Failed to execute JSON query: jq: error (at <stdin>:1): boom\njq: error (at <stdin>:2): boom\njq: error (at <stdin>:3): boom",
			"{}",
			err.message
		);
		// a mid-stream error is collected but the call succeeds
		let out = execute(r, &sel("q=if%20.a%20==%202%20then%20error(%22mid%22)%20else%20.a%20end&compact=true"))
			.unwrap();
		assert_eq!(out, "[jq stderr: jq: error (at <stdin>:2): mid]\n1\n3");
	}
}
