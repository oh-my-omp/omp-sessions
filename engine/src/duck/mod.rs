//! A safe layer over DuckDB's C API (`libduckdb-sys`; how DuckDB is linked is a
//! feature of `engine/Cargo.toml`): one in-memory database with its connection, statement extraction
//! and preparation, parameter binding, streaming results, appenders and
//! interruption, as the query process uses them. `convert` turns result
//! vectors into JSON values; `sql_type` writes a column type as SQL.
//!
//! Ownership follows DuckDB's: a connection keeps its database open, and every
//! extracted statement, prepared statement, result and appender keeps its
//! connection open (`Arc<Conn>`), so no handle outlives what it points into.

pub mod convert;

use std::ffi::{CStr, CString, c_char};
use std::ptr;
use std::sync::Arc;

use libduckdb_sys as ffi;

/// A DuckDB error: its message, and whether DuckDB reports it as an interrupted query.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DuckError {
	pub message: String,
	pub interrupted: bool,
}

impl DuckError {
	pub fn new(message: impl Into<String>) -> Self {
		Self { message: message.into(), interrupted: false }
	}
}

pub type Result<T> = std::result::Result<T, DuckError>;

fn cstring(text: &str) -> Result<CString> {
	CString::new(text).map_err(|_| DuckError::new("SQL text holds a NUL character"))
}

/// The text of a DuckDB-owned C string, or `fallback` when it is null.
///
/// # Safety
/// `text` is null or a NUL-terminated string that lives for this call.
unsafe fn text_of(text: *const c_char, fallback: &str) -> String {
	if text.is_null() { fallback.to_owned() } else { unsafe { CStr::from_ptr(text) }.to_string_lossy().into_owned() }
}

/// Takes a string DuckDB allocated for the caller, freeing it.
fn take_owned(text: *mut c_char) -> Option<String> {
	if text.is_null() {
		return None;
	}
	let owned = unsafe { CStr::from_ptr(text) }.to_string_lossy().into_owned();
	unsafe { ffi::duckdb_free(text.cast()) };
	Some(owned)
}

struct Db(ffi::duckdb_database);

impl Drop for Db {
	fn drop(&mut self) {
		unsafe { ffi::duckdb_close(&mut self.0) };
	}
}

struct Conn {
	raw: ffi::duckdb_connection,
	_db: Db,
}

// A connection is used by one thread at a time; `duckdb_interrupt` is the one
// call made from another thread while a query runs, and DuckDB makes it safe
// to call concurrently.
unsafe impl Send for Conn {}
unsafe impl Sync for Conn {}

impl Drop for Conn {
	fn drop(&mut self) {
		unsafe { ffi::duckdb_disconnect(&mut self.raw) };
	}
}

/// An in-memory database with its single connection.
pub struct Duck {
	conn: Arc<Conn>,
}

/// Interrupts whatever the connection is running; callable from another thread.
#[derive(Clone)]
pub struct Interrupter(Arc<Conn>);

impl Interrupter {
	pub fn interrupt(&self) {
		unsafe { ffi::duckdb_interrupt(self.0.raw) };
	}
}

impl Duck {
	/// Opens an in-memory database with DuckDB's defaults and connects to it;
	/// the caller configures it with `SET` statements.
	pub fn open() -> Result<Self> {
		let mut db: ffi::duckdb_database = ptr::null_mut();
		let mut error: *mut c_char = ptr::null_mut();
		let state = unsafe { ffi::duckdb_open_ext(ptr::null(), &mut db, ptr::null_mut(), &mut error) };
		if state != ffi::DuckDBSuccess {
			return Err(DuckError::new(take_owned(error).unwrap_or_else(|| "could not open DuckDB".to_owned())));
		}
		let db = Db(db);
		let mut raw: ffi::duckdb_connection = ptr::null_mut();
		if unsafe { ffi::duckdb_connect(db.0, &mut raw) } != ffi::DuckDBSuccess {
			return Err(DuckError::new("could not connect to DuckDB"));
		}
		Ok(Self { conn: Arc::new(Conn { raw, _db: db }) })
	}

	pub fn interrupter(&self) -> Interrupter {
		Interrupter(Arc::clone(&self.conn))
	}

