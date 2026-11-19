//! Result values as JSON, by `@duckdb/node-api`'s `JsonDuckDBValueConverter`
//! (the converter the TypeScript `session_query` used), so a `sql` result
//! reads as it did: 8- to 32-bit integers are numbers and wider integers are
//! text; FLOAT and DOUBLE are numbers unless not finite (then `NaN`,
//! `Infinity`, `-Infinity`); dates, times, timestamps and decimals are
//! DuckDB's own text for the value; INTERVAL is `{months, days, micros}` with
//! `micros` as text; LIST and ARRAY are arrays; a STRUCT is an object, or an
//! array when its entries are all unnamed; MAP is `[{key, value}]`; UNION is
//! `{tag, value}`; BLOB escapes bytes as DuckDB casts them to VARCHAR, and
//! GEOMETRY is its WKB written the same way; BIGNUM is its decimal text;
//! VARIANT is decoded and converted as node-api does it ([`variant`]).
//!
//! A column's reader is built once from its logical type ([`Reader::of`]) and
//! then reads any row of a vector of that type.

use std::ffi::c_char;

mod variant;

use libduckdb_sys as ffi;

use super::{Column, DuckError, LogicalType, Result, take_owned};
use crate::json::{Obj, Value};

/// How one logical type's values are read.
pub enum Reader {
	Null,
	Bool,
	I8,
	I16,
	I32,
	U8,
	U16,
	U32,
	I64,
	U64,
	F32,
	F64,
	I128,
	U128,
	Varchar,
	Blob,
	Uuid,
	Bit,
	Bignum,
	Geometry,
	Variant,
	Interval,
	/// A value whose text DuckDB writes: dates, times, timestamps.
	Temporal(ffi::DUCKDB_TYPE),
	Decimal { width: u8, scale: u8, storage: ffi::DUCKDB_TYPE },
	Enum { storage: ffi::DUCKDB_TYPE, dictionary: Vec<String> },
	List(Box<Reader>),
	Array(Box<Reader>, usize),
	/// Entry names and readers; a tuple when every name is empty.
	Struct(Vec<(String, Reader)>),
	Map(Box<Reader>, Box<Reader>),
	Union(Vec<(String, Reader)>),
	/// A type this converter does not read; its name.
	Unsupported(&'static str),
}

/// DuckDB's name of a type id (`DuckDBTypeId[id]` in `@duckdb/node-api`).
pub fn type_name(id: ffi::DUCKDB_TYPE) -> &'static str {
	match id {
		ffi::DUCKDB_TYPE_DUCKDB_TYPE_BOOLEAN => "BOOLEAN",
		ffi::DUCKDB_TYPE_DUCKDB_TYPE_TINYINT => "TINYINT",
		ffi::DUCKDB_TYPE_DUCKDB_TYPE_SMALLINT => "SMALLINT",
		ffi::DUCKDB_TYPE_DUCKDB_TYPE_INTEGER => "INTEGER",
		ffi::DUCKDB_TYPE_DUCKDB_TYPE_BIGINT => "BIGINT",
		ffi::DUCKDB_TYPE_DUCKDB_TYPE_UTINYINT => "UTINYINT",
		ffi::DUCKDB_TYPE_DUCKDB_TYPE_USMALLINT => "USMALLINT",
		ffi::DUCKDB_TYPE_DUCKDB_TYPE_UINTEGER => "UINTEGER",
		ffi::DUCKDB_TYPE_DUCKDB_TYPE_UBIGINT => "UBIGINT",
		ffi::DUCKDB_TYPE_DUCKDB_TYPE_FLOAT => "FLOAT",
		ffi::DUCKDB_TYPE_DUCKDB_TYPE_DOUBLE => "DOUBLE",
		ffi::DUCKDB_TYPE_DUCKDB_TYPE_TIMESTAMP => "TIMESTAMP",
		ffi::DUCKDB_TYPE_DUCKDB_TYPE_DATE => "DATE",
		ffi::DUCKDB_TYPE_DUCKDB_TYPE_TIME => "TIME",
		ffi::DUCKDB_TYPE_DUCKDB_TYPE_INTERVAL => "INTERVAL",
		ffi::DUCKDB_TYPE_DUCKDB_TYPE_HUGEINT => "HUGEINT",
		ffi::DUCKDB_TYPE_DUCKDB_TYPE_VARCHAR => "VARCHAR",
		ffi::DUCKDB_TYPE_DUCKDB_TYPE_BLOB => "BLOB",
		ffi::DUCKDB_TYPE_DUCKDB_TYPE_DECIMAL => "DECIMAL",
		ffi::DUCKDB_TYPE_DUCKDB_TYPE_TIMESTAMP_S => "TIMESTAMP_S",
		ffi::DUCKDB_TYPE_DUCKDB_TYPE_TIMESTAMP_MS => "TIMESTAMP_MS",
		ffi::DUCKDB_TYPE_DUCKDB_TYPE_TIMESTAMP_NS => "TIMESTAMP_NS",
		ffi::DUCKDB_TYPE_DUCKDB_TYPE_ENUM => "ENUM",
		ffi::DUCKDB_TYPE_DUCKDB_TYPE_LIST => "LIST",
		ffi::DUCKDB_TYPE_DUCKDB_TYPE_STRUCT => "STRUCT",
		ffi::DUCKDB_TYPE_DUCKDB_TYPE_MAP => "MAP",
		ffi::DUCKDB_TYPE_DUCKDB_TYPE_UUID => "UUID",
		ffi::DUCKDB_TYPE_DUCKDB_TYPE_UNION => "UNION",
		ffi::DUCKDB_TYPE_DUCKDB_TYPE_BIT => "BIT",
		ffi::DUCKDB_TYPE_DUCKDB_TYPE_TIME_TZ => "TIME_TZ",
		ffi::DUCKDB_TYPE_DUCKDB_TYPE_TIMESTAMP_TZ => "TIMESTAMP_TZ",
		ffi::DUCKDB_TYPE_DUCKDB_TYPE_UHUGEINT => "UHUGEINT",
		ffi::DUCKDB_TYPE_DUCKDB_TYPE_ARRAY => "ARRAY",
		ffi::DUCKDB_TYPE_DUCKDB_TYPE_ANY => "ANY",
		ffi::DUCKDB_TYPE_DUCKDB_TYPE_BIGNUM => "BIGNUM",
		ffi::DUCKDB_TYPE_DUCKDB_TYPE_SQLNULL => "SQLNULL",
		ffi::DUCKDB_TYPE_DUCKDB_TYPE_STRING_LITERAL => "STRING_LITERAL",
		ffi::DUCKDB_TYPE_DUCKDB_TYPE_INTEGER_LITERAL => "INTEGER_LITERAL",
		ffi::DUCKDB_TYPE_DUCKDB_TYPE_TIME_NS => "TIME_NS",
		ffi::DUCKDB_TYPE_DUCKDB_TYPE_GEOMETRY => "GEOMETRY",
		ffi::DUCKDB_TYPE_DUCKDB_TYPE_VARIANT => "VARIANT",
		ffi::DUCKDB_TYPE_DUCKDB_TYPE_TIMESTAMP_TZ_NS => "TIMESTAMP_TZ_NS",
		_ => "INVALID",
	}
}

