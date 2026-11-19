//! Streaming, projecting JSONL scanner for omp session files.
//!
//! A byte-level JSON tokenizer walks every record without materializing it:
//! a [`Policy`] tree decides which values are built, and strings are captured
//! up to a per-position cap while their full UTF-16 length and line count are
//! still measured. Skipped strings are crossed with `memchr`, so a record of
//! hundreds of MB costs one pass over its bytes and no allocation for it.
//! Validation is `JSON.parse`-equivalent for every built or measured value;
//! inside skipped strings only the record-ending newline is checked (other raw
//! control bytes there are tolerated). Each record reports the byte span of
//! its line, so a later pass can read only the lines it needs.

use std::io::Read as _;
use std::io::Seek as _;
use std::io::SeekFrom;
use std::path::Path;

use crate::error::EngineError;
use crate::error::Result;
use crate::json::Clipped;
use crate::json::Obj;
use crate::json::Value;
use crate::json::utf16_len;
use crate::json::utf16_prefix;
use crate::metrics;

/// What to build at one value position.
#[derive(Debug)]
pub enum Policy {
	Skip,
	Keep(Keep),
}

/// A kept position: strings directly here are captured up to `cap` UTF-16
/// units (0 = measured only, `usize::MAX` = whole), containers keep at most
/// `max_items` children, and `child` gives each child's policy.
#[derive(Debug)]
pub struct Keep {
	pub cap: usize,
	pub max_items: usize,
	pub child: Child,
}

/// The policy of a kept container's children.
#[derive(Debug)]
pub enum Child {
	/// A scalar position: any child of a container found here is skipped.
	None,
	/// Every child follows this same policy (`keepAll`).
	Same,
	/// Every child follows `0` (`each`).
	Each(&'static Policy),
	/// The listed object keys follow their policy; other keys and array elements are skipped (`keys`).
	Keys(&'static [(&'static str, &'static Policy)]),
	/// Every child follows this same policy except the listed keys, which are skipped.
	SameExcept(&'static [&'static str]),
}

/// A child position in a container: an object key or an array index.
#[derive(Clone, Copy, Debug)]
pub enum Slot<'a> {
	Key(&'a str),
	Index(usize),
}

pub static SKIP: Policy = Policy::Skip;

impl Policy {
	/// The policy of the child at `slot` of a container kept with `self`.
	pub fn child(&'static self, slot: Slot<'_>) -> &'static Policy {
		match self {
			Policy::Skip => &SKIP,
			Policy::Keep(keep) => match &keep.child {
				Child::None => &SKIP,
				Child::Same => self,
				Child::Each(element) => element,
				Child::Keys(table) => match slot {
					Slot::Key(key) => table.iter().find(|(name, _)| *name == key).map_or(&SKIP, |(_, policy)| policy),
					Slot::Index(_) => &SKIP,
				},
				Child::SameExcept(skipped) => match slot {
					Slot::Key(key) if skipped.contains(&key) => &SKIP,
					_ => self,
				},
			},
		}
	}
}

/// A scalar position: a string is captured up to `cap` UTF-16 units.
pub const fn scalar(cap: usize) -> Policy {
	Policy::Keep(Keep { cap, max_items: 0, child: Child::None })
}

/// Keeps the listed keys of an object.
pub const fn keys(table: &'static [(&'static str, &'static Policy)]) -> Policy {
	Policy::Keep(Keep { cap: 0, max_items: usize::MAX, child: Child::Keys(table) })
}

/// Keeps an array whose every element follows `element`, at most `max_items` of them.
pub const fn each(element: &'static Policy, max_items: usize) -> Policy {
	Policy::Keep(Keep { cap: 0, max_items, child: Child::Each(element) })
}

/// Keeps a whole subtree: strings capped at `cap`, containers at `max_items` children.
pub const fn keep_all(cap: usize, max_items: usize) -> Policy {
	Policy::Keep(Keep { cap, max_items, child: Child::Same })
}

/// One complete line's top-level value, projected, with the byte span of the line.
#[derive(Debug)]
pub struct ScannedRecord {
	/// Physical line number, 1-based.
	pub line: u64,
	/// Byte offset of the line's first byte.
	pub start: u64,
	/// Byte offset just past the line's last byte (its newline excluded).
	pub end: u64,
	pub value: Value,
}

/// The byte span of one line, as an earlier scan reported it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LineSpan {
	pub line: u64,
	pub start: u64,
	pub end: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ScanStats {
	pub records: u64,
	/// Physical line numbers of complete lines that failed to parse.
	pub invalid_lines: Vec<u64>,
	/// Line number of an unterminated, incomplete final record (a writer mid-append), if any.
	pub incomplete_tail: Option<u64>,
	pub bytes: u64,
}

const QUOTE: u8 = 0x22;
const BACKSLASH: u8 = 0x5c;
const NEWLINE: u8 = 0x0a;
const MAX_LITERAL: usize = 512;
/// Bytes `scan_file` and `scan_lines` read at a time.
const READ_BYTES: usize = 1024 * 1024;
/// Captures object keys of kept objects whole (`scalar(Infinity)`).
static KEY_POLICY: Policy = scalar(usize::MAX);
/// Measures a skipped object's keys: same raw-byte and escape validation as
/// capture, no value built (`scalar(0)`).
static SKIP_KEY_POLICY: Policy = scalar(0);

/// A parse failure inside a line: the line is counted invalid and the scanner
/// resynchronizes at its newline.
struct ParseError;

/// Tokenizer state: mirrors the TypeScript `Mode` union one variant per mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
	Value,
	ObjectKeyOrEnd,
	ObjectKey,
	Colon,
	ObjectCommaOrEnd,
	ArrayValueOrEnd,
	ArrayCommaOrEnd,
	Str,
	Literal,
	AfterTop,
	SkipLine,
}

/// A container being built (`Some`) or only walked for balance (`None`).
enum Target {
	Obj(Obj),
	Arr(Vec<Value>),
}