	/// Runs `sql` (one or more statements) and discards its result.
	pub fn run(&self, sql: &str) -> Result<()> {
		let sql = cstring(sql)?;
		let mut result = Materialized::empty();
		let state = unsafe { ffi::duckdb_query(self.conn.raw, sql.as_ptr(), &mut result.0) };
		result.check(state)
	}

	/// Splits `sql` into its statements; a parse error is DuckDB's message.
	pub fn extract(&self, sql: &str) -> Result<Extracted> {
		let sql = cstring(sql)?;
		let mut raw: ffi::duckdb_extracted_statements = ptr::null_mut();
		let count = unsafe { ffi::duckdb_extract_statements(self.conn.raw, sql.as_ptr(), &mut raw) };
		let extracted = Extracted { raw, count: usize::try_from(count).unwrap_or(0), next: 0, conn: Arc::clone(&self.conn) };
		if count == 0 {
			let error = unsafe { ffi::duckdb_extract_statements_error(raw) };
			return Err(DuckError::new(unsafe { text_of(error, "query holds no statement") }));
		}
		Ok(extracted)
	}

	/// Prepares one statement.
	pub fn prepare(&self, sql: &str) -> Result<Prepared> {
		let sql = cstring(sql)?;
		let mut raw: ffi::duckdb_prepared_statement = ptr::null_mut();
		let state = unsafe { ffi::duckdb_prepare(self.conn.raw, sql.as_ptr(), &mut raw) };
		Prepared::checked(raw, state, &self.conn)
	}

	/// The first column of every row `sql` returns, as text (NULL as None).
	pub fn strings(&self, sql: &str, params: &[Scalar]) -> Result<Vec<Option<String>>> {
		let mut prepared = self.prepare(sql)?;
		for (index, value) in params.iter().enumerate() {
			prepared.bind(index + 1, value)?;
		}
		let mut stream = prepared.stream()?;
		let mut out = Vec::new();
		while let Some(chunk) = stream.next_chunk()? {
			let vector = chunk.vector(0);
			for row in 0..chunk.len() {
				out.push(convert::varchar_at(vector, row));
			}
		}
		Ok(out)
	}

	/// An appender on the existing table `schema.table`.
	pub fn appender(&self, schema: &str, table: &str) -> Result<Appender> {
		let (schema_c, table_c) = (cstring(schema)?, cstring(table)?);
		let mut raw: ffi::duckdb_appender = ptr::null_mut();
		let state =
			unsafe { ffi::duckdb_appender_create_ext(self.conn.raw, ptr::null(), schema_c.as_ptr(), table_c.as_ptr(), &mut raw) };
		let appender = Appender { raw, _conn: Arc::clone(&self.conn) };
		if state != ffi::DuckDBSuccess {
			return Err(appender.error("could not create an appender"));
		}
		Ok(appender)
	}
}

/// A materialized `duckdb_result` (statements run for their effect).
struct Materialized(ffi::duckdb_result);

impl Materialized {
	fn empty() -> Self {
		// SAFETY: duckdb_result is a plain C struct; all-zero is DuckDB's "no result".
		Self(unsafe { std::mem::zeroed() })
	}

	fn check(&mut self, state: ffi::duckdb_state) -> Result<()> {
		if state == ffi::DuckDBSuccess { Ok(()) } else { Err(result_error(&mut self.0)) }
	}
}

impl Drop for Materialized {
	fn drop(&mut self) {
		unsafe { ffi::duckdb_destroy_result(&mut self.0) };
	}
}

fn result_error(result: &mut ffi::duckdb_result) -> DuckError {
	let message = unsafe { text_of(ffi::duckdb_result_error(result), "DuckDB reported an error without a message") };
	let interrupted = unsafe { ffi::duckdb_result_error_type(result) } == ffi::duckdb_error_type_DUCKDB_ERROR_INTERRUPT;
	DuckError { message, interrupted }
}

/// The statements of one SQL text, prepared once each, in order: a statement
/// may name what an earlier one creates, so each is prepared only after the
/// previous one ran.
pub struct Extracted {
	raw: ffi::duckdb_extracted_statements,
	count: usize,
	next: usize,
	conn: Arc<Conn>,
}

impl Extracted {
	/// How many statements the text holds.
	pub fn total(&self) -> usize {
		self.count
	}
}

impl Iterator for Extracted {
	type Item = Result<Prepared>;

