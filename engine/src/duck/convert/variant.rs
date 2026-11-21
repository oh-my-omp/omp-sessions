//! VARIANT values, decoded as `@duckdb/node-api`'s `DuckDBVariantVector`
//! decodes them and converted as its `fromVariantValue` converts them.
//!
//! A VARIANT vector is physically a STRUCT of `keys` (LIST(VARCHAR)),
//! `children` (LIST(STRUCT(keys_index, values_index))), `values`
//! (LIST(STRUCT(type_id, byte_offset))) and `data` (BLOB). A row's root node is
//! entry 0 of its `values`; a scalar's payload sits at its byte offset in the
//! row's blob; an OBJECT or ARRAY names a slice of the row's `children`.
//!
//! node-api converts a scalar by its recorded type only where it keeps one: the
//! root, and the items of an ARRAY whose items differ in type. Anywhere else (an
//! OBJECT's fields, the items of a uniform ARRAY) it converts the decoded
//! JavaScript value by the type `typeForValue` infers for it. The two differ for
//! numbers: an inferred integer outside the 32-bit range is BIGINT, written as
//! text, and an inferred GEOMETRY has no converter.

use libduckdb_sys as ffi;

use super::{DuckError, Result, bit_text, blob_text, hugeint, json_number, string_bytes, temporal_text, uhugeint, uuid_text, value_text};
use crate::json::{Obj, Value};

/// `VariantLogicalType`: the tag of one VARIANT node.
mod tag {
	pub const NULL: u8 = 0;
	pub const BOOL_TRUE: u8 = 1;
	pub const BOOL_FALSE: u8 = 2;
	pub const INT8: u8 = 3;
	pub const INT16: u8 = 4;
	pub const INT32: u8 = 5;
	pub const INT64: u8 = 6;
	pub const INT128: u8 = 7;
	pub const UINT8: u8 = 8;
	pub const UINT16: u8 = 9;
	pub const UINT32: u8 = 10;
	pub const UINT64: u8 = 11;
	pub const UINT128: u8 = 12;
	pub const FLOAT: u8 = 13;
	pub const DOUBLE: u8 = 14;
	pub const DECIMAL: u8 = 15;
	pub const VARCHAR: u8 = 16;
	pub const BLOB: u8 = 17;
	pub const UUID: u8 = 18;
	pub const DATE: u8 = 19;
	pub const TIME_MICROS: u8 = 20;
	pub const TIME_NANOS: u8 = 21;
	pub const TIMESTAMP_SEC: u8 = 22;
	pub const TIMESTAMP_MILIS: u8 = 23;
	pub const TIMESTAMP_MICROS: u8 = 24;
	pub const TIMESTAMP_NANOS: u8 = 25;
	pub const TIME_MICROS_TZ: u8 = 26;
	pub const TIMESTAMP_MICROS_TZ: u8 = 27;
	pub const INTERVAL: u8 = 28;
	pub const OBJECT: u8 = 29;
	pub const ARRAY: u8 = 30;
	pub const BIGNUM: u8 = 31;
	pub const BITSTRING: u8 = 32;
	pub const GEOMETRY: u8 = 33;
	pub const TIMESTAMP_NANOS_TZ: u8 = 34;
}

/// A scalar as node-api holds it after decoding.
enum Leaf {
	Bool(bool),
	/// A JavaScript number read from an integer of at most 32 bits.
	Integer(f64),
	/// A JavaScript number read from a FLOAT or DOUBLE.
	Float(f64),
	/// A value written as the same text under its recorded and its inferred
	/// type: wide integers, BIGNUM, DECIMAL, VARCHAR, BLOB, BIT, UUID, dates and times.
	Text(String),
	Interval(Value),
	/// WKB as BLOB text; only its recorded type has a converter.
	Geometry(String),
}

impl Leaf {
	/// node-api's JSON for this scalar, by its recorded type (`typed`) or by `typeForValue`.
	fn convert(self, typed: bool) -> Result<Value> {
		Ok(match self {
			Leaf::Bool(flag) => Value::Bool(flag),
			Leaf::Integer(number) => {
				if typed || in_int32(number) {
					Value::Num(number)
				} else {
					Value::Str(integral_text(number))
				}
			}
			Leaf::Float(number) => {
				if !typed && number.is_finite() && number.fract() == 0.0 {
					// typeForValue: INTEGER within 32 bits, else BIGINT (`BigInt(value)` as text).
					if in_int32(number) { Value::Num(number) } else { Value::Str(integral_text(number)) }
				} else {
					json_number(number)
				}
			}
			Leaf::Text(text) => Value::Str(text),
			Leaf::Interval(interval) => interval,
			Leaf::Geometry(text) => {
				if typed {
					Value::Str(text)
				} else {
					return Err(DuckError::new("Unsupported type: ANY"));
				}
			}
		})
	}
}