struct Frame {
	array: bool,
	policy: &'static Policy,
	/// `None` for a skipped container: its `key` and `count` are never read.
	target: Option<Target>,
	key: String,
	count: usize,
}

/**
 * Incremental tokenizer. Feed chunks with [`Scanner::push`], finish with
 * [`Scanner::end`]. Each complete line that parses yields one record (its
 * top-level value projected); a skipped top-level value yields none.
 */
struct Scanner<'a> {
	root: &'a mut dyn FnMut(u64) -> &'static Policy,
	emit: &'a mut dyn FnMut(ScannedRecord),
	mode: Mode,
	stack: Vec<Frame>,
	line: u64,
	/// Byte offset of the byte about to be consumed (or just consumed inside `byte`).
	pos: u64,
	/// Byte offset where the current physical line began.
	line_start: u64,
	record_started: bool,
	records: u64,
	bytes: u64,
	invalid: Vec<u64>,
	/// The top-level value once built; `None` = not built yet or skipped.
	top: Option<Value>,
	// Open string state.
	string_is_key: bool,
	string_cap: usize,
	string_capture: bool,
	string_measure: bool,
	string_chars: usize,
	string_newlines: usize,
	/// Captured text so far: decoded raw bytes and escapes in line order.
	string_out: String,
	/// UTF-16 units stored into `string_out`.
	string_stored: usize,
	/// A high surrogate written by a `\u` escape, awaiting a possible low-surrogate escape.
	pending_high: Option<u16>,
	/// Incomplete UTF-8 tail held between capture feeds (streaming TextDecoder state).
	utf8_tail: [u8; 4],
	utf8_tail_len: usize,
	/// 0 none, 1 after backslash, 2 collecting `\u` hex digits.
	escape: u8,
	hex: [u8; 4],
	hex_len: u8,
	literal_buf: [u8; MAX_LITERAL],
	literal_len: usize,
	literal_policy: &'static Policy,
}

impl<'a> Scanner<'a> {
	fn new(root: &'a mut dyn FnMut(u64) -> &'static Policy, emit: &'a mut dyn FnMut(ScannedRecord)) -> Self {
		Self {
			root,
			emit,
			mode: Mode::Value,
			stack: Vec::new(),
			line: 1,
			pos: 0,
			line_start: 0,
			record_started: false,
			records: 0,
			bytes: 0,
			invalid: Vec::new(),
			top: None,
			string_is_key: false,
			string_cap: 0,
			string_capture: false,
			string_measure: false,
			string_chars: 0,
			string_newlines: 0,
			string_out: String::new(),
			string_stored: 0,
			pending_high: None,
			utf8_tail: [0; 4],
			utf8_tail_len: 0,
			escape: 0,
			hex: [0; 4],
			hex_len: 0,
			literal_buf: [0; MAX_LITERAL],
			literal_len: 0,
			literal_policy: &SKIP,
		}
	}

	/// Starts a fresh line: line number and stream offset given by the caller
	/// (`scan_lines` positions each span's line directly).
	fn reset_at(&mut self, line: u64, offset: u64) {
		self.reset();
		self.line = line;
		self.line_start = offset;
		self.pos = offset;
	}

	fn push(&mut self, chunk: &[u8]) {
		self.bytes += chunk.len() as u64;
		let base = self.pos;
		let mut index = 0;
		while index < chunk.len() {
			if self.mode == Mode::SkipLine {
				let Some(at) = memchr::memchr(NEWLINE, &chunk[index..]) else {
					break;
				};
				index += at + 1;
				self.line += 1;
				self.line_start = base + index as u64;
				self.reset();
				continue;
			}
			if self.mode == Mode::Str {
				index = self.string_run(chunk, index);
				continue;
			}
			let byte = chunk[index];
			self.pos = base + index as u64;
			match self.byte(byte) {
				Ok(()) => index += 1,
				Err(ParseError) => {
					self.fail();
					// Leave a newline unconsumed so skipLine ends the line there.
					if byte != NEWLINE {
						index += 1;
					}
				}
			}
		}
		self.pos = base + chunk.len() as u64;
	}

	fn end(&mut self) -> ScanStats {
		let mut incomplete_tail = None;
		if self.mode == Mode::Literal && self.stack.is_empty() {
			// A top-level literal is complete at EOF if it parses; a failure is
			// not counted invalid — the line is an incomplete tail instead.
			let _ = self.end_literal();
		}
		if self.mode == Mode::AfterTop {
			self.finish_record();
		} else if self.mode != Mode::SkipLine && self.record_started {
			incomplete_tail = Some(self.line);
		}
		ScanStats {
			records: self.records,
			invalid_lines: std::mem::take(&mut self.invalid),
			incomplete_tail,
			bytes: self.bytes,
		}
	}

	fn reset(&mut self) {
		self.mode = Mode::Value;
		self.stack.clear();
		self.record_started = false;
		self.top = None;
		self.escape = 0;
		self.hex_len = 0;
		self.literal_len = 0;
		self.string_out.clear();
		self.string_stored = 0;
		self.pending_high = None;
		self.utf8_tail_len = 0;
	}

	/// Malformed line: count it and resynchronize at its newline.
	fn fail(&mut self) {
		self.invalid.push(self.line);
		self.mode = Mode::SkipLine;
	}

	fn finish_record(&mut self) {
		if let Some(value) = self.top.take() {
			self.records += 1;
			(self.emit)(ScannedRecord { line: self.line, start: self.line_start, end: self.pos, value });
		}
		self.reset();
	}