	/// Prepares the next statement against the database as it is now.
	fn next(&mut self) -> Option<Self::Item> {
		if self.next == self.count {
			return None;
		}
		let index = self.next as ffi::idx_t;
		self.next += 1;
		let mut raw: ffi::duckdb_prepared_statement = ptr::null_mut();
		let state = unsafe { ffi::duckdb_prepare_extracted_statement(self.conn.raw, self.raw, index, &mut raw) };
		Some(Prepared::checked(raw, state, &self.conn))
	}
}

impl Drop for Extracted {
	fn drop(&mut self) {
		unsafe { ffi::duckdb_destroy_extracted(&mut self.raw) };
	}
}

/// The type of a prepared statement, by DuckDB's `StatementType` numbering.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StatementType(ffi::duckdb_statement_type);

impl StatementType {
	pub const SELECT: Self = Self(ffi::duckdb_statement_type_DUCKDB_STATEMENT_TYPE_SELECT);
	pub const INSERT: Self = Self(ffi::duckdb_statement_type_DUCKDB_STATEMENT_TYPE_INSERT);
	pub const UPDATE: Self = Self(ffi::duckdb_statement_type_DUCKDB_STATEMENT_TYPE_UPDATE);
	pub const DELETE: Self = Self(ffi::duckdb_statement_type_DUCKDB_STATEMENT_TYPE_DELETE);
	pub const MERGE_INTO: Self = Self(ffi::duckdb_statement_type_DUCKDB_STATEMENT_TYPE_MERGE_INTO);
	pub const CREATE: Self = Self(ffi::duckdb_statement_type_DUCKDB_STATEMENT_TYPE_CREATE);
	pub const CREATE_FUNC: Self = Self(ffi::duckdb_statement_type_DUCKDB_STATEMENT_TYPE_CREATE_FUNC);
	pub const ALTER: Self = Self(ffi::duckdb_statement_type_DUCKDB_STATEMENT_TYPE_ALTER);
	pub const DROP: Self = Self(ffi::duckdb_statement_type_DUCKDB_STATEMENT_TYPE_DROP);
	pub const EXPLAIN: Self = Self(ffi::duckdb_statement_type_DUCKDB_STATEMENT_TYPE_EXPLAIN);

	/// The enum member's name (`COPY`, `VARIABLE_SET`, …), or its number for one this build does not name.
	pub fn name(self) -> String {
		const NAMES: [&str; 31] = [
			"INVALID",
			"SELECT",
			"INSERT",
			"UPDATE",
			"EXPLAIN",
			"DELETE",
			"PREPARE",
			"CREATE",
			"EXECUTE",
			"ALTER",
			"TRANSACTION",
			"COPY",
			"ANALYZE",
			"VARIABLE_SET",
			"CREATE_FUNC",
			"DROP",
			"EXPORT",
			"PRAGMA",
			"VACUUM",
			"CALL",
			"SET",
			"LOAD",
			"RELATION",
			"EXTENSION",
			"LOGICAL_PLAN",
			"ATTACH",
			"DETACH",
			"MULTI",
			"COPY_DATABASE",
			"UPDATE_EXTENSIONS",
			"MERGE_INTO",
		];
		usize::try_from(self.0)
			.ok()
			.and_then(|index| NAMES.get(index))
			.map_or_else(|| self.0.to_string(), |name| (*name).to_owned())
	}
}

/// A parameter value.
#[derive(Clone, Debug, PartialEq)]
pub enum Scalar {
	Null,
	Bool(bool),
	Num(f64),
	Str(String),
}

/// A prepared statement.
pub struct Prepared {
	raw: ffi::duckdb_prepared_statement,
	conn: Arc<Conn>,
}

impl Prepared {
	fn checked(raw: ffi::duckdb_prepared_statement, state: ffi::duckdb_state, conn: &Arc<Conn>) -> Result<Self> {
		let prepared = Self { raw, conn: Arc::clone(conn) };
		if state != ffi::DuckDBSuccess {
			let message = unsafe { text_of(ffi::duckdb_prepare_error(raw), "could not prepare the statement") };
			return Err(DuckError::new(message));
		}
		Ok(prepared)
	}

	pub fn statement_type(&self) -> StatementType {
		StatementType(unsafe { ffi::duckdb_prepared_statement_type(self.raw) })
	}

	pub fn column_count(&self) -> usize {
		usize::try_from(unsafe { ffi::duckdb_prepared_statement_column_count(self.raw) }).unwrap_or(0)
	}