fn in_int32(number: f64) -> bool {
	(-2_147_483_648.0..=2_147_483_647.0).contains(&number)
}

enum Decoded {
	Null,
	Leaf(Leaf),
	/// Fields in node-api's object order: first position, last value per key.
	Object(Vec<(String, Node)>),
	/// Items, and whether node-api found one type among the non-null ones
	/// (then it keeps none of their types).
	Array { items: Vec<Node>, uniform: bool },
}

/// One decoded node and the type node-api records for it, as a comparable key
/// (`JSON.stringify(type.toJson())` decides whether ARRAY items are uniform).
struct Node {
	value: Decoded,
	kind: Value,
}

/// The vectors and row-local list slices of one VARIANT row.
struct Row<'a> {
	blob: &'a [u8],
	keys: ffi::duckdb_vector,
	keys_entry: ffi::duckdb_list_entry,
	keys_index: *const u32,
	values_index: *const u32,
	children_entry: ffi::duckdb_list_entry,
	type_ids: *const u8,
	byte_offsets: *const u32,
	values_entry: ffi::duckdb_list_entry,
}

/// Row `row` of a valid VARIANT vector, converted as node-api converts it.
///
/// # Safety
/// `vector` is a live VARIANT vector and `row` is a valid row within it.
pub(super) unsafe fn read(vector: ffi::duckdb_vector, row: usize) -> Result<Value> {
	unsafe {
		let child = |index: ffi::idx_t| ffi::duckdb_struct_vector_get_child(vector, index);
		let (keys, children, values, data) = (child(0), child(1), child(2), child(3));
		if !super::valid(data, row) {
			// A valid row's data BLOB is expected; node-api reads a missing one as NULL.
			return Ok(Value::Null);
		}
		let entry = |list: ffi::duckdb_vector| ffi::duckdb_vector_get_data(list).cast::<ffi::duckdb_list_entry>().add(row).read_unaligned();
		let pairs = ffi::duckdb_list_vector_get_child(children);
		let nodes = ffi::duckdb_list_vector_get_child(values);
		let column = |parent: ffi::duckdb_vector, index: ffi::idx_t| ffi::duckdb_vector_get_data(ffi::duckdb_struct_vector_get_child(parent, index));
		let row = Row {
			blob: string_bytes(data, row),
			keys: ffi::duckdb_list_vector_get_child(keys),
			keys_entry: entry(keys),
			keys_index: column(pairs, 0).cast::<u32>(),
			values_index: column(pairs, 1).cast::<u32>(),
			children_entry: entry(children),
			type_ids: column(nodes, 0).cast::<u8>(),
			byte_offsets: column(nodes, 1).cast::<u32>(),
			values_entry: entry(values),
		};
		let root = row.decode(0)?;
		convert(root, true)
	}
}

/// node-api's `convertVariantInner`: containers walk their items untyped,
/// except the items of a non-uniform ARRAY, which keep their recorded types.
fn convert(node: Node, typed: bool) -> Result<Value> {
	match node.value {
		Decoded::Null => Ok(Value::Null),
		Decoded::Leaf(leaf) => leaf.convert(typed),
		Decoded::Object(fields) => {
			let mut obj = Obj::new();
			for (key, field) in fields {
				obj.insert(key, convert(field, false)?);
			}
			Ok(Value::Obj(obj))
		}
		Decoded::Array { items, uniform } => items.into_iter().map(|item| convert(item, !uniform)).collect::<Result<Vec<_>>>().map(Value::Arr),
	}
}

fn type_key(name: &str) -> Value {
	Value::Str(name.to_owned())
}

fn wrapped_key(wrapper: &str, inner: Value) -> Value {
	let mut obj = Obj::new();
	obj.insert(wrapper, inner);
	Value::Obj(obj)
}