impl Reader {
	/// The reader for values of `logical`.
	pub fn of(logical: &LogicalType) -> Reader {
		let raw = logical.raw();
		let id = logical.id();
		let child = |of: ffi::duckdb_logical_type| Reader::of(&LogicalType(of));
		let entries = |count: ffi::idx_t,
		               name: unsafe extern "C" fn(ffi::duckdb_logical_type, ffi::idx_t) -> *mut c_char,
		               member: unsafe extern "C" fn(ffi::duckdb_logical_type, ffi::idx_t) -> ffi::duckdb_logical_type| {
			(0..count)
				.map(|index| (take_owned(unsafe { name(raw, index) }).unwrap_or_default(), child(unsafe { member(raw, index) })))
				.collect::<Vec<_>>()
		};
		match id {
			ffi::DUCKDB_TYPE_DUCKDB_TYPE_SQLNULL => Reader::Null,
			ffi::DUCKDB_TYPE_DUCKDB_TYPE_BOOLEAN => Reader::Bool,
			ffi::DUCKDB_TYPE_DUCKDB_TYPE_TINYINT => Reader::I8,
			ffi::DUCKDB_TYPE_DUCKDB_TYPE_SMALLINT => Reader::I16,
			ffi::DUCKDB_TYPE_DUCKDB_TYPE_INTEGER => Reader::I32,
			ffi::DUCKDB_TYPE_DUCKDB_TYPE_UTINYINT => Reader::U8,
			ffi::DUCKDB_TYPE_DUCKDB_TYPE_USMALLINT => Reader::U16,
			ffi::DUCKDB_TYPE_DUCKDB_TYPE_UINTEGER => Reader::U32,
			ffi::DUCKDB_TYPE_DUCKDB_TYPE_BIGINT => Reader::I64,
			ffi::DUCKDB_TYPE_DUCKDB_TYPE_UBIGINT => Reader::U64,
			ffi::DUCKDB_TYPE_DUCKDB_TYPE_FLOAT => Reader::F32,
			ffi::DUCKDB_TYPE_DUCKDB_TYPE_DOUBLE => Reader::F64,
			ffi::DUCKDB_TYPE_DUCKDB_TYPE_HUGEINT => Reader::I128,
			ffi::DUCKDB_TYPE_DUCKDB_TYPE_UHUGEINT => Reader::U128,
			ffi::DUCKDB_TYPE_DUCKDB_TYPE_VARCHAR => Reader::Varchar,
			ffi::DUCKDB_TYPE_DUCKDB_TYPE_BLOB => Reader::Blob,
			ffi::DUCKDB_TYPE_DUCKDB_TYPE_UUID => Reader::Uuid,
			ffi::DUCKDB_TYPE_DUCKDB_TYPE_BIT => Reader::Bit,
			ffi::DUCKDB_TYPE_DUCKDB_TYPE_BIGNUM => Reader::Bignum,
			ffi::DUCKDB_TYPE_DUCKDB_TYPE_GEOMETRY => Reader::Geometry,
			ffi::DUCKDB_TYPE_DUCKDB_TYPE_VARIANT => Reader::Variant,
			ffi::DUCKDB_TYPE_DUCKDB_TYPE_INTERVAL => Reader::Interval,
			ffi::DUCKDB_TYPE_DUCKDB_TYPE_DATE
			| ffi::DUCKDB_TYPE_DUCKDB_TYPE_TIME
			| ffi::DUCKDB_TYPE_DUCKDB_TYPE_TIME_NS
			| ffi::DUCKDB_TYPE_DUCKDB_TYPE_TIME_TZ
			| ffi::DUCKDB_TYPE_DUCKDB_TYPE_TIMESTAMP
			| ffi::DUCKDB_TYPE_DUCKDB_TYPE_TIMESTAMP_S
			| ffi::DUCKDB_TYPE_DUCKDB_TYPE_TIMESTAMP_MS
			| ffi::DUCKDB_TYPE_DUCKDB_TYPE_TIMESTAMP_NS
			| ffi::DUCKDB_TYPE_DUCKDB_TYPE_TIMESTAMP_TZ
			| ffi::DUCKDB_TYPE_DUCKDB_TYPE_TIMESTAMP_TZ_NS => Reader::Temporal(id),
			ffi::DUCKDB_TYPE_DUCKDB_TYPE_DECIMAL => unsafe {
				Reader::Decimal {
					width: ffi::duckdb_decimal_width(raw),
					scale: ffi::duckdb_decimal_scale(raw),
					storage: ffi::duckdb_decimal_internal_type(raw),
				}
			},
			ffi::DUCKDB_TYPE_DUCKDB_TYPE_ENUM => {
				let size = unsafe { ffi::duckdb_enum_dictionary_size(raw) };
				let dictionary = (0..size)
					.map(|index| take_owned(unsafe { ffi::duckdb_enum_dictionary_value(raw, ffi::idx_t::from(index)) }).unwrap_or_default())
					.collect();
				Reader::Enum { storage: unsafe { ffi::duckdb_enum_internal_type(raw) }, dictionary }
			}
			ffi::DUCKDB_TYPE_DUCKDB_TYPE_LIST => Reader::List(Box::new(child(unsafe { ffi::duckdb_list_type_child_type(raw) }))),
			ffi::DUCKDB_TYPE_DUCKDB_TYPE_ARRAY => Reader::Array(
				Box::new(child(unsafe { ffi::duckdb_array_type_child_type(raw) })),
				usize::try_from(unsafe { ffi::duckdb_array_type_array_size(raw) }).unwrap_or(0),
			),
			ffi::DUCKDB_TYPE_DUCKDB_TYPE_MAP => Reader::Map(
				Box::new(child(unsafe { ffi::duckdb_map_type_key_type(raw) })),
				Box::new(child(unsafe { ffi::duckdb_map_type_value_type(raw) })),
			),
			ffi::DUCKDB_TYPE_DUCKDB_TYPE_STRUCT => Reader::Struct(entries(
				unsafe { ffi::duckdb_struct_type_child_count(raw) },
				ffi::duckdb_struct_type_child_name,
				ffi::duckdb_struct_type_child_type,
			)),
			ffi::DUCKDB_TYPE_DUCKDB_TYPE_UNION => Reader::Union(entries(
				unsafe { ffi::duckdb_union_type_member_count(raw) },
				ffi::duckdb_union_type_member_name,
				ffi::duckdb_union_type_member_type,
			)),
			other => Reader::Unsupported(type_name(other)),
		}
	}

