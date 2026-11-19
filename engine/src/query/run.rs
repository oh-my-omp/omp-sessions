//! Running caller SQL on the query process's database, and exporting a query
//! result. Statements are parsed by DuckDB and admitted by statement type:
//! queries and changes to the database's own tables, macros and views.
//! Statements that configure the engine or reach files (ATTACH, INSTALL, LOAD,
//! SET, PRAGMA, CALL, …) are refused. A COPY or EXPORT naming a file fails
//! earlier, while DuckDB prepares it: the locked file-system access rejects the
//! path. Export writes its file from a separate database that runs no caller SQL.

use std::path::{Component, Path, PathBuf};

use super::{Cancel, Database, failed};
use crate::duck::convert::{Reader, value_chars};
use crate::duck::{self, Duck, Prepared, Scalar, StatementType};
use crate::error::{EngineError, ErrorKind, Result};
use crate::json::{Obj, Value};
use crate::protocol::{ExportFormat, Reply};

const ADMITTED: [StatementType; 10] = [
	StatementType::SELECT,
	StatementType::INSERT,
	StatementType::UPDATE,
	StatementType::DELETE,
	StatementType::MERGE_INTO,
	StatementType::CREATE,
	StatementType::CREATE_FUNC,
	StatementType::ALTER,
	StatementType::DROP,
	StatementType::EXPLAIN,
];

/// Characters one `sql` result may spend: `limit` bounds the row count, the
/// budget bounds what those rows hold, so a single huge cell (one transcript
/// line can be 200 MB) never reaches the caller. Counted per value
/// ([`value_chars`]) without serializing a row.
const RESULT_CHAR_BUDGET: usize = 16 * 1024 * 1024;

/// The bindings of a statement's parameters.
pub enum Bindings {
	Positional(Vec<Scalar>),
	Named(Vec<(String, Scalar)>),
}

impl Bindings {
	/// `params` of a request: an array of scalars, or an object of them by name.
	pub fn parse(params: Option<&Value>) -> Result<Option<Bindings>> {
		let scalar = |value: &Value| match value {
			Value::Null => Ok(Scalar::Null),
			Value::Bool(flag) => Ok(Scalar::Bool(*flag)),
			Value::Num(number) => Ok(Scalar::Num(*number)),
			Value::Str(text) => Ok(Scalar::Str(text.clone())),
			_ => Err(EngineError::bad_request("params: each binding is a string, number, boolean or null")),
		};
		match params {
			None => Ok(None),
			Some(Value::Arr(items)) => Ok(Some(Bindings::Positional(items.iter().map(scalar).collect::<Result<_>>()?))),
			Some(Value::Obj(obj)) => Ok(Some(Bindings::Named(
				obj.iter().map(|(name, value)| Ok((name.to_owned(), scalar(value)?))).collect::<Result<_>>()?,
			))),
			Some(_) => Err(EngineError::bad_request("params: an array of bindings or an object of them by name")),
		}
	}

	fn bind(&self, prepared: &mut Prepared) -> duck::Result<()> {
		match self {
			Bindings::Positional(values) => {
				for (index, value) in values.iter().enumerate() {
					prepared.bind(index + 1, value)?;
				}
			}
			Bindings::Named(values) => {
				for (name, value) in values {
					let index = prepared.parameter_index(name)?;
					prepared.bind(index, value)?;
				}
			}
		}
		Ok(())
	}
}

/// `deduplicatedColumnNames`: a repeated name gets `:1`, `:2`, … after its first use.
fn deduplicated(names: &[String]) -> Vec<String> {
	let mut seen: Vec<(&str, usize)> = Vec::new();
	names
		.iter()
		.map(|name| match seen.iter_mut().find(|(known, _)| known == name) {
			Some(slot) => {
				slot.1 += 1;
				format!("{name}:{}", slot.1 - 1)
			}
			None => {
				seen.push((name, 1));
				name.clone()
			}
		})
		.collect()
}

/// Runs every statement of `query` in order and returns the last one's
/// result: at most `limit` rows when it returns rows, within the character
/// budget, else the number of rows it changed. `truncated` reports that a row
/// was not returned. `params` bind the only statement; a script with
/// parameters is refused.
pub fn run_sql(duck: &Duck, query: &str, params: Option<&Bindings>, limit: usize, cancel: &Cancel) -> Result<Reply> {
	let extracted = duck.extract(query).map_err(|error| failed(error, cancel))?;
	let total = extracted.total();
	if params.is_some() && total > 1 {
		return Err(EngineError::bad_request("params bind a single statement; this query holds several"));
	}
	for (index, prepared) in extracted.enumerate() {
		let mut prepared = prepared.map_err(|error| failed(error, cancel))?;
		let kind = prepared.statement_type();
		if !ADMITTED.contains(&kind) {
			return Err(EngineError::new(
				ErrorKind::Refused,
				format!(
					"statement {} is {}; session_query runs queries and changes to its own tables, views and macros only (use action \"export\" to write a file)",
					index + 1,
					kind.name()
				),
			));
		}
		if let Some(params) = params {
			params.bind(&mut prepared).map_err(|error| failed(error, cancel))?;
		}
		if index + 1 < total {
			prepared.execute().map_err(|error| failed(error, cancel))?;
			continue;
		}
		if prepared.column_count() == 0 {
			let changes = prepared.execute().map_err(|error| failed(error, cancel))?;
			return Ok(Reply::Changes { changes });
		}
		// Chunks are read one at a time and converted one row at a time, so a
		// result is never materialized whole.
		let mut stream = prepared.stream().map_err(|error| failed(error, cancel))?;
		let columns = stream.column_names();
		let names = deduplicated(&columns);
		let readers: Vec<Reader> = stream.column_types().iter().map(Reader::of).collect();
		let mut rows = Vec::new();
		let mut chars = 0;
		let mut truncated = false;
		while !truncated && let Some(chunk) = stream.next_chunk().map_err(|error| failed(error, cancel))? {
			let vectors: Vec<_> = (0..readers.len()).map(|column| chunk.vector(column)).collect();
			for row in 0..chunk.len() {
				if rows.len() == limit {
					truncated = true;
					break;
				}
				let mut object = Obj::new();
				let mut row_chars = 0;
				for ((reader, vector), name) in readers.iter().zip(&vectors).zip(&names) {
					let value = reader.read(*vector, row).map_err(|error| failed(error, cancel))?;
					row_chars += value_chars(&value);
					object.insert(name.clone(), value);
				}
				if chars + row_chars > RESULT_CHAR_BUDGET {
					truncated = true;
					break;
				}
				rows.push(Value::Obj(object));
				chars += row_chars;
			}
		}
		return Ok(Reply::SqlRows { columns, rows, truncated });
	}
	Err(EngineError::bad_request("query holds no statement"))
}

