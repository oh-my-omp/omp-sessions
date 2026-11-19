//! The value model every reader of a session file shares, and its JSON text.
//!
//! A [`Value`] is what the scanner projects out of one JSONL record: the
//! values it was told to build, with long strings cut to a captured prefix
//! ([`Clipped`]) that still carries the full length and line count. Objects
//! keep JavaScript's property order, and [`stringify`] writes JavaScript's
//! `JSON.stringify` text, so output rendered here reads exactly as the
//! TypeScript implementation it replaces wrote it. String lengths are counted
//! in UTF-16 code units, as JavaScript counts them.

use std::fmt::Write as _;

use serde::ser::{Serialize, SerializeMap, SerializeSeq, Serializer};

/// A string longer than its capture cap: the captured prefix plus the full measure.
#[derive(Clone, Debug, PartialEq)]
pub struct Clipped {
	/// The captured prefix.
	pub text: String,
	/// Length of the whole string in UTF-16 code units.
	pub chars: usize,
	/// Line count of the whole string (0 for an empty string).
	pub lines: usize,
}

impl Clipped {
	/// The plain-JSON form of a clipped string: its prefix and a clip marker.
	pub fn marked(&self) -> String {
		format!("{}…[clipped: {} chars total]", self.text, self.chars)
	}
}

/// One projected JSON value.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
	Null,
	Bool(bool),
	Num(f64),
	Str(String),
	Clipped(Clipped),
	Arr(Vec<Value>),
	Obj(Obj),
}

impl Value {
	pub fn as_obj(&self) -> Option<&Obj> {
		match self {
			Value::Obj(obj) => Some(obj),
			_ => None,
		}
	}

	pub fn as_arr(&self) -> Option<&[Value]> {
		match self {
			Value::Arr(items) => Some(items),
			_ => None,
		}
	}

	/// The string, or a clipped string's captured prefix.
	pub fn as_str(&self) -> Option<&str> {
		match self {
			Value::Str(text) => Some(text),
			Value::Clipped(clipped) => Some(&clipped.text),
			_ => None,
		}
	}
}

/// A JSON object with JavaScript's own-property order: array-index keys
/// (`"0"` … `"4294967294"`) first in ascending numeric order, then every other
/// key in insertion order. Setting an existing key replaces its value in place.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Obj {
	entries: Vec<(String, Value)>,
}

/// The numeric value of a canonical array-index key, as JavaScript recognizes one.
fn array_index(key: &str) -> Option<u32> {
	if key.is_empty() || key.len() > 10 || !key.bytes().all(|b| b.is_ascii_digit()) {
		return None;
	}
	if key.len() > 1 && key.starts_with('0') {
		return None;
	}
	let value: u64 = key.parse().ok()?;
	if value >= u64::from(u32::MAX) {
		return None;
	}
	u32::try_from(value).ok()
}

impl Obj {
	pub fn new() -> Self {
		Self { entries: Vec::new() }
	}

	pub fn insert(&mut self, key: impl Into<String>, value: Value) {
		let key = key.into();
		if let Some(slot) = self.entries.iter_mut().find(|(existing, _)| *existing == key) {
			slot.1 = value;
			return;
		}
		match array_index(&key) {
			Some(index) => {
				let at = self
					.entries
					.iter()
					.position(|(existing, _)| array_index(existing).is_none_or(|other| other > index))
					.unwrap_or(self.entries.len());
				self.entries.insert(at, (key, value));
			}
			None => self.entries.push((key, value)),
		}
	}

	pub fn get(&self, key: &str) -> Option<&Value> {
		self.entries.iter().find(|(existing, _)| existing == key).map(|(_, value)| value)
	}

	pub fn iter(&self) -> impl Iterator<Item = (&str, &Value)> {
		self.entries.iter().map(|(key, value)| (key.as_str(), value))
	}

	pub fn len(&self) -> usize {
		self.entries.len()
	}

	pub fn is_empty(&self) -> bool {
		self.entries.is_empty()
	}
}

/// Length in UTF-16 code units, JavaScript's `String.prototype.length`.
pub fn utf16_len(text: &str) -> usize {
	text.chars().map(char::len_utf16).sum()
}

/// The longest prefix of at most `units` UTF-16 code units that ends on a
/// character boundary (JavaScript's `slice(0, units)`, except that a cut
/// inside a surrogate pair drops the whole pair instead of keeping half).
pub fn utf16_prefix(text: &str, units: usize) -> &str {
	let mut count = 0;
	for (index, ch) in text.char_indices() {
		count += ch.len_utf16();
		if count > units {
			return &text[..index];
		}
	}
	text
}

/// `Number.prototype.toString` of a finite number; JSON's `null` for NaN and ±Infinity.
pub fn write_number(out: &mut String, value: f64) {
	if value.is_finite() {
		out.push_str(ryu_js::Buffer::new().format_finite(value));
	} else {
		out.push_str("null");
	}
}