	/// Row `row` of `column` (a column of this reader's type) as JSON.
	pub fn read(&self, column: Column<'_>, row: usize) -> Result<Value> {
		if row >= column.len {
			return Err(DuckError::new(format!("row {row} lies past the chunk's {} rows", column.len)));
		}
		// SAFETY: the column belongs to a live chunk and `row` lies within it.
		unsafe { self.read_raw(column.raw, row) }
	}

	/// # Safety
	/// `vector` is a live vector of this reader's type and `row` is within it.
	unsafe fn read_raw(&self, vector: ffi::duckdb_vector, row: usize) -> Result<Value> {
		if !valid(vector, row) {
			return Ok(Value::Null);
		}
		// SAFETY: `vector` holds values of this reader's type and `row` is within
		// it, so each cast below reads the type's physical layout.
		unsafe {
			let data = ffi::duckdb_vector_get_data(vector);
			let at = |size: usize| data.cast::<u8>().add(row * size);
			Ok(match self {
				Reader::Null => Value::Null,
				Reader::Bool => Value::Bool(*at(1) != 0),
				Reader::I8 => Value::Num(f64::from(*at(1).cast::<i8>())),
				Reader::I16 => Value::Num(f64::from(at(2).cast::<i16>().read_unaligned())),
				Reader::I32 => Value::Num(f64::from(at(4).cast::<i32>().read_unaligned())),
				Reader::U8 => Value::Num(f64::from(*at(1))),
				Reader::U16 => Value::Num(f64::from(at(2).cast::<u16>().read_unaligned())),
				Reader::U32 => Value::Num(f64::from(at(4).cast::<u32>().read_unaligned())),
				Reader::I64 => Value::Str(at(8).cast::<i64>().read_unaligned().to_string()),
				Reader::U64 => Value::Str(at(8).cast::<u64>().read_unaligned().to_string()),
				Reader::F32 => json_number(f64::from(at(4).cast::<f32>().read_unaligned())),
				Reader::F64 => json_number(at(8).cast::<f64>().read_unaligned()),
				Reader::I128 => Value::Str(hugeint(at(16)).to_string()),
				Reader::U128 => Value::Str(uhugeint(at(16)).to_string()),
				Reader::Varchar => Value::Str(String::from_utf8_lossy(string_bytes(vector, row)).into_owned()),
				Reader::Blob => Value::Str(blob_text(string_bytes(vector, row))),
				Reader::Uuid => Value::Str(uuid_text(hugeint(at(16)))),
				Reader::Bit => Value::Str(bit_text(string_bytes(vector, row))),
				Reader::Bignum => Value::Str(variant::bignum_text(string_bytes(vector, row))),
				Reader::Geometry => Value::Str(blob_text(string_bytes(vector, row))),
				Reader::Variant => variant::read(vector, row)?,
				Reader::Interval => {
					let interval = at(16).cast::<ffi::duckdb_interval>().read_unaligned();
					let mut obj = Obj::new();
					obj.insert("months", Value::Num(f64::from(interval.months)));
					obj.insert("days", Value::Num(f64::from(interval.days)));
					obj.insert("micros", Value::Str(interval.micros.to_string()));
					Value::Obj(obj)
				}
				Reader::Temporal(id) => Value::Str(temporal_text(*id, data, row)?),
				Reader::Decimal { width, scale, storage } => {
					let value: i128 = match *storage {
						ffi::DUCKDB_TYPE_DUCKDB_TYPE_SMALLINT => i128::from(at(2).cast::<i16>().read_unaligned()),
						ffi::DUCKDB_TYPE_DUCKDB_TYPE_INTEGER => i128::from(at(4).cast::<i32>().read_unaligned()),
						ffi::DUCKDB_TYPE_DUCKDB_TYPE_BIGINT => i128::from(at(8).cast::<i64>().read_unaligned()),
						_ => hugeint(at(16)),
					};
					let decimal = ffi::duckdb_decimal {
						width: *width,
						scale: *scale,
						value: ffi::duckdb_hugeint { lower: value as u64, upper: (value >> 64) as i64 },
					};
					Value::Str(value_text(ffi::duckdb_create_decimal(decimal))?)
				}
				Reader::Enum { storage, dictionary } => {
					let index = match *storage {
						ffi::DUCKDB_TYPE_DUCKDB_TYPE_UTINYINT => usize::from(*at(1)),
						ffi::DUCKDB_TYPE_DUCKDB_TYPE_USMALLINT => usize::from(at(2).cast::<u16>().read_unaligned()),
						_ => at(4).cast::<u32>().read_unaligned() as usize,
					};
					dictionary.get(index).map_or(Value::Null, |text| Value::Str(text.clone()))
				}
				Reader::List(item) => {
					let entry = at(16).cast::<ffi::duckdb_list_entry>().read_unaligned();
					let children = ffi::duckdb_list_vector_get_child(vector);
					let mut items = Vec::with_capacity(entry.length as usize);
					for index in entry.offset..entry.offset + entry.length {
						items.push(item.read_raw(children, index as usize)?);
					}
					Value::Arr(items)
				}
				Reader::Array(item, size) => {
					let children = ffi::duckdb_array_vector_get_child(vector);
					let mut items = Vec::with_capacity(*size);
					for index in row * size..(row + 1) * size {
						items.push(item.read_raw(children, index)?);
					}
					Value::Arr(items)
				}
				Reader::Map(key, value) => {
					let entry = at(16).cast::<ffi::duckdb_list_entry>().read_unaligned();
					let pairs = ffi::duckdb_list_vector_get_child(vector);
					let (keys, values) = (ffi::duckdb_struct_vector_get_child(pairs, 0), ffi::duckdb_struct_vector_get_child(pairs, 1));
					let mut items = Vec::with_capacity(entry.length as usize);
					for index in entry.offset..entry.offset + entry.length {
						let mut pair = Obj::new();
						pair.insert("key", key.read_raw(keys, index as usize)?);
						pair.insert("value", value.read_raw(values, index as usize)?);
						items.push(Value::Obj(pair));
					}
					Value::Arr(items)
				}
				Reader::Struct(entries) => {
					let tuple = !entries.is_empty() && entries.iter().all(|(name, _)| name.is_empty());
					if tuple {
						let mut items = Vec::with_capacity(entries.len());
						for (index, (_, reader)) in entries.iter().enumerate() {
							items.push(reader.read_raw(ffi::duckdb_struct_vector_get_child(vector, index as ffi::idx_t), row)?);
						}
						Value::Arr(items)
					} else {
						let mut obj = Obj::new();
						for (index, (name, reader)) in entries.iter().enumerate() {
							obj.insert(name.clone(), reader.read_raw(ffi::duckdb_struct_vector_get_child(vector, index as ffi::idx_t), row)?);
						}
						Value::Obj(obj)
					}
				}
				Reader::Union(members) => {
					// Physically a STRUCT: child 0 the tag, child 1 + tag the member.
					let tags = ffi::duckdb_struct_vector_get_child(vector, 0);
					let tag = usize::from(*ffi::duckdb_vector_get_data(tags).cast::<u8>().add(row));
					let Some((name, reader)) = members.get(tag) else {
						return Err(DuckError::new(format!("UNION tag {tag} has no member")));
					};
					let mut obj = Obj::new();
					obj.insert("tag", Value::Str(name.clone()));
					obj.insert("value", reader.read_raw(ffi::duckdb_struct_vector_get_child(vector, (tag + 1) as ffi::idx_t), row)?);
					Value::Obj(obj)
				}
				Reader::Unsupported(name) => return Err(DuckError::new(format!("Unsupported type: {name}"))),
			})
		}
	}
}