	fn current_policy(&mut self) -> &'static Policy {
		let Some(frame) = self.stack.last() else {
			return (self.root)(self.line);
		};
		let Policy::Keep(keep) = frame.policy else {
			return &SKIP;
		};
		if frame.count >= keep.max_items {
			return &SKIP;
		}
		let slot = if frame.array { Slot::Index(frame.count) } else { Slot::Key(&frame.key) };
		frame.policy.child(slot)
	}

	fn attach(&mut self, value: Option<Value>) {
		let Some(frame) = self.stack.last_mut() else {
			self.top = value;
			self.mode = Mode::AfterTop;
			return;
		};
		let next = if frame.array { Mode::ArrayCommaOrEnd } else { Mode::ObjectCommaOrEnd };
		if let Some(target) = &mut frame.target {
			if let Some(value) = value {
				match target {
					Target::Arr(items) => items.push(value),
					Target::Obj(obj) => obj.insert(std::mem::take(&mut frame.key), value),
				}
			}
			// A kept container's count covers every child, kept or skipped.
			frame.count += 1;
		}
		self.mode = next;
	}

	fn byte(&mut self, byte: u8) -> std::result::Result<(), ParseError> {
		let whitespace = byte == 0x20 || byte == 0x09 || byte == 0x0d;
		if byte == NEWLINE {
			if self.mode == Mode::Literal {
				self.end_literal()?;
			}
			if self.mode == Mode::AfterTop {
				self.finish_record();
			} else if self.record_started {
				return Err(ParseError);
			}
			self.line += 1;
			self.line_start = self.pos + 1;
			return Ok(());
		}
		match self.mode {
			Mode::Literal => {
				if whitespace || byte == b',' || byte == b'}' || byte == b']' {
					self.end_literal()?;
					return self.byte(byte);
				}
				if self.literal_len >= MAX_LITERAL {
					return Err(ParseError);
				}
				self.literal_buf[self.literal_len] = byte;
				self.literal_len += 1;
				Ok(())
			}
			Mode::AfterTop => {
				if whitespace {
					Ok(())
				} else {
					Err(ParseError)
				}
			}
			Mode::Value | Mode::ArrayValueOrEnd => {
				if whitespace {
					return Ok(());
				}
				if self.mode == Mode::ArrayValueOrEnd && byte == b']' {
					return self.close_container(true);
				}
				self.start_value(byte)
			}
			Mode::ObjectKeyOrEnd | Mode::ObjectKey => {
				if whitespace {
					return Ok(());
				}
				if self.mode == Mode::ObjectKeyOrEnd && byte == b'}' {
					return self.close_container(false);
				}
				if byte != QUOTE {
					return Err(ParseError);
				}
				let key_policy = match self.stack.last() {
					Some(Frame { policy: Policy::Skip, .. }) => &SKIP_KEY_POLICY,
					_ => &KEY_POLICY,
				};
				self.start_string(true, key_policy);
				Ok(())
			}
			Mode::Colon => {
				if whitespace {
					return Ok(());
				}
				if byte != b':' {
					return Err(ParseError);
				}
				self.mode = Mode::Value;
				Ok(())
			}
			Mode::ObjectCommaOrEnd => {
				if whitespace {
					return Ok(());
				}
				match byte {
					b',' => {
						self.mode = Mode::ObjectKey;
						Ok(())
					}
					b'}' => self.close_container(false),
					_ => Err(ParseError),
				}
			}
			Mode::ArrayCommaOrEnd => {
				if whitespace {
					return Ok(());
				}
				match byte {
					b',' => {
						self.mode = Mode::Value;
						Ok(())
					}
					b']' => self.close_container(true),
					_ => Err(ParseError),
				}
			}
			Mode::Str | Mode::SkipLine => unreachable!("scanner mode {:?} reached the structural byte path", self.mode),
		}
	}

	fn start_value(&mut self, byte: u8) -> std::result::Result<(), ParseError> {
		self.record_started = true;
		let policy = self.current_policy();
		if byte == b'{' || byte == b'[' {
			let array = byte == b'[';
			let target = match policy {
				Policy::Keep(_) => Some(if array { Target::Arr(Vec::new()) } else { Target::Obj(Obj::new()) }),
				Policy::Skip => None,
			};
			self.stack.push(Frame { array, policy, target, key: String::new(), count: 0 });
			self.mode = if array { Mode::ArrayValueOrEnd } else { Mode::ObjectKeyOrEnd };
			return Ok(());
		}
		if byte == QUOTE {
			self.start_string(false, policy);
			return Ok(());
		}
		if byte == b'-' || byte.is_ascii_digit() || byte == b't' || byte == b'f' || byte == b'n' {
			self.literal_buf[0] = byte;
			self.literal_len = 1;
			self.literal_policy = policy;
			self.mode = Mode::Literal;
			return Ok(());
		}
		Err(ParseError)
	}

	fn end_literal(&mut self) -> std::result::Result<(), ParseError> {
		let buf = &self.literal_buf[..self.literal_len];
		self.literal_len = 0;
		let keep = matches!(self.literal_policy, Policy::Keep(_));
		let value = if buf == b"true" {
			Value::Bool(true)
		} else if buf == b"false" {
			Value::Bool(false)
		} else if buf == b"null" {
			Value::Null
		} else if is_number_literal(buf) {
			// The grammar guarantees ASCII, so UTF-8 and f64 parsing cannot
			// fail; a failure would mean the grammar check itself is wrong.
			match std::str::from_utf8(buf).ok().and_then(|text| text.parse::<f64>().ok()) {
				Some(number) => Value::Num(number),
				None => return Err(ParseError),
			}
		} else {
			return Err(ParseError);
		};
		self.attach(keep.then_some(value));
		Ok(())
	}

	fn close_container(&mut self, array: bool) -> std::result::Result<(), ParseError> {
		match self.stack.pop() {
			Some(frame) if frame.array == array => {
				let value = frame.target.map(|target| match target {
					Target::Obj(obj) => Value::Obj(obj),
					Target::Arr(items) => Value::Arr(items),
				});
				self.attach(value);
				Ok(())
			}
			_ => Err(ParseError),
		}
	}

	fn start_string(&mut self, is_key: bool, policy: &'static Policy) {
		self.record_started = true;
		self.mode = Mode::Str;
		self.string_is_key = is_key;
		self.string_measure = matches!(policy, Policy::Keep(_));
		self.string_cap = match policy {
			Policy::Keep(keep) => keep.cap,
			Policy::Skip => 0,
		};
		self.string_capture = self.string_measure && self.string_cap > 0;
		self.string_chars = 0;
		self.string_newlines = 0;
		self.string_out.clear();
		self.string_stored = 0;
		self.pending_high = None;
		// A string a malformed line cut short may have left a partial UTF-8 tail; drop it.
		self.utf8_tail_len = 0;
		self.escape = 0;
		self.hex_len = 0;
	}

	/// Consumes string bytes from `index`; returns the next unconsumed index.
	fn string_run(&mut self, chunk: &[u8], index: usize) -> usize {
		if self.escape != 0 {
			let byte = chunk[index];
			if byte == NEWLINE {
				self.fail();
				return index;
			}
			self.escape_byte(byte);
			return index + 1;
		}
		let Some(at) = memchr::memchr3(QUOTE, BACKSLASH, NEWLINE, &chunk[index..]) else {
			if !self.raw_bytes(&chunk[index..]) {
				return chunk.len();
			}
			return chunk.len();
		};
		let stop = index + at;
		match chunk[stop] {
			NEWLINE => {
				self.fail();
				stop
			}
			QUOTE => {
				if stop > index && !self.raw_bytes(&chunk[index..stop]) {
					return stop;
				}
				if self.end_string().is_err() {
					self.fail();
				}
				stop + 1
			}
			_ => {
				if stop > index && !self.raw_bytes(&chunk[index..stop]) {
					return stop;
				}
				self.escape = 1;
				stop + 1
			}
		}
	}

	/// Measures (and captures) raw UTF-8 bytes; false when a control byte makes
	/// the line invalid.
	fn raw_bytes(&mut self, bytes: &[u8]) -> bool {
		if !self.string_measure {
			return true;
		}
		let mut length = 0usize;
		for &byte in bytes {
			if byte < 0x20 {
				self.fail();
				return false;
			}
			// UTF-16 length: ASCII and 2/3-byte leads count 1, 4-byte leads 2,
			// continuations 0.
			if byte < 0x80 || (0xc0..0xf0).contains(&byte) {
				length += 1;
			} else if byte >= 0xf0 {
				length += 2;
			}
		}
		self.string_chars += length;
		if self.string_capture && self.string_stored < self.string_cap {
			self.capture_feed(bytes);
		}
		true
	}

	/// Feeds raw UTF-8 bytes to the incremental lossy decoder held in
	/// `utf8_tail`/`string_out`, as `TextDecoder` with `stream: true` does.
	fn capture_feed(&mut self, mut bytes: &[u8]) {
		while self.utf8_tail_len > 0 && !bytes.is_empty() {
			let take = bytes.len().min(4);
			let mut tmp = [0u8; 8];
			tmp[..self.utf8_tail_len].copy_from_slice(&self.utf8_tail[..self.utf8_tail_len]);
			tmp[self.utf8_tail_len..self.utf8_tail_len + take].copy_from_slice(&bytes[..take]);
			let total = self.utf8_tail_len + take;
			self.utf8_tail_len = 0;
			self.decode_capture(&tmp[..total]);
			bytes = &bytes[take..];
		}
		self.decode_capture(bytes);
	}

	/// Decodes `bytes` as a `TextDecoder` stream piece: valid UTF-8 is stored,
	/// each maximal invalid subpart stores one U+FFFD, and an incomplete tail
	/// is held in `utf8_tail` for the next piece (or flushed at string end).
	fn decode_capture(&mut self, mut bytes: &[u8]) {
		while !bytes.is_empty() {
			match std::str::from_utf8(bytes) {
				Ok(text) => {
					self.store_str(text);
					return;
				}
				Err(error) => {
					let valid = error.valid_up_to();
					if valid > 0
						&& let Ok(text) = std::str::from_utf8(&bytes[..valid])
					{
						self.store_str(text);
					}
					match error.error_len() {
						Some(bad) => {
							self.store_char('\u{FFFD}');
							bytes = &bytes[valid + bad..];
						}
						None => {
							let tail = &bytes[valid..];
							self.utf8_tail[..tail.len()].copy_from_slice(tail);
							self.utf8_tail_len = tail.len();
							return;
						}
					}
				}
			}
		}
	}

	/// Stores decoded text, flushing a pending high surrogate first: raw bytes
	/// can never complete an escape-written surrogate pair.
	fn store_str(&mut self, text: &str) {
		if text.is_empty() || self.string_stored >= self.string_cap {
			return;
		}
		if self.pending_high.take().is_some() {
			self.string_out.push('\u{FFFD}');
		}
		self.string_stored += utf16_len(text);
		self.string_out.push_str(text);
	}

	/// Stores one decoded char, after flushing a pending high surrogate.
	fn store_char(&mut self, c: char) {
		if self.string_stored >= self.string_cap {
			return;
		}
		if self.pending_high.take().is_some() {
			self.string_out.push('\u{FFFD}');
		}
		self.string_stored += c.len_utf16();
		self.string_out.push(c);
	}

	/// A simple escape's char (`\"` `\\` `\/` `\b` `\f` `\n` `\r` `\t`).
	fn append_char(&mut self, c: char) {
		if !self.string_measure {
			return;
		}
		self.string_chars += 1;
		if c == '\n' {
			self.string_newlines += 1;
		}
		if !self.string_capture || self.string_stored >= self.string_cap {
			return;
		}
		self.string_stored += 1;
		if self.pending_high.take().is_some() {
			self.string_out.push('\u{FFFD}');
		}
		self.string_out.push(c);
	}

	/// A `\u` escape's UTF-16 unit: a high+low escape pair joins into one char;
	/// a lone surrogate stores U+FFFD (and counts one unit either way).
	fn append_unit(&mut self, unit: u16) {
		if !self.string_measure {
			return;
		}
		self.string_chars += 1;
		if unit == 0x000a {
			self.string_newlines += 1;
		}
		if !self.string_capture || self.string_stored >= self.string_cap {
			return;
		}
		self.string_stored += 1;
		if let Some(high) = self.pending_high.take() {
			if (0xdc00..=0xdfff).contains(&unit) {
				let code = 0x1_0000u32 + (((high as u32) - 0xd800) << 10) + (u32::from(unit) - 0xdc00);
				self.string_out.push(char::from_u32(code).unwrap_or('\u{FFFD}'));
				return;
			}
			// A lone high surrogate: the JS string keeps it, Rust stores U+FFFD.
			self.string_out.push('\u{FFFD}');
		}
		if (0xd800..=0xdbff).contains(&unit) {
			self.pending_high = Some(unit);
		} else if (0xdc00..=0xdfff).contains(&unit) {
			self.string_out.push('\u{FFFD}');
		} else {
			self.string_out.push(char::from_u32(u32::from(unit)).unwrap_or('\u{FFFD}'));
		}
	}

	fn escape_byte(&mut self, byte: u8) {
		if self.escape == 2 {
			self.hex[self.hex_len as usize] = byte;
			self.hex_len += 1;
			if self.hex_len < 4 {
				return;
			}
			let mut unit = 0u16;
			for &h in &self.hex {
				let digit = match h {
					b'0'..=b'9' => h - b'0',
					b'a'..=b'f' => h - b'a' + 10,
					b'A'..=b'F' => h - b'A' + 10,
					_ => {
						self.fail();
						return;
					}
				};
				unit = (unit << 4) | u16::from(digit);
			}
			self.append_unit(unit);
			self.escape = 0;
			return;
		}
		if byte == b'u' {
			self.escape = 2;
			self.hex_len = 0;
			return;
		}
		let decoded = match byte {
			QUOTE => '"',
			BACKSLASH => '\\',
			b'/' => '/',
			b'b' => '\u{8}',
			b'f' => '\u{c}',
			b'n' => '\n',
			b'r' => '\r',
			b't' => '\t',
			_ => {
				self.fail();
				return;
			}
		};
		self.append_char(decoded);
		self.escape = 0;
	}

	fn end_string(&mut self) -> std::result::Result<(), ParseError> {
		// Decoder flush: a held incomplete tail yields one U+FFFD, after any
		// pending surrogate, exactly as `TextDecoder.decode()` at stream end.
		if self.utf8_tail_len > 0 {
			self.utf8_tail_len = 0;
			self.store_char('\u{FFFD}');
		}
		if self.pending_high.take().is_some() && self.string_stored < self.string_cap {
			self.string_out.push('\u{FFFD}');
		}
		let text = std::mem::take(&mut self.string_out);
		self.string_stored = 0;
		if self.string_is_key {
			match self.stack.last_mut() {
				Some(frame) if !frame.array => {
					if frame.target.is_some() {
						frame.key = text;
					}
					self.mode = Mode::Colon;
					Ok(())
				}
				_ => Err(ParseError),
			}
		} else {
			if !self.string_measure {
				self.attach(None);
				return Ok(());
			}
			let chars = self.string_chars;
			let value = if chars <= self.string_cap {
				Value::Str(text)
			} else {
				Value::Clipped(Clipped {
					text: utf16_prefix(&text, self.string_cap).to_string(),
					chars,
					lines: if chars == 0 { 0 } else { self.string_newlines + 1 },
				})
			};
			self.attach(Some(value));
			Ok(())
		}
	}
}