/// `varintDecode`: an unsigned LEB128 value of at most 32 bits, and the offset after it.
fn varint(blob: &[u8], offset: usize) -> Result<(usize, usize)> {
	let mut value: u64 = 0;
	let mut shift = 0;
	let mut at = offset;
	loop {
		let &byte = blob.get(at).ok_or_else(|| DuckError::new("varint truncated"))?;
		at += 1;
		value |= u64::from(byte & 0x7f) << shift;
		if byte & 0x80 == 0 {
			break;
		}
		shift += 7;
		if shift >= 35 {
			return Err(DuckError::new("varint overflow"));
		}
	}
	let value = u32::try_from(value).map_err(|_| DuckError::new("varint overflow"))?;
	Ok((value as usize, at))
}

/// `size` bytes of the blob at `offset`, or the error a DataView read past the end raises.
fn bytes(blob: &[u8], offset: usize, size: usize) -> Result<&[u8]> {
	offset.checked_add(size).and_then(|end| blob.get(offset..end)).ok_or_else(|| DuckError::new("Offset is outside the bounds of the DataView"))
}

/// A length-prefixed payload: `<varint length><length bytes>`.
fn prefixed(blob: &[u8], offset: usize) -> Result<&[u8]> {
	let (length, start) = varint(blob, offset)?;
	start.checked_add(length).and_then(|end| blob.get(start..end)).ok_or_else(|| {
		DuckError::new(format!(
			"Malformed VARIANT payload: length-prefixed read of {length} bytes at offset {start} exceeds row blob size {}",
			blob.len()
		))
	})
}

/// DuckDB's text of a temporal payload of physical type `id` held in `payload`.
fn temporal(id: ffi::DUCKDB_TYPE, payload: &[u8]) -> Result<Leaf> {
	// SAFETY: `payload` holds the type's physical width; temporal_text reads it unaligned at row 0.
	unsafe { temporal_text(id, payload.as_ptr().cast_mut().cast(), 0) }.map(Leaf::Text)
}