fn valid(vector: ffi::duckdb_vector, row: usize) -> bool {
	let validity = unsafe { ffi::duckdb_vector_get_validity(vector) };
	validity.is_null() || unsafe { ffi::duckdb_validity_row_is_valid(validity, row as ffi::idx_t) }
}

/// `jsonNumberFromValue`: a finite number as itself, else JavaScript's `String(value)`.
fn json_number(value: f64) -> Value {
	if value.is_finite() {
		Value::Num(value)
	} else if value.is_nan() {
		Value::Str("NaN".to_owned())
	} else {
		Value::Str(if value > 0.0 { "Infinity" } else { "-Infinity" }.to_owned())
	}
}

/// A `duckdb_hugeint` (lower u64, upper i64) at `at`.
unsafe fn hugeint(at: *const u8) -> i128 {
	let value = unsafe { at.cast::<ffi::duckdb_hugeint>().read_unaligned() };
	(i128::from(value.upper) << 64) | i128::from(value.lower)
}

unsafe fn uhugeint(at: *const u8) -> u128 {
	let value = unsafe { at.cast::<ffi::duckdb_uhugeint>().read_unaligned() };
	(u128::from(value.upper) << 64) | u128::from(value.lower)
}

/// The bytes of row `row` of a string vector (VARCHAR, BLOB, BIT).
///
/// # Safety
/// `vector` is a string vector of the current chunk and the row is valid.
unsafe fn string_bytes<'a>(vector: ffi::duckdb_vector, row: usize) -> &'a [u8] {
	unsafe {
		let data = ffi::duckdb_vector_get_data(vector).cast::<ffi::duckdb_string_t>().add(row);
		let length = ffi::duckdb_string_t_length(*data) as usize;
		let start = ffi::duckdb_string_t_data(data);
		std::slice::from_raw_parts(start.cast::<u8>(), length)
	}
}