/// `-?(0|[1-9]\d*)(\.\d+)?([eE][+-]?\d+)?` over literal bytes; literals are
/// ASCII, digits match only ASCII.
fn is_number_literal(buf: &[u8]) -> bool {
	let end = buf.len();
	let mut index = usize::from(buf.first() == Some(&b'-'));
	if index >= end {
		return false;
	}
	if buf[index] == b'0' {
		index += 1;
	} else if buf[index].is_ascii_digit() && buf[index] != b'0' {
		while index < end && buf[index].is_ascii_digit() {
			index += 1;
		}
	} else {
		return false;
	}
	if index < end && buf[index] == b'.' {
		let mut next = index + 1;
		while next < end && buf[next].is_ascii_digit() {
			next += 1;
		}
		if next == index + 1 {
			return false;
		}
		index = next;
	}
	if index < end && (buf[index] == b'e' || buf[index] == b'E') {
		index += 1;
		if index < end && (buf[index] == b'+' || buf[index] == b'-') {
			index += 1;
		}
		let mut next = index;
		while next < end && buf[next].is_ascii_digit() {
			next += 1;
		}
		if next == index {
			return false;
		}
		index = next;
	}
	index == end
}

/// Scans a whole JSONL file with `policy(line)` for each line, calling
/// `on_record` per parsed line. Counts the file in [`crate::metrics::transcript`]
/// and the bytes read in [`crate::metrics::scanned`].
pub fn scan_file(
	file: &Path,
	policy: &mut dyn FnMut(u64) -> &'static Policy,
	on_record: &mut dyn FnMut(ScannedRecord),
) -> Result<ScanStats> {
	metrics::transcript();
	let mut handle = std::fs::File::open(file).map_err(|error| EngineError::io(file, &error))?;
	let mut buffer = vec![0u8; READ_BYTES];
	let mut scanner = Scanner::new(policy, on_record);
	loop {
		let read = handle.read(&mut buffer).map_err(|error| EngineError::io(file, &error))?;
		if read == 0 {
			break;
		}
		metrics::scanned(read as u64);
		scanner.push(&buffer[..read]);
	}
	Ok(scanner.end())
}