impl Row<'_> {
	fn decode(&self, index: usize) -> Result<Node> {
		let length = self.values_entry.length as usize;
		if index >= length {
			return Err(DuckError::new(format!("Malformed VARIANT: value_index {index} out of bounds (row values length {length})")));
		}
		let at = self.values_entry.offset as usize + index;
		// SAFETY: `at` lies within this row's slice of the `values` child vectors.
		let (tag, offset) = unsafe { (*self.type_ids.add(at), self.byte_offsets.add(at).read_unaligned() as usize) };
		let blob = self.blob;
		let fixed = |size: usize| bytes(blob, offset, size);
		let leaf = |leaf: Leaf, kind: &str| Ok(Node { value: Decoded::Leaf(leaf), kind: type_key(kind) });
		match tag {
			tag::NULL => Ok(Node { value: Decoded::Null, kind: type_key("SQLNULL") }),
			tag::BOOL_TRUE => leaf(Leaf::Bool(true), "BOOLEAN"),
			tag::BOOL_FALSE => leaf(Leaf::Bool(false), "BOOLEAN"),
			tag::INT8 => leaf(Leaf::Integer(f64::from(fixed(1)?[0] as i8)), "TINYINT"),
			tag::INT16 => leaf(Leaf::Integer(f64::from(i16::from_le_bytes(array(fixed(2)?)))), "SMALLINT"),
			tag::INT32 => leaf(Leaf::Integer(f64::from(i32::from_le_bytes(array(fixed(4)?)))), "INTEGER"),
			tag::UINT8 => leaf(Leaf::Integer(f64::from(fixed(1)?[0])), "UTINYINT"),
			tag::UINT16 => leaf(Leaf::Integer(f64::from(u16::from_le_bytes(array(fixed(2)?)))), "USMALLINT"),
			tag::UINT32 => leaf(Leaf::Integer(f64::from(u32::from_le_bytes(array(fixed(4)?)))), "UINTEGER"),
			tag::INT64 => leaf(Leaf::Text(i64::from_le_bytes(array(fixed(8)?)).to_string()), "BIGINT"),
			tag::UINT64 => leaf(Leaf::Text(u64::from_le_bytes(array(fixed(8)?)).to_string()), "UBIGINT"),
			// SAFETY: 16 bytes were checked to lie within the blob.
			tag::INT128 => leaf(Leaf::Text(unsafe { hugeint(fixed(16)?.as_ptr()) }.to_string()), "HUGEINT"),
			tag::UINT128 => leaf(Leaf::Text(unsafe { uhugeint(fixed(16)?.as_ptr()) }.to_string()), "UHUGEINT"),
			tag::FLOAT => leaf(Leaf::Float(f64::from(f32::from_le_bytes(array(fixed(4)?)))), "FLOAT"),
			tag::DOUBLE => leaf(Leaf::Float(f64::from_le_bytes(array(fixed(8)?))), "DOUBLE"),
			tag::UUID => leaf(Leaf::Text(uuid_text(unsafe { hugeint(fixed(16)?.as_ptr()) })), "UUID"),
			tag::DATE => leaf(temporal(ffi::DUCKDB_TYPE_DUCKDB_TYPE_DATE, fixed(4)?)?, "DATE"),
			tag::TIME_MICROS => leaf(temporal(ffi::DUCKDB_TYPE_DUCKDB_TYPE_TIME, fixed(8)?)?, "TIME"),
			tag::TIME_NANOS => leaf(temporal(ffi::DUCKDB_TYPE_DUCKDB_TYPE_TIME_NS, fixed(8)?)?, "TIME_NS"),
			tag::TIME_MICROS_TZ => leaf(temporal(ffi::DUCKDB_TYPE_DUCKDB_TYPE_TIME_TZ, fixed(8)?)?, "TIMETZ"),
			tag::TIMESTAMP_SEC => leaf(temporal(ffi::DUCKDB_TYPE_DUCKDB_TYPE_TIMESTAMP_S, fixed(8)?)?, "TIMESTAMP_S"),
			tag::TIMESTAMP_MILIS => leaf(temporal(ffi::DUCKDB_TYPE_DUCKDB_TYPE_TIMESTAMP_MS, fixed(8)?)?, "TIMESTAMP_MS"),
			tag::TIMESTAMP_MICROS => leaf(temporal(ffi::DUCKDB_TYPE_DUCKDB_TYPE_TIMESTAMP, fixed(8)?)?, "TIMESTAMP"),
			tag::TIMESTAMP_NANOS => leaf(temporal(ffi::DUCKDB_TYPE_DUCKDB_TYPE_TIMESTAMP_NS, fixed(8)?)?, "TIMESTAMP_NS"),
			tag::TIMESTAMP_MICROS_TZ => leaf(temporal(ffi::DUCKDB_TYPE_DUCKDB_TYPE_TIMESTAMP_TZ, fixed(8)?)?, "TIMESTAMPTZ"),
			tag::TIMESTAMP_NANOS_TZ => leaf(temporal(ffi::DUCKDB_TYPE_DUCKDB_TYPE_TIMESTAMP_TZ_NS, fixed(8)?)?, "TIMESTAMPTZ_NS"),
			tag::INTERVAL => {
				let payload = fixed(16)?;
				let mut obj = Obj::new();
				obj.insert("months", Value::Num(f64::from(i32::from_le_bytes(array(&payload[0..4])))));
				obj.insert("days", Value::Num(f64::from(i32::from_le_bytes(array(&payload[4..8])))));
				obj.insert("micros", Value::Str(i64::from_le_bytes(array(&payload[8..16])).to_string()));
				leaf(Leaf::Interval(Value::Obj(obj)), "INTERVAL")
			}
			tag::VARCHAR => leaf(Leaf::Text(String::from_utf8_lossy(prefixed(blob, offset)?).into_owned()), "VARCHAR"),
			tag::BLOB => leaf(Leaf::Text(blob_text(prefixed(blob, offset)?)), "BLOB"),
			tag::BITSTRING => leaf(Leaf::Text(bit_text(prefixed(blob, offset)?)), "BIT"),
			tag::BIGNUM => leaf(Leaf::Text(bignum_text(prefixed(blob, offset)?)), "BIGNUM"),
			tag::GEOMETRY => leaf(Leaf::Geometry(blob_text(prefixed(blob, offset)?)), "GEOMETRY"),
			tag::DECIMAL => {
				let (width, next) = varint(blob, offset)?;
				let (scale, start) = varint(blob, next)?;
				let value: i128 = match width {
					0..=4 => i128::from(i16::from_le_bytes(array(bytes(blob, start, 2)?))),
					5..=9 => i128::from(i32::from_le_bytes(array(bytes(blob, start, 4)?))),
					10..=18 => i128::from(i64::from_le_bytes(array(bytes(blob, start, 8)?))),
					// SAFETY: 16 bytes were checked to lie within the blob.
					19..=38 => unsafe { hugeint(bytes(blob, start, 16)?.as_ptr()) },
					_ => return Err(DuckError::new(format!("VARIANT DECIMAL width too large: {width}"))),
				};
				let decimal = ffi::duckdb_decimal {
					width: width as u8,
					scale: scale as u8,
					value: ffi::duckdb_hugeint { lower: value as u64, upper: (value >> 64) as i64 },
				};
				// SAFETY: the value is created here and destroyed by value_text.
				let text = unsafe { value_text(ffi::duckdb_create_decimal(decimal))? };
				leaf(Leaf::Text(text), &format!("DECIMAL({width},{scale})"))
			}
			tag::OBJECT => {
				let (count, first) = self.nested_header(offset)?;
				let mut fields: Vec<(String, Node)> = Vec::with_capacity(count);
				let mut kinds = Obj::new();
				for k in 0..count {
					let pair = self.children_entry.offset as usize + first + k;
					// SAFETY: `pair` lies within this row's slice of the `children` child vectors.
					let (key_index, value_index) =
						unsafe { (self.keys_index.add(pair).read_unaligned() as usize, self.values_index.add(pair).read_unaligned() as usize) };
					let keys_length = self.keys_entry.length as usize;
					if key_index >= keys_length {
						return Err(DuckError::new(format!(
							"Malformed VARIANT: keys_index {key_index} out of bounds (row keys length {keys_length})"
						)));
					}
					// SAFETY: the key lies within this row's slice of the `keys` child vector.
					let key = unsafe { self.key(self.keys_entry.offset as usize + key_index) };
					let field = self.decode(value_index)?;
					kinds.insert(key.clone(), field.kind.clone());
					// `entries[key] = value`: a repeated key keeps its first position and takes the last value.
					match fields.iter_mut().find(|(name, _)| *name == key) {
						Some(slot) => slot.1 = field,
						None => fields.push((key, field)),
					}
				}
				Ok(Node { value: Decoded::Object(fields), kind: wrapped_key("STRUCT", Value::Obj(kinds)) })
			}
			tag::ARRAY => {
				let (count, first) = self.nested_header(offset)?;
				let mut items = Vec::with_capacity(count);
				for k in 0..count {
					let pair = self.children_entry.offset as usize + first + k;
					// SAFETY: `pair` lies within this row's slice of the `children` child vectors.
					let value_index = unsafe { self.values_index.add(pair).read_unaligned() } as usize;
					items.push(self.decode(value_index)?);
				}
				// uniformVariantChildType: SQLNULL items fit any type; all-null (or none) is SQLNULL.
				let null = type_key("SQLNULL");
				let mut common: Option<&Value> = None;
				let mut uniform = true;
				for item in items.iter().filter(|item| item.kind != null) {
					match common {
						None => common = Some(&item.kind),
						Some(kind) if *kind == item.kind => {}
						Some(_) => {
							uniform = false;
							break;
						}
					}
				}
				let kind = if uniform { wrapped_key("LIST", common.cloned().unwrap_or(null)) } else { wrapped_key("LIST", type_key("VARIANT")) };
				Ok(Node { value: Decoded::Array { items, uniform }, kind })
			}
			other => Err(DuckError::new(format!("Unknown VARIANT type id: {other}"))),
		}
	}

	/// An OBJECT or ARRAY header: `<varint child_count>`, then `<varint children_idx>` when the count is not zero.
	fn nested_header(&self, offset: usize) -> Result<(usize, usize)> {
		let (count, next) = varint(self.blob, offset)?;
		if count == 0 {
			return Ok((0, 0));
		}
		let (first, _) = varint(self.blob, next)?;
		let length = self.children_entry.length as usize;
		if first + count > length {
			return Err(DuckError::new(format!(
				"Malformed VARIANT: children slice [{first}, {}) out of bounds (row children length {length})",
				first + count
			)));
		}
		Ok((count, first))
	}

	/// # Safety
	/// `at` is a row of the `keys` child vector.
	unsafe fn key(&self, at: usize) -> String {
		if super::valid(self.keys, at) { String::from_utf8_lossy(unsafe { string_bytes(self.keys, at) }).into_owned() } else { "null".to_owned() }
	}
}