/// The text of row `row` of a VARCHAR (or JSON) column; None when it is NULL or past the chunk.
pub fn varchar_at(column: Column<'_>, row: usize) -> Option<String> {
	if row >= column.len || !valid(column.raw, row) {
		return None;
	}
	// SAFETY: the column belongs to a live chunk and `row` lies within it.
	Some(String::from_utf8_lossy(unsafe { string_bytes(column.raw, row) }).into_owned())
}

/// `stringFromBlob`: printable ASCII as itself, every other byte and both quotes as `\xHH`.
fn blob_text(bytes: &[u8]) -> String {
	let mut out = String::with_capacity(bytes.len());
	for &byte in bytes {
		if byte <= 0x1f || byte == b'"' || byte == b'\'' || byte >= 0x7f {
			out.push_str(&format!("\\x{byte:02X}"));
		} else {
			out.push(char::from(byte));
		}
	}
	out
}

/// `DuckDBUUIDValue.toString`: the stored value has its top bit flipped.
fn uuid_text(stored: i128) -> String {
	let hex = format!("{:032x}", (stored as u128) ^ (1u128 << 127));
	format!("{}-{}-{}-{}-{}", &hex[0..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..32])
}

/// `DuckDBBitValue.toString`: byte 0 counts the padding bits that lead byte 1.
fn bit_text(data: &[u8]) -> String {
	let Some((&padding, bytes)) = data.split_first() else { return String::new() };
	let total = bytes.len() * 8;
	(usize::from(padding)..total).map(|offset| if bytes[offset / 8] >> (7 - offset % 8) & 1 == 1 { '1' } else { '0' }).collect()
}