/// A JSON string literal as `JSON.stringify` writes it.
pub fn write_string(out: &mut String, text: &str) {
	out.push('"');
	let mut start = 0;
	for (index, byte) in text.bytes().enumerate() {
		let escape = match byte {
			b'"' => "\\\"",
			b'\\' => "\\\\",
			b'\n' => "\\n",
			b'\r' => "\\r",
			b'\t' => "\\t",
			0x08 => "\\b",
			0x0c => "\\f",
			0x00..=0x1f => "",
			_ => continue,
		};
		out.push_str(&text[start..index]);
		if escape.is_empty() {
			let _ = write!(out, "\\u{byte:04x}");
		} else {
			out.push_str(escape);
		}
		start = index + 1;
	}
	out.push_str(&text[start..]);
	out.push('"');
}

fn write_value(out: &mut String, value: &Value, gap: Option<&str>, depth: usize) {
	match value {
		Value::Null => out.push_str("null"),
		Value::Bool(flag) => out.push_str(if *flag { "true" } else { "false" }),
		Value::Num(number) => write_number(out, *number),
		Value::Str(text) => write_string(out, text),
		Value::Clipped(clipped) => write_string(out, &clipped.marked()),
		Value::Arr(items) => {
			if items.is_empty() {
				out.push_str("[]");
				return;
			}
			out.push('[');
			for (index, item) in items.iter().enumerate() {
				if index > 0 {
					out.push(',');
				}
				newline(out, gap, depth + 1);
				write_value(out, item, gap, depth + 1);
			}
			newline(out, gap, depth);
			out.push(']');
		}
		Value::Obj(obj) => {
			if obj.is_empty() {
				out.push_str("{}");
				return;
			}
			out.push('{');
			for (index, (key, item)) in obj.iter().enumerate() {
				if index > 0 {
					out.push(',');
				}
				newline(out, gap, depth + 1);
				write_string(out, key);
				out.push(':');
				if gap.is_some() {
					out.push(' ');
				}
				write_value(out, item, gap, depth + 1);
			}
			newline(out, gap, depth);
			out.push('}');
		}
	}
}

fn newline(out: &mut String, gap: Option<&str>, depth: usize) {
	if let Some(gap) = gap {
		out.push('\n');
		for _ in 0..depth {
			out.push_str(gap);
		}
	}
}

/// `JSON.stringify(value)` (`gap` None) or `JSON.stringify(value, null, gap)`.
/// A clipped string is written as [`Clipped::marked`].
pub fn stringify(value: &Value, gap: Option<&str>) -> String {
	let mut out = String::new();
	write_value(&mut out, value, gap, 0);
	out
}

/// Wire form of a value (protocol rows): the same shape `stringify` writes.
impl Serialize for Value {
	fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
		match self {
			Value::Null => serializer.serialize_unit(),
			Value::Bool(flag) => serializer.serialize_bool(*flag),
			Value::Num(number) => serializer.serialize_f64(*number),
			Value::Str(text) => serializer.serialize_str(text),
			Value::Clipped(clipped) => serializer.serialize_str(&clipped.marked()),
			Value::Arr(items) => {
				let mut seq = serializer.serialize_seq(Some(items.len()))?;
				for item in items {
					seq.serialize_element(item)?;
				}
				seq.end()
			}
			Value::Obj(obj) => {
				let mut map = serializer.serialize_map(Some(obj.len()))?;
				for (key, item) in obj.iter() {
					map.serialize_entry(key, item)?;
				}
				map.end()
			}
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn obj(pairs: &[(&str, Value)]) -> Value {
		let mut out = Obj::new();
		for (key, value) in pairs {
			out.insert(*key, value.clone());
		}
		Value::Obj(out)
	}

	#[test]
	fn index_keys_come_first_in_numeric_order() {
		let value = obj(&[("b", Value::Num(1.0)), ("10", Value::Num(2.0)), ("2", Value::Num(3.0)), ("02", Value::Num(4.0))]);
		assert_eq!(stringify(&value, None), r#"{"2":3,"10":2,"b":1,"02":4}"#);
	}

	#[test]
	fn repeated_key_keeps_first_position_and_last_value() {
		let value = obj(&[("a", Value::Num(1.0)), ("b", Value::Num(2.0)), ("a", Value::Num(3.0))]);
		assert_eq!(stringify(&value, None), r#"{"a":3,"b":2}"#);
	}

	#[test]
	fn numbers_and_strings_match_javascript() {
		let value = Value::Arr(vec![
			Value::Num(1.0),
			Value::Num(0.1),
			Value::Num(1e21),
			Value::Num(-0.0),
			Value::Num(f64::NAN),
			Value::Str("a\"\\\n\u{1}\u{7f}é".into()),
		]);
		assert_eq!(stringify(&value, None), "[1,0.1,1e+21,0,null,\"a\\\"\\\\\\n\\u0001\u{7f}é\"]");
	}

	#[test]
	fn pretty_layout_matches_json_stringify_with_two_spaces() {
		let value = obj(&[("a", Value::Arr(vec![Value::Num(1.0), Value::Obj(Obj::new())])), ("b", Value::Arr(vec![]))]);
		assert_eq!(stringify(&value, Some("  ")), "{\n  \"a\": [\n    1,\n    {}\n  ],\n  \"b\": []\n}");
	}

	#[test]
	fn utf16_prefix_counts_surrogate_pairs_as_two() {
		assert_eq!(utf16_len("a😀b"), 4);
		assert_eq!(utf16_prefix("a😀b", 2), "a");
		assert_eq!(utf16_prefix("a😀b", 3), "a😀");
	}
}