	/// The 1-based index of the parameter `name`.
	pub fn parameter_index(&self, name: &str) -> Result<usize> {
		let name_c = cstring(name)?;
		let mut index: ffi::idx_t = 0;
		if unsafe { ffi::duckdb_bind_parameter_index(self.raw, &mut index, name_c.as_ptr()) } != ffi::DuckDBSuccess {
			return Err(DuckError::new(format!("Could not find parameter with name {name}")));
		}
		Ok(usize::try_from(index).unwrap_or(0))
	}

	/// Binds `value` to the 1-based parameter `index` with the type
	/// `@duckdb/node-api`'s `typeForValue` gives a JavaScript value: null as
	/// SQLNULL, a boolean as BOOLEAN, an integral number as INTEGER within the
	/// 32-bit range and BIGINT beyond it, any other number as DOUBLE, text as VARCHAR.
	pub fn bind(&mut self, index: usize, value: &Scalar) -> Result<()> {
		let at = index as ffi::idx_t;
		let state = unsafe {
			match value {
				Scalar::Null => ffi::duckdb_bind_null(self.raw, at),
				Scalar::Bool(flag) => ffi::duckdb_bind_boolean(self.raw, at, *flag),
				Scalar::Num(number) if number.is_finite() && number.fract() == 0.0 => {
					if (-2_147_483_648.0..=2_147_483_647.0).contains(number) {
						ffi::duckdb_bind_int32(self.raw, at, *number as i32)
					} else if (-9_223_372_036_854_775_808.0..9_223_372_036_854_775_808.0).contains(number) {
						ffi::duckdb_bind_int64(self.raw, at, *number as i64)
					} else {
						return Err(DuckError::new(format!("parameter {index}: {number} lies outside BIGINT")));
					}
				}
				Scalar::Num(number) => ffi::duckdb_bind_double(self.raw, at, *number),
				Scalar::Str(text) => ffi::duckdb_bind_varchar_length(self.raw, at, text.as_ptr().cast(), text.len() as ffi::idx_t),
			}
		};
		if state != ffi::DuckDBSuccess {
			return Err(DuckError::new(format!("Could not bind parameter {index}")));
		}
		Ok(())
	}

	/// Runs the statement to completion; the number of rows it changed.
	pub fn execute(&self) -> Result<u64> {
		let mut result = Materialized::empty();
		let state = unsafe { ffi::duckdb_execute_prepared(self.raw, &mut result.0) };
		result.check(state)?;
		Ok(unsafe { ffi::duckdb_rows_changed(&mut result.0) })
	}

	/// Runs the statement and yields its rows chunk by chunk as they are produced.
	pub fn stream(&self) -> Result<Stream> {
		let mut result = Materialized::empty();
		let state = unsafe { ffi::duckdb_execute_prepared_streaming(self.raw, &mut result.0) };
		result.check(state)?;
		let result = std::mem::ManuallyDrop::new(result);
		Ok(Stream { raw: result.0, _conn: Arc::clone(&self.conn) })
	}
}

impl Drop for Prepared {
	fn drop(&mut self) {
		unsafe { ffi::duckdb_destroy_prepare(&mut self.raw) };
	}
}

/// A logical type DuckDB handed over; destroyed on drop.
pub struct LogicalType(ffi::duckdb_logical_type);

impl LogicalType {
	pub fn id(&self) -> ffi::DUCKDB_TYPE {
		unsafe { ffi::duckdb_get_type_id(self.0) }
	}

	fn raw(&self) -> ffi::duckdb_logical_type {
		self.0
	}
}

impl Drop for LogicalType {
	fn drop(&mut self) {
		unsafe { ffi::duckdb_destroy_logical_type(&mut self.0) };
	}
}

/// A streaming result.
pub struct Stream {
	raw: ffi::duckdb_result,
	_conn: Arc<Conn>,
}

impl Stream {
	pub fn column_count(&mut self) -> usize {
		usize::try_from(unsafe { ffi::duckdb_column_count(&mut self.raw) }).unwrap_or(0)
	}

	/// Column names as DuckDB reports them, duplicates kept.
	pub fn column_names(&mut self) -> Vec<String> {
		(0..self.column_count())
			.map(|column| unsafe { text_of(ffi::duckdb_column_name(&mut self.raw, column as ffi::idx_t), "") })
			.collect()
	}