/// DuckDB's text of `value` (its cast to VARCHAR), which this call destroys.
unsafe fn value_text(mut value: ffi::duckdb_value) -> Result<String> {
	let text = take_owned(unsafe { ffi::duckdb_get_varchar(value) });
	unsafe { ffi::duckdb_destroy_value(&mut value) };
	text.ok_or_else(|| DuckError::new("DuckDB could not write a value as text"))
}

/// The local UTC offset in minutes when the process first needs one, as
/// `@duckdb/node-api` fixes it (`-new Date().getTimezoneOffset()`).
static LOCAL_OFFSET_MINUTES: std::sync::LazyLock<i64> = std::sync::LazyLock::new(|| {
	let offset = jiff::tz::TimeZone::system().to_offset(jiff::Timestamp::now());
	i64::from(offset.seconds()) / 60
});

/// `getTimezoneOffsetString`: `+HH`, or `+HH:MM` when minutes are not zero.
fn offset_text(minutes: i64) -> String {
	let sign = if minutes < 0 { '-' } else { '+' };
	let (hours, minutes) = (minutes.abs() / 60, minutes.abs() % 60);
	if minutes == 0 { format!("{sign}{hours:02}") } else { format!("{sign}{hours:02}:{minutes:02}") }
}

/// A TIMESTAMPTZ (or its nanosecond form) as `@duckdb/node-api` writes it:
/// the wall time at the process's local offset, then that offset.
unsafe fn zoned_text(value: i64, units_per_minute: i64, nanos: bool) -> Result<String> {
	// DuckDB's infinities stay as they are, as in getDuckDBTimestampStringFromMicroseconds.
	if value == i64::MAX {
		return Ok("infinity".to_owned());
	}
	if value == -i64::MAX {
		return Ok("-infinity".to_owned());
	}
	let minutes = *LOCAL_OFFSET_MINUTES;
	let shifted = value.saturating_add(minutes * units_per_minute);
	let wall = unsafe {
		value_text(if nanos {
			ffi::duckdb_create_timestamp_ns(ffi::duckdb_timestamp_ns { nanos: shifted })
		} else {
			ffi::duckdb_create_timestamp(ffi::duckdb_timestamp { micros: shifted })
		})?
	};
	Ok(format!("{wall}{}", offset_text(minutes)))
}