fn array<const N: usize>(slice: &[u8]) -> [u8; N] {
	let mut out = [0u8; N];
	out.copy_from_slice(&slice[..N]);
	out
}

/// `getBigNumFromBytes` then `String(bigint)`: a 3-byte header whose top bit
/// marks a positive value, then the magnitude big-endian, every byte inverted
/// when the value is negative.
pub(super) fn bignum_text(stored: &[u8]) -> String {
	let Some(&first) = stored.first() else { return "0".to_owned() };
	let positive = first & 0x80 != 0;
	let mask = if positive { 0 } else { 0xff };
	let magnitude: Vec<u8> = stored.iter().skip(3).map(|byte| byte ^ mask).collect();
	decimal_text(!positive, limbs_of_big_endian(&magnitude))
}

/// `BigInt(value).toString()` of an integral double.
fn integral_text(value: f64) -> String {
	if value.abs() < 9.0e18 {
		return (value as i64).to_string();
	}
	let bits = value.to_bits();
	let exponent = ((bits >> 52) & 0x7ff) as i64 - 1075;
	let mantissa = (bits & ((1u64 << 52) - 1)) | (1u64 << 52);
	// |value| = mantissa · 2^exponent, exponent ≥ 0 at this size.
	let mut limbs = vec![mantissa as u32, (mantissa >> 32) as u32];
	let (words, shift) = ((exponent / 32) as usize, (exponent % 32) as u32);
	if shift > 0 {
		let mut carry = 0u32;
		for limb in &mut limbs {
			let next = *limb >> (32 - shift);
			*limb = (*limb << shift) | carry;
			carry = next;
		}
		limbs.push(carry);
	}
	let mut shifted = vec![0u32; words];
	shifted.extend(limbs);
	decimal_text(value < 0.0, shifted)
}