/// `path.resolve` of an absolute path: `.` and `..` removed lexically.
fn resolve(target: &Path) -> PathBuf {
	let mut out = PathBuf::new();
	for component in target.components() {
		match component {
			Component::ParentDir => {
				out.pop();
			}
			Component::CurDir => {}
			other => out.push(other),
		}
	}
	out
}

/// An absolute export path outside the sessions directory, whose directory
/// exists. Both sides are compared as real paths, so another spelling of the
/// sessions directory (case, 8.3 short names, symbolic links) still lies inside it.
fn export_path(target: &str, sessions_dir: &str) -> Result<PathBuf> {
	let path = Path::new(target);
	if !path.is_absolute() {
		return Err(EngineError::bad_request(format!("export path {target} is not absolute")));
	}
	let resolved = resolve(path);
	let directory = resolved.parent().map(Path::to_path_buf).unwrap_or_else(|| resolved.clone());
	if !directory.is_dir() {
		return Err(EngineError::bad_request(format!("export directory {} does not exist", directory.display())));
	}
	let real = std::fs::canonicalize(&resolved).unwrap_or_else(|_| {
		let base = std::fs::canonicalize(&directory).unwrap_or_else(|_| directory.clone());
		match resolved.file_name() {
			Some(name) => base.join(name),
			None => base,
		}
	});
	let sessions = std::fs::canonicalize(sessions_dir).unwrap_or_else(|_| resolve(Path::new(sessions_dir)));
	if real.starts_with(&sessions) {
		return Err(EngineError::new(ErrorKind::Refused, "export path lies inside the omp sessions directory"));
	}
	Ok(resolved)
}

/// Runs `query` (one SELECT) and writes its whole result to `target` as `format`.
pub fn export(database: &Database, query: &str, params: Option<&Bindings>, target: &str, format: ExportFormat, cancel: &Cancel) -> Result<Reply> {
	let file = export_path(target, &database.sessions_dir)?;
	let mut extracted = database.duck.extract(query).map_err(|error| failed(error, cancel))?;
	if extracted.total() != 1 {
		return Err(EngineError::bad_request("export takes one statement that returns rows"));
	}
	let Some(prepared) = extracted.next() else { return Err(EngineError::bad_request("export takes one statement that returns rows")) };
	let mut prepared = prepared.map_err(|error| failed(error, cancel))?;
	let kind = prepared.statement_type();
	if kind != StatementType::SELECT {
		return Err(EngineError::new(ErrorKind::Refused, format!("export takes a SELECT, not {}", kind.name())));
	}
	if let Some(params) = params {
		params.bind(&mut prepared).map_err(|error| failed(error, cancel))?;
	}
	let mut stream = prepared.stream().map_err(|error| failed(error, cancel))?;
	let columns = stream.column_names();
	let types: Vec<String> = stream.column_types().iter().map(duck::sql_type).collect();
	// The writer runs no caller SQL, so it keeps DuckDB's file access; it spills
	// into the query process's own spill directory.
	let writer = Duck::open().map_err(|error| failed(error, cancel))?;
	let spill = database.spill.path().to_string_lossy();
	writer
		.run(&format!("SET memory_limit = '4GB'; SET temp_directory = {}", super::sources::literal(&spill)))
		.map_err(|error| failed(error, cancel))?;
	let defs: Vec<String> =
		columns.iter().zip(&types).map(|(name, sql)| format!("{} {sql}", super::sources::identifier(name))).collect();
	writer.run(&format!("CREATE TABLE result ({})", defs.join(", "))).map_err(|error| failed(error, cancel))?;
	let mut appender = writer.appender("main", "result").map_err(|error| failed(error, cancel))?;
	let mut rows = 0u64;
	while let Some(chunk) = stream.next_chunk().map_err(|error| failed(error, cancel))? {
		appender.chunk(&chunk).map_err(|error| failed(error, cancel))?;
		rows += chunk.len() as u64;
	}
	appender.close().map_err(|error| failed(error, cancel))?;
	let path = file.to_string_lossy().into_owned();
	writer
		.run(&format!("COPY result TO {} (FORMAT {})", super::sources::literal(&path), format.name()))
		.map_err(|error| failed(error, cancel))?;
	Ok(Reply::Exported { path, format, rows, columns })
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn repeated_column_names_are_numbered() {
		let names: Vec<String> = ["a", "b", "a", "a"].iter().map(|name| (*name).to_owned()).collect();
		assert_eq!(deduplicated(&names), ["a", "b", "a:1", "a:2"]);
	}

	#[test]
	fn export_paths_resolve_dots() {
		assert_eq!(resolve(Path::new("/a/b/../c/./d")), PathBuf::from("/a/c/d"));
	}
}