/// DuckDB's text of a temporal value: the C API's value constructors take the
/// vector's physical form, and the value's text is DuckDB's cast to VARCHAR.
unsafe fn temporal_text(id: ffi::DUCKDB_TYPE, data: *mut std::ffi::c_void, row: usize) -> Result<String> {
	unsafe {
		let int32 = || data.cast::<i32>().add(row).read_unaligned();
		let int64 = || data.cast::<i64>().add(row).read_unaligned();
		match id {
			ffi::DUCKDB_TYPE_DUCKDB_TYPE_TIMESTAMP_TZ => return zoned_text(int64(), 60_000_000, false),
			ffi::DUCKDB_TYPE_DUCKDB_TYPE_TIMESTAMP_TZ_NS => return zoned_text(int64(), 60_000_000_000, true),
			_ => {}
		}
		let value = match id {
			ffi::DUCKDB_TYPE_DUCKDB_TYPE_DATE => ffi::duckdb_create_date(ffi::duckdb_date { days: int32() }),
			ffi::DUCKDB_TYPE_DUCKDB_TYPE_TIME => ffi::duckdb_create_time(ffi::duckdb_time { micros: int64() }),
			ffi::DUCKDB_TYPE_DUCKDB_TYPE_TIME_NS => ffi::duckdb_create_time_ns(ffi::duckdb_time_ns { nanos: int64() }),
			ffi::DUCKDB_TYPE_DUCKDB_TYPE_TIME_TZ => {
				ffi::duckdb_create_time_tz_value(ffi::duckdb_time_tz { bits: data.cast::<u64>().add(row).read_unaligned() })
			}
			ffi::DUCKDB_TYPE_DUCKDB_TYPE_TIMESTAMP_S => ffi::duckdb_create_timestamp_s(ffi::duckdb_timestamp_s { seconds: int64() }),
			ffi::DUCKDB_TYPE_DUCKDB_TYPE_TIMESTAMP_MS => ffi::duckdb_create_timestamp_ms(ffi::duckdb_timestamp_ms { millis: int64() }),
			ffi::DUCKDB_TYPE_DUCKDB_TYPE_TIMESTAMP_NS => ffi::duckdb_create_timestamp_ns(ffi::duckdb_timestamp_ns { nanos: int64() }),
			_ => ffi::duckdb_create_timestamp(ffi::duckdb_timestamp { micros: int64() }),
		};
		value_text(value)
	}
}