	pub fn column_types(&mut self) -> Vec<LogicalType> {
		(0..self.column_count())
			.map(|column| LogicalType(unsafe { ffi::duckdb_column_logical_type(&mut self.raw, column as ffi::idx_t) }))
			.collect()
	}

	/// The next chunk, or None once the result is exhausted.
	pub fn next_chunk(&mut self) -> Result<Option<Chunk>> {
		let chunk = unsafe { ffi::duckdb_fetch_chunk(self.raw) };
		if chunk.is_null() {
			if unsafe { ffi::duckdb_result_error(&mut self.raw) }.is_null() {
				return Ok(None);
			}
			return Err(result_error(&mut self.raw));
		}
		let chunk = Chunk(chunk);
		if chunk.is_empty() {
			return Ok(None);
		}
		Ok(Some(chunk))
	}
}

impl Drop for Stream {
	fn drop(&mut self) {
		unsafe { ffi::duckdb_destroy_result(&mut self.raw) };
	}
}

/// One chunk of result rows.
pub struct Chunk(ffi::duckdb_data_chunk);

impl Chunk {
	pub fn len(&self) -> usize {
		usize::try_from(unsafe { ffi::duckdb_data_chunk_get_size(self.0) }).unwrap_or(0)
	}

	pub fn is_empty(&self) -> bool {
		self.len() == 0
	}

	/// Column `column` of this chunk, readable while the chunk lives.
	pub fn vector(&self, column: usize) -> Column<'_> {
		Column {
			raw: unsafe { ffi::duckdb_data_chunk_get_vector(self.0, column as ffi::idx_t) },
			len: self.len(),
			_chunk: std::marker::PhantomData,
		}
	}
}

/// One column vector of a live chunk, with the chunk's row count.
#[derive(Clone, Copy)]
pub struct Column<'a> {
	raw: ffi::duckdb_vector,
	len: usize,
	_chunk: std::marker::PhantomData<&'a Chunk>,
}

impl Drop for Chunk {
	fn drop(&mut self) {
		unsafe { ffi::duckdb_destroy_data_chunk(&mut self.0) };
	}
}

/// Appends rows to one table; `close` flushes them.
pub struct Appender {
	raw: ffi::duckdb_appender,
	_conn: Arc<Conn>,
}

impl Appender {
	fn error(&self, fallback: &str) -> DuckError {
		DuckError::new(unsafe { text_of(ffi::duckdb_appender_error(self.raw), fallback) })
	}

	fn check(&self, state: ffi::duckdb_state) -> Result<()> {
		if state == ffi::DuckDBSuccess { Ok(()) } else { Err(self.error("the appender failed")) }
	}

	pub fn null(&mut self) -> Result<()> {
		self.check(unsafe { ffi::duckdb_append_null(self.raw) })
	}

	pub fn varchar(&mut self, text: &str) -> Result<()> {
		self.check(unsafe { ffi::duckdb_append_varchar_length(self.raw, text.as_ptr().cast(), text.len() as ffi::idx_t) })
	}

	pub fn int64(&mut self, value: i64) -> Result<()> {
		self.check(unsafe { ffi::duckdb_append_int64(self.raw, value) })
	}

	pub fn double(&mut self, value: f64) -> Result<()> {
		self.check(unsafe { ffi::duckdb_append_double(self.raw, value) })
	}

	pub fn boolean(&mut self, value: bool) -> Result<()> {
		self.check(unsafe { ffi::duckdb_append_bool(self.raw, value) })
	}

	pub fn end_row(&mut self) -> Result<()> {
		self.check(unsafe { ffi::duckdb_appender_end_row(self.raw) })
	}

	/// Appends a whole chunk whose column types match the table's.
	pub fn chunk(&mut self, chunk: &Chunk) -> Result<()> {
		self.check(unsafe { ffi::duckdb_append_data_chunk(self.raw, chunk.0) })
	}

	/// Flushes the appended rows into the table.
	pub fn close(mut self) -> Result<()> {
		let state = unsafe { ffi::duckdb_appender_close(self.raw) };
		let outcome = self.check(state);
		unsafe { ffi::duckdb_appender_destroy(&mut self.raw) };
		self.raw = ptr::null_mut();
		outcome
	}
}

impl Drop for Appender {
	fn drop(&mut self) {
		if !self.raw.is_null() {
			unsafe { ffi::duckdb_appender_destroy(&mut self.raw) };
		}
	}
}