/// Scans only the given lines of a JSONL file (spans from an earlier scan, in
/// ascending order), each with `policy(line)`. A line yields the same record a
/// whole-file scan would. The metrics already hold these bytes from that scan.
pub fn scan_lines(
	file: &Path,
	lines: &[LineSpan],
	policy: &mut dyn FnMut(u64) -> &'static Policy,
	on_record: &mut dyn FnMut(ScannedRecord),
) -> Result<()> {
	let mut handle = std::fs::File::open(file).map_err(|error| EngineError::io(file, &error))?;
	let mut buffer = vec![0u8; READ_BYTES];
	let mut scanner = Scanner::new(policy, on_record);
	for span in lines {
		scanner.reset_at(span.line, span.start);
		handle.seek(SeekFrom::Start(span.start)).map_err(|error| EngineError::io(file, &error))?;
		let mut left = span.end.saturating_sub(span.start);
		while left > 0 {
			let take = left.min(READ_BYTES as u64) as usize;
			handle.read_exact(&mut buffer[..take]).map_err(|error| EngineError::io(file, &error))?;
			scanner.push(&buffer[..take]);
			left -= take as u64;
		}
		// Finish the line as if its newline followed.
		scanner.push(b"\n");
	}
	Ok(())
}

/// Scans JSONL bytes held in memory (tests and in-memory sources).
pub fn scan_bytes(
	chunks: &mut dyn Iterator<Item = &[u8]>,
	policy: &mut dyn FnMut(u64) -> &'static Policy,
	on_record: &mut dyn FnMut(ScannedRecord),
) -> ScanStats {
	let mut scanner = Scanner::new(policy, on_record);
	for chunk in chunks {
		scanner.push(chunk);
	}
	scanner.end()
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::io::Write as _;
	use std::sync::atomic::AtomicU64;
	use std::sync::atomic::Ordering;

	static EVERYTHING: Policy = keep_all(usize::MAX, usize::MAX);

	struct Out {
		records: Vec<ScannedRecord>,
	}

	impl Out {
		fn new() -> Self {
			Self { records: Vec::new() }
		}
	}

	/// Scans `chunks` with the root policy `policy` for every line.
	fn scan_all(chunks: &[&[u8]], policy: &'static Policy) -> (ScanStats, Vec<ScannedRecord>) {
		let mut out = Out::new();
		let mut policy_fn = |_: u64| policy;
		let stats = {
			let mut emit = |record: ScannedRecord| out.records.push(record);
			let mut iter = chunks.iter().copied();
			scan_bytes(&mut iter, &mut policy_fn, &mut emit)
		};
		(stats, out.records)
	}

	fn value_of(records: &[ScannedRecord], index: usize) -> &Value {
		&records[index].value
	}

	#[test]
	fn records_and_spans() {
		let (stats, records) = scan_all(&[b"\"a\"\n{\"b\":2}\n123 \n"], &EVERYTHING);
		assert_eq!(stats.records, 3);
		assert_eq!(records[0].line, 1);
		assert_eq!((records[0].start, records[0].end), (0, 3));
		assert_eq!(records[1].line, 2);
		assert_eq!((records[1].start, records[1].end), (4, 11));
		assert_eq!(*value_of(&records, 2), Value::Num(123.0));
		// Trailing whitespace before the newline is inside the line's span.
		assert_eq!((records[2].start, records[2].end), (12, 16));
		assert!(stats.invalid_lines.is_empty());
		assert_eq!(stats.incomplete_tail, None);
	}

	#[test]
	fn chunk_boundaries_inside_char_and_escape() {
		let line = "{\"s\":\"aé😀\\n\\u0042\\ud83d\\ude00\"}\n".as_bytes().to_vec();
		let (whole_stats, whole) = scan_all(&[&line], &EVERYTHING);
		for split in 1..line.len() {
			let (stats, records) = scan_all(&[&line[..split], &line[split..]], &EVERYTHING);
			assert_eq!(stats, whole_stats, "split at {split}");
			assert_eq!(records.len(), 1);
			assert_eq!(records[0].value, whole[0].value, "split at {split}");
			assert_eq!((records[0].start, records[0].end), (whole[0].start, whole[0].end));
		}
		// The string itself: a + é + 😀 + \n + B + 😀 = 1+1+2+1+1+2 = 8 units.
		let Value::Obj(obj) = value_of(&whole, 0) else {
			panic!("expected object");
		};
		assert_eq!(obj.get("s"), Some(&Value::Str("aé😀\nB😀".to_string())));
	}

	#[test]
	fn malformed_lines_resync() {
		let data = b"{\"a\": bad}\n{\"ok\":1}\ntrailing garbage {\"x\":1}\n{\"z\":2}\n";
		let (stats, records) = scan_all(&[data], &EVERYTHING);
		assert_eq!(stats.invalid_lines, vec![1, 3]);
		assert_eq!(stats.records, 2);
		assert_eq!(records[0].line, 2);
		assert_eq!(records[1].line, 4);
		assert!(stats.incomplete_tail.is_none());
	}

	#[test]
	fn newline_inside_string_is_invalid() {
		let (stats, records) = scan_all(&[b"\"ab\ncd\"\n{\"k\":1}\n"], &EVERYTHING);
		assert_eq!(stats.invalid_lines, vec![1, 2]);
		assert_eq!(stats.records, 1);
		assert_eq!(records[0].line, 3);
		// Same for an unescaped control byte in a measured string.
		let (stats2, _) = scan_all(&[b"\"a\x01b\"\n{\"k\":1}\n"], &EVERYTHING);
		assert_eq!(stats2.invalid_lines, vec![1]);
	}

	#[test]
	fn incomplete_tail_variants() {
		// Unterminated object tail.
		let (stats, records) = scan_all(&[b"{\"a\":1}\n{\"b\":"], &EVERYTHING);
		assert_eq!(records.len(), 1);
		assert_eq!(stats.incomplete_tail, Some(2));
		// A complete top-level literal at EOF still yields its record.
		let (stats, records) = scan_all(&[b"{\"a\":1}\ntrue"], &EVERYTHING);
		assert_eq!(records.len(), 2);
		assert_eq!(*value_of(&records, 1), Value::Bool(true));
		assert_eq!(stats.incomplete_tail, None);
		// A number with trailing space: the record ends before the newline.
		let (stats, records) = scan_all(&[b"1e3 "], &EVERYTHING);
		assert_eq!(records.len(), 1);
		assert_eq!(*value_of(&records, 0), Value::Num(1000.0));
		assert_eq!(stats.incomplete_tail, None);
		// An unterminated literal is an incomplete tail, not an invalid line.
		let (stats, records) = scan_all(&[b"tru"], &EVERYTHING);
		assert!(records.is_empty());
		assert_eq!(stats.incomplete_tail, Some(1));
		assert!(stats.invalid_lines.is_empty());
		// Whitespace only: nothing.
		let (stats, records) = scan_all(&[b"  \n\n"], &EVERYTHING);
		assert!(records.is_empty());
		assert_eq!(stats.incomplete_tail, None);
		assert!(stats.invalid_lines.is_empty());
	}

	#[test]
	fn clip_measures() {
		static CAP3: Policy = scalar(3);
		// a(1) b(1) é escape(1) 😀 raw(2) d(1) = 6 units; cap 3.
		let (_, records) = scan_all(&["\"ab\\u00e9😀d\"\n".as_bytes()], &CAP3);
		assert_eq!(
			*value_of(&records, 0),
			Value::Clipped(Clipped { text: "abé".to_string(), chars: 6, lines: 1 })
		);
		// Escaped newlines count toward lines only when clipped.
		static CAP1: Policy = scalar(1);
		let (_, records) = scan_all(&[b"\"a\\nb\"\n"], &CAP1);
		assert_eq!(
			*value_of(&records, 0),
			Value::Clipped(Clipped { text: "a".to_string(), chars: 3, lines: 2 })
		);
		// At or under the cap the plain string is kept.
		let (_, records) = scan_all(&[b"\"ab\"\n"], &CAP3);
		assert_eq!(*value_of(&records, 0), Value::Str("ab".to_string()));
		// An empty string reports lines = 0; measure-only (cap 0) still clips >0.
		static CAP0: Policy = scalar(0);
		let (_, records) = scan_all(&[b"\"x\"\n"], &CAP0);
		assert_eq!(
			*value_of(&records, 0),
			Value::Clipped(Clipped { text: String::new(), chars: 1, lines: 1 })
		);
	}

	#[test]
	fn surrogate_escapes() {
		// A high+low escape pair joins into one char (2 UTF-16 units).
		let (_, records) = scan_all(&[b"\"\\ud83d\\ude00\"\n"], &EVERYTHING);
		assert_eq!(*value_of(&records, 0), Value::Str("😀".to_string()));
		// A lone surrogate stores U+FFFD and counts one unit.
		let (_, records) = scan_all(&[b"\"\\ud800\"\n"], &EVERYTHING);
		assert_eq!(*value_of(&records, 0), Value::Str("\u{FFFD}".to_string()));
		// High surrogate followed by a BMP escape: two separate units.
		let (_, records) = scan_all(&[b"\"\\ud800\\u0041\"\n"], &EVERYTHING);
		assert_eq!(*value_of(&records, 0), Value::Str("\u{FFFD}A".to_string()));
		// A pending surrogate resolves before raw bytes too.
		let (_, records) = scan_all(&["\"\\ud800x\"\n".as_bytes()], &EVERYTHING);
		assert_eq!(*value_of(&records, 0), Value::Str("\u{FFFD}x".to_string()));
	}

	#[test]
	fn invalid_escapes_and_utf8() {
		// Bad escape and bad hex digit.
		let (stats, _) = scan_all(&[b"\"a\\x\"\n{\"k\":1}\n"], &EVERYTHING);
		assert_eq!(stats.invalid_lines, vec![1]);
		let (stats, _) = scan_all(&[b"\"a\\u12zz\"\n{\"k\":1}\n"], &EVERYTHING);
		assert_eq!(stats.invalid_lines, vec![1]);
		// Invalid UTF-8 inside a captured string decodes lossily.
		let (_, records) = scan_all(&[b"\"a\xffb\"\n"], &EVERYTHING);
		assert_eq!(*value_of(&records, 0), Value::Str("a\u{FFFD}b".to_string()));
		// A truncated multi-byte tail at the closing quote becomes one U+FFFD.
		let (_, records) = scan_all(&[b"\"a\xf0\x9f\"\n"], &EVERYTHING);
		assert_eq!(*value_of(&records, 0), Value::Str("a\u{FFFD}".to_string()));
	}

	#[test]
	fn skipped_string_tolerates_control_bytes() {
		static S_TABLE: &[(&str, &Policy)] = &[("t", &scalar(5))];
		static OBJ: Policy = keys(S_TABLE);
		// A raw control byte inside the skipped string is tolerated.
		let (stats, records) = scan_all(&[b"{\"s\":\"a\x01b\",\"t\":\"kept\"}\n"], &OBJ);
		assert!(stats.invalid_lines.is_empty());
		let Value::Obj(obj) = value_of(&records, 0) else {
			panic!("expected object");
		};
		assert_eq!(obj.get("t"), Some(&Value::Str("kept".to_string())));
		assert_eq!(obj.len(), 1);
		// But escapes in skipped strings are still validated.
		let (stats, _) = scan_all(&[b"{\"s\":\"a\\x\",\"t\":1}\n"], &OBJ);
		assert_eq!(stats.invalid_lines, vec![1]);
	}

	#[test]
	fn max_items_and_skipped_children() {
		static PAIR: Policy = keep_all(usize::MAX, 2);
		let (_, records) = scan_all(&[b"[1,2,3,4]\n"], &PAIR);
		assert_eq!(*value_of(&records, 0), Value::Arr(vec![Value::Num(1.0), Value::Num(2.0)]));
		let (_, records) = scan_all(&[b"{\"a\":1,\"b\":[5],\"c\":2}\n"], &PAIR);
		let Value::Obj(obj) = value_of(&records, 0) else {
			panic!("expected object");
		};
		assert_eq!(obj.len(), 2);
		assert_eq!(obj.get("a"), Some(&Value::Num(1.0)));
		assert_eq!(obj.get("b"), Some(&Value::Arr(vec![Value::Num(5.0)])));
	}

	#[test]
	fn duplicate_keys_last_wins_in_place() {
		let (_, records) = scan_all(&[b"{\"a\":1,\"b\":2,\"a\":3}\n"], &EVERYTHING);
		let Value::Obj(obj) = value_of(&records, 0) else {
			panic!("expected object");
		};
		let keys: Vec<&str> = obj.iter().map(|(key, _)| key).collect();
		assert_eq!(keys, ["a", "b"]);
		assert_eq!(obj.get("a"), Some(&Value::Num(3.0)));
	}

	#[test]
	fn literal_cap_and_grammar() {
		// 513-byte literal exceeds MAX_LITERAL: the line fails when byte 513 arrives.
		let mut line = b"1".repeat(513);
		line.push(b'\n');
		let (stats, _) = scan_all(&[&line], &EVERYTHING);
		assert_eq!(stats.invalid_lines, vec![1]);
		// Grammar edge cases that JSON.parse rejects.
		for bad in [b"01\n".as_slice(), b"1.\n".as_slice(), b"-\n".as_slice(), b"1e\n".as_slice(), b"nul\n".as_slice()] {
			let (stats, _) = scan_all(&[bad], &EVERYTHING);
			assert_eq!(stats.invalid_lines, vec![1], "{bad:?}");
		}
		for good in [b"0\n".as_slice(), b"-0\n".as_slice(), b"1.5e+3\n".as_slice(), b"1E-3\n".as_slice()] {
			let (stats, records) = scan_all(&[good], &EVERYTHING);
			assert_eq!(stats.records, 1, "{good:?}");
			assert!(matches!(records[0].value, Value::Num(_)));
		}
		// -0 keeps its sign.
		let (_, records) = scan_all(&[b"-0\n"], &EVERYTHING);
		assert_eq!(*value_of(&records, 0), Value::Num(-0.0));
	}

	#[test]
	fn skipped_top_level_yields_no_record() {
		let (stats, records) = scan_all(&[b"123\n{\"a\":1}\n"], &SKIP);
		assert!(records.is_empty());
		assert_eq!(stats.records, 0);
		assert!(stats.invalid_lines.is_empty());
	}

	static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

	fn temp_file(contents: &[u8]) -> std::path::PathBuf {
		let path = std::env::temp_dir().join(format!(
			"omp-scan-test-{}-{}.jsonl",
			std::process::id(),
			TEMP_SEQ.fetch_add(1, Ordering::Relaxed)
		));
		let mut handle = std::fs::File::create(&path).expect("create temp file");
		handle.write_all(contents).expect("write temp file");
		path
	}

	#[test]
	fn scan_file_and_scan_lines_agree() {
		let mut contents = Vec::new();
		contents.extend_from_slice(b"{\"a\":1,\"s\":\"first\"}\n");
		let bad_start = contents.len() as u64;
		contents.extend_from_slice(b"{bad line}\n");
		let bad_end = contents.len() as u64 - 1;
		contents.extend_from_slice(b"[1,2,{\"x\":\"y\"}]\n");
		contents.extend_from_slice("\"str😀tail\"\n".as_bytes());
		contents.extend_from_slice(b"true");
		let path = temp_file(&contents);

		let mut records = Vec::new();
		let stats = {
			let mut policy = |_: u64| &EVERYTHING;
			let mut emit = |record: ScannedRecord| records.push(record);
			scan_file(&path, &mut policy, &mut emit).expect("scan_file")
		};
		assert_eq!(stats.records, 4);
		assert_eq!(stats.invalid_lines, vec![2]);
		assert_eq!(stats.incomplete_tail, None);
		// Spans cover each line's bytes excluding the newline.
		for record in &records {
			let span = &contents[record.start as usize..record.end as usize];
			assert!(!span.contains(&b'\n'));
			if record.end < contents.len() as u64 {
				assert_eq!(contents[record.end as usize], b'\n');
			}
		}
		// scan_lines over the same spans yields the same records; a malformed
		// line's span yields nothing.
		let mut spans: Vec<LineSpan> = records
			.iter()
			.map(|record| LineSpan { line: record.line, start: record.start, end: record.end })
			.collect();
		spans.insert(1, LineSpan { line: 2, start: bad_start, end: bad_end });
		let mut again = Vec::new();
		{
			let mut policy = |_: u64| &EVERYTHING;
			let mut emit = |record: ScannedRecord| again.push(record);
			scan_lines(&path, &spans, &mut policy, &mut emit).expect("scan_lines");
		}
		assert_eq!(again.len(), records.len());
		for (a, b) in records.iter().zip(again.iter()) {
			assert_eq!((a.line, a.start, a.end), (b.line, b.start, b.end));
			assert_eq!(a.value, b.value);
		}
		// A subset in ascending order works too.
		let mut third = Vec::new();
		{
			let mut policy = |_: u64| &EVERYTHING;
			let mut emit = |record: ScannedRecord| third.push(record);
			scan_lines(&path, &[spans[0], spans[3]], &mut policy, &mut emit).expect("scan_lines subset");
		}
		assert_eq!(third.len(), 2);
		assert_eq!(third[0].line, 1);
		assert_eq!(third[1].line, 4);
		std::fs::remove_file(&path).expect("remove temp file");
	}
}