/// Little-endian 32-bit limbs of a big-endian magnitude.
fn limbs_of_big_endian(bytes: &[u8]) -> Vec<u32> {
	bytes.rchunks(4).map(|chunk| chunk.iter().fold(0u32, |acc, &byte| (acc << 8) | u32::from(byte))).collect()
}

/// The decimal text of a magnitude in little-endian 32-bit limbs, signed.
fn decimal_text(negative: bool, mut limbs: Vec<u32>) -> String {
	while limbs.last() == Some(&0) {
		limbs.pop();
	}
	if limbs.is_empty() {
		return "0".to_owned();
	}
	let mut groups: Vec<u32> = Vec::new();
	while !limbs.is_empty() {
		let mut remainder = 0u64;
		for limb in limbs.iter_mut().rev() {
			let current = (remainder << 32) | u64::from(*limb);
			*limb = (current / 1_000_000_000) as u32;
			remainder = current % 1_000_000_000;
		}
		groups.push(remainder as u32);
		while limbs.last() == Some(&0) {
			limbs.pop();
		}
	}
	let mut text = String::new();
	if negative {
		text.push('-');
	}
	let mut groups = groups.iter().rev();
	if let Some(top) = groups.next() {
		text.push_str(&top.to_string());
	}
	for group in groups {
		text.push_str(&format!("{group:09}"));
	}
	text
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn bignum_bytes_read_as_node_api_reads_them() {
		// 0x800002 header, magnitude 0x0100 = 256
		assert_eq!(bignum_text(&[0x80, 0x00, 0x02, 0x01, 0x00]), "256");
		// negative: header and magnitude inverted
		assert_eq!(bignum_text(&[0x7f, 0xff, 0xfd, 0xfe, 0xff]), "-256");
		assert_eq!(bignum_text(&[0x80, 0x00, 0x01, 0x00]), "0");
	}

	#[test]
	fn integral_doubles_read_as_javascript_bigint_text() {
		assert_eq!(integral_text(4_294_967_295.0), "4294967295");
		assert_eq!(integral_text(1e20), "100000000000000000000");
		assert_eq!(integral_text(-2f64.powi(70)), "-1180591620717411303424");
	}

	#[test]
	fn inferred_and_recorded_numbers_differ_only_past_32_bits() {
		assert_eq!(Leaf::Integer(4_294_967_295.0).convert(true).unwrap(), Value::Num(4_294_967_295.0));
		assert_eq!(Leaf::Integer(4_294_967_295.0).convert(false).unwrap(), Value::Str("4294967295".into()));
		assert_eq!(Leaf::Float(3.0).convert(false).unwrap(), Value::Num(3.0));
		assert_eq!(Leaf::Float(1.5).convert(false).unwrap(), Value::Num(1.5));
		assert_eq!(Leaf::Float(f64::NAN).convert(false).unwrap(), Value::Str("NaN".into()));
		assert!(Leaf::Geometry("x".into()).convert(false).is_err());
	}
}