/// A column type as SQL, the way `@duckdb/node-api` writes a type
/// (`String(type)`): an alias is not written, so JSON is VARCHAR.
pub fn sql_type(logical: &LogicalType) -> String {
	let raw = logical.raw();
	let child = |of: ffi::duckdb_logical_type| LogicalType(of);
	let quoted_identifier = |name: &str| format!("\"{}\"", name.replace('"', "\"\""));
	let quoted_string = |text: &str| format!("'{}'", text.replace('\'', "''"));
	match logical.id() {
		ffi::DUCKDB_TYPE_DUCKDB_TYPE_DECIMAL => {
			let (width, scale) = unsafe { (ffi::duckdb_decimal_width(raw), ffi::duckdb_decimal_scale(raw)) };
			format!("DECIMAL({width},{scale})")
		}
		ffi::DUCKDB_TYPE_DUCKDB_TYPE_ENUM => {
			let size = unsafe { ffi::duckdb_enum_dictionary_size(raw) };
			let values: Vec<String> = (0..size)
				.map(|index| quoted_string(&take_owned(unsafe { ffi::duckdb_enum_dictionary_value(raw, ffi::idx_t::from(index)) }).unwrap_or_default()))
				.collect();
			format!("ENUM({})", values.join(", "))
		}
		ffi::DUCKDB_TYPE_DUCKDB_TYPE_LIST => format!("{}[]", sql_type(&child(unsafe { ffi::duckdb_list_type_child_type(raw) }))),
		ffi::DUCKDB_TYPE_DUCKDB_TYPE_ARRAY => {
			let size = unsafe { ffi::duckdb_array_type_array_size(raw) };
			format!("{}[{size}]", sql_type(&child(unsafe { ffi::duckdb_array_type_child_type(raw) })))
		}
		ffi::DUCKDB_TYPE_DUCKDB_TYPE_MAP => format!(
			"MAP({}, {})",
			sql_type(&child(unsafe { ffi::duckdb_map_type_key_type(raw) })),
			sql_type(&child(unsafe { ffi::duckdb_map_type_value_type(raw) }))
		),
		ffi::DUCKDB_TYPE_DUCKDB_TYPE_STRUCT => {
			let count = unsafe { ffi::duckdb_struct_type_child_count(raw) };
			if count == 0 {
				return "STRUCT".to_owned();
			}
			let entries: Vec<(String, String)> = (0..count)
				.map(|index| {
					let name = take_owned(unsafe { ffi::duckdb_struct_type_child_name(raw, index) }).unwrap_or_default();
					(name, sql_type(&child(unsafe { ffi::duckdb_struct_type_child_type(raw, index) })))
				})
				.collect();
			if entries.iter().all(|(name, _)| name.is_empty()) {
				let types: Vec<&str> = entries.iter().map(|(_, sql)| sql.as_str()).collect();
				return format!("TUPLE({})", types.join(", "));
			}
			let parts: Vec<String> = entries.iter().map(|(name, sql)| format!("{} {sql}", quoted_identifier(name))).collect();
			format!("STRUCT({})", parts.join(", "))
		}
		ffi::DUCKDB_TYPE_DUCKDB_TYPE_UNION => {
			let count = unsafe { ffi::duckdb_union_type_member_count(raw) };
			let parts: Vec<String> = (0..count)
				.map(|index| {
					let name = take_owned(unsafe { ffi::duckdb_union_type_member_name(raw, index) }).unwrap_or_default();
					format!("{} {}", quoted_identifier(&name), sql_type(&child(unsafe { ffi::duckdb_union_type_member_type(raw, index) })))
				})
				.collect();
			format!("UNION({})", parts.join(", "))
		}
		ffi::DUCKDB_TYPE_DUCKDB_TYPE_TIME_TZ => "TIME WITH TIME ZONE".to_owned(),
		ffi::DUCKDB_TYPE_DUCKDB_TYPE_TIMESTAMP_TZ => "TIMESTAMP WITH TIME ZONE".to_owned(),
		ffi::DUCKDB_TYPE_DUCKDB_TYPE_TIMESTAMP_TZ_NS => "TIMESTAMPTZ_NS".to_owned(),
		ffi::DUCKDB_TYPE_DUCKDB_TYPE_SQLNULL => "\"NULL\"".to_owned(),
		id => convert::type_name(id).to_owned(),
	}
}
