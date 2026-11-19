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

use std::path::Path;

use crate::error::Result;
use crate::json::Value;

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

/// Scans a whole JSONL file with `policy(line)` for each line, calling
/// `on_record` per parsed line. Adds the bytes read to [`crate::metrics::scanned`].
pub fn scan_file(
	file: &Path,
	policy: &mut dyn FnMut(u64) -> &'static Policy,
	on_record: &mut dyn FnMut(ScannedRecord),
) -> Result<ScanStats> {
	let _ = (file, policy, on_record);
	todo!("slice: scanner")
}

/// Scans only the given lines of a JSONL file (spans from an earlier scan, in
/// ascending order), each with `policy(line)`. A line yields the same record a
/// whole-file scan would. Adds the bytes read to [`crate::metrics::scanned`].
pub fn scan_lines(
	file: &Path,
	lines: &[LineSpan],
	policy: &mut dyn FnMut(u64) -> &'static Policy,
	on_record: &mut dyn FnMut(ScannedRecord),
) -> Result<()> {
	let _ = (file, lines, policy, on_record);
	todo!("slice: scanner")
}

/// Scans JSONL bytes held in memory (tests and in-memory sources).
pub fn scan_bytes(
	chunks: &mut dyn Iterator<Item = &[u8]>,
	policy: &mut dyn FnMut(u64) -> &'static Policy,
	on_record: &mut dyn FnMut(ScannedRecord),
) -> ScanStats {
	let _ = (chunks, policy, on_record);
	todo!("slice: scanner")
}