/// `valueChars` of the TypeScript `runSql`: the characters a converted value
/// spends of the result budget, without serializing it: a string by its
/// UTF-16 length, a number, boolean or null by its JSON text, and a list or
/// object by its values.
pub fn value_chars(value: &Value) -> usize {
	match value {
		Value::Null => 4,
		Value::Bool(flag) => if *flag { 4 } else { 5 },
		Value::Num(number) => {
			let mut text = String::new();
			crate::json::write_number(&mut text, *number);
			text.len()
		}
		Value::Str(text) => crate::json::utf16_len(text),
		Value::Clipped(clipped) => crate::json::utf16_len(&clipped.marked()),
		Value::Arr(items) => items.iter().map(value_chars).sum(),
		Value::Obj(obj) => obj.iter().map(|(_, item)| value_chars(item)).sum(),
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn blob_bit_and_uuid_text_match_node_api() {
		assert_eq!(blob_text(b"a'\"\x00\x7f~"), "a\\x27\\x22\\x00\\x7F~");
		// padding 3, bits 10110 after it
		assert_eq!(bit_text(&[3, 0b0001_0110]), "10110");
		assert_eq!(uuid_text((0x0011_2233_4455_6677_8899_aabb_ccdd_eeffu128 ^ (1u128 << 127)) as i128), "00112233-4455-6677-8899-aabbccddeeff");
	}

	#[test]
	fn not_finite_numbers_are_text() {
		assert_eq!(json_number(f64::NAN), Value::Str("NaN".into()));
		assert_eq!(json_number(f64::NEG_INFINITY), Value::Str("-Infinity".into()));
		assert_eq!(json_number(1.5), Value::Num(1.5));
	}
}
