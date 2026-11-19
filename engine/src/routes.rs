//! session:// URL grammar → Route, and resolution of routes to session files.
//! One traversable tree (projects/<bucket>/<date>/<sid>/…) so grep never
//! visits a session twice; `session://<id-prefix>/…`, `here/…` and detail
//! leaves resolve directly but are not listed. The routes removed by decision
//! §8 parse to a `bad_request` naming their replacement.

use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::error::{EngineError, ErrorKind, Result};
use crate::events::EntrySelector;
use crate::protocol::Context;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BucketRef {
	Named(String),
	/// The caller's project: `Context::here_dir`.
	Here,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Locator {
	Prefix(String),
	Listed { bucket: BucketRef, date: String, sid: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionPath {
	pub locator: Locator,
	/// Full subagent id, or "" for the root session.
	pub agent: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Route {
	Root,
	Projects,
	Bucket(BucketRef),
	Date { bucket: BucketRef, date: String },
	SessionDir(SessionPath),
	AgentsDir(SessionPath),
	Timeline(SessionPath),
	Events(SessionPath),
	Raw(SessionPath),
	Entry { at: SessionPath, selector: EntrySelector },
	Artifact { at: SessionPath, id: String },
	Local { at: SessionPath, relative: String },
}

/// A parsed URL: its route plus the `type` filter written as a `type/<filter>/` path prefix.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
	pub route: Route,
	pub path_types: Vec<String>,
}

/// A located root session file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionRef {
	pub sid: String,
	pub file: PathBuf,
	/// The bucket directory's name.
	pub bucket: String,
}

/// A root session file in a bucket: its id, its creation time and its path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BucketSession {
	pub sid: String,
	pub created_ms: i64,
	pub file: PathBuf,
}

/// The pointer every grammar error ends with.
pub const GRAMMAR: &str = "The grammar is in xd://read/urls.";
/// Where listing and searching sessions moved (decision §8), as a sentence and as a clause.
pub const ARCHIVE_HINT: &str = "List and search sessions with omp's archive eval global (archive.sessions, archive.search).";
const ARCHIVE: &str = "list and search sessions with omp's archive eval global (archive.sessions, archive.search).";

fn invalid(url: &str, why: &str) -> EngineError {
	EngineError::bad_request(format!("Invalid session:// URL {url}: {why}. {GRAMMAR}"))
}

fn removed(route: &str) -> EngineError {
	EngineError::bad_request(format!("{route} was removed: {ARCHIVE}"))
}

/// `^[\w-]+$` (ASCII word characters and `-`).
fn is_id_prefix(text: &str) -> bool {
	!text.is_empty() && text.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// `^[A-Za-z0-9_.-]+$`.
fn is_agent_id(text: &str) -> bool {
	!text.is_empty() && text.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'.' || b == b'-')
}

fn is_digits(text: &[u8]) -> bool {
	text.iter().all(u8::is_ascii_digit)
}

/// `^\d{4}-\d{2}-\d{2}$`.
fn is_date(text: &str) -> bool {
	let b = text.as_bytes();
	b.len() == 10 && is_digits(&b[0..4]) && b[4] == b'-' && is_digits(&b[5..7]) && b[7] == b'-' && is_digits(&b[8..10])
}

fn segment(name: &str, url: &str) -> Result<String> {
	if name.is_empty() || name == "." || name == ".." || name.contains('/') || name.contains('\\') {
		return Err(invalid(url, &format!("bad path segment \"{name}\"")));
	}
	Ok(name.to_owned())
}

/// JavaScript white space and line terminators, as `Number()` and `trim` strip them.
fn is_js_space(c: char) -> bool {
	c.is_whitespace() || c == '\u{FEFF}'
}

/// JavaScript `Number(text)` for a string.
fn js_number(text: &str) -> f64 {
	let t = text.trim_matches(is_js_space);
	if t.is_empty() {
		return 0.0;
	}
	let radix = |digits: &str, radix: u32| -> f64 {
		if digits.is_empty() || !digits.chars().all(|c| c.is_digit(radix)) {
			return f64::NAN;
		}
		digits.chars().fold(0.0, |acc, c| acc * f64::from(radix) + f64::from(c.to_digit(radix).unwrap_or(0)))
	};
	let lower = t.get(..2).map(str::to_ascii_lowercase);
	match lower.as_deref() {
		Some("0x") => return radix(&t[2..], 16),
		Some("0o") => return radix(&t[2..], 8),
		Some("0b") => return radix(&t[2..], 2),
		_ => {}
	}
	let unsigned = t.strip_prefix(['+', '-']).unwrap_or(t);
	if unsigned == "Infinity" {
		return if t.starts_with('-') { f64::NEG_INFINITY } else { f64::INFINITY };
	}
	// StrDecimalLiteral: digits with an optional fraction and exponent; Rust's
	// float parser also takes `inf`/`nan` words, which JavaScript does not.
	let bytes = unsigned.as_bytes();
	let mut i = 0;
	let int_start = i;
	while i < bytes.len() && bytes[i].is_ascii_digit() {
		i += 1;
	}
	let mut digits = i - int_start;
	if i < bytes.len() && bytes[i] == b'.' {
		i += 1;
		let frac_start = i;
		while i < bytes.len() && bytes[i].is_ascii_digit() {
			i += 1;
		}
		digits += i - frac_start;
	}
	if digits == 0 {
		return f64::NAN;
	}
	if i < bytes.len() && (bytes[i] == b'e' || bytes[i] == b'E') {
		i += 1;
		if i < bytes.len() && (bytes[i] == b'+' || bytes[i] == b'-') {
			i += 1;
		}
		let exp_start = i;
		while i < bytes.len() && bytes[i].is_ascii_digit() {
			i += 1;
		}
		if i == exp_start {
			return f64::NAN;
		}
	}
	if i != bytes.len() {
		return f64::NAN;
	}
	t.parse::<f64>().unwrap_or(f64::NAN)
}

fn session_route(locator: Locator, rest: &[String], agent: String, url: &str) -> Result<Route> {
	let at = SessionPath { locator, agent };
	let Some((head, tail)) = rest.split_first() else { return Ok(Route::SessionDir(at)) };
	match head.as_str() {
		"timeline" if tail.is_empty() => return Ok(Route::Timeline(at)),
		"events.jsonl" if tail.is_empty() => return Ok(Route::Events(at)),
		"raw.jsonl" if tail.is_empty() => return Ok(Route::Raw(at)),
		"entry" if tail.len() == 1 => {
			let id = &tail[0];
			if let Some(line) = id.strip_prefix('@') {
				let line = js_number(line);
				if line.fract() != 0.0 || !line.is_finite() || line < 1.0 {
					return Err(invalid(url, &format!("bad line \"{id}\"")));
				}
				// An integral f64 ≥ 1; lines past u64 saturate and match nothing.
				#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
				return Ok(Route::Entry { at, selector: EntrySelector::Line(line as u64) });
			}
			return Ok(Route::Entry { at, selector: EntrySelector::Id(id.clone()) });
		}
		"artifacts" if tail.len() == 1 && !tail[0].is_empty() && is_digits(tail[0].as_bytes()) => {
			return Ok(Route::Artifact { at, id: tail[0].clone() });
		}
		"local" if !tail.is_empty() => {
			let parts = tail.iter().map(|part| segment(part, url)).collect::<Result<Vec<_>>>()?;
			return Ok(Route::Local { at, relative: parts.join("/") });
		}
		"agents" => {
			let Some((id, more)) = tail.split_first() else { return Ok(Route::AgentsDir(at)) };
			if !is_agent_id(id) {
				return Err(invalid(url, &format!("bad agent id \"{id}\"")));
			}
			return session_route(at.locator, more, id.clone(), url);
		}
		_ => {}
	}
	Err(invalid(url, &format!("unknown path /{}", rest.join("/"))))
}

fn bucket_route(bucket: BucketRef, rest: &[String], url: &str) -> Result<Route> {
	let Some((date, more)) = rest.split_first() else { return Ok(Route::Bucket(bucket)) };
	if date == "sessions.jsonl" && more.is_empty() {
		return Err(removed(&match &bucket {
			BucketRef::Here => "session://here/sessions.jsonl".to_owned(),
			BucketRef::Named(name) => format!("session://projects/{name}/sessions.jsonl"),
		}));
	}
	if !is_date(date) {
		return Err(invalid(url, &format!("expected a YYYY-MM-DD date, got \"{date}\"")));
	}
	let Some((sid, more)) = more.split_first() else { return Ok(Route::Date { bucket, date: date.clone() }) };
	if !is_id_prefix(sid) {
		return Err(invalid(url, &format!("bad session id \"{sid}\"")));
	}
	session_route(Locator::Listed { bucket, date: date.clone(), sid: sid.clone() }, more, String::new(), url)
}

fn route(host: &str, rest: &[String], url: &str) -> Result<Route> {
	match host {
		"" => {
			if !rest.is_empty() {
				return Err(invalid(url, "the root has no path"));
			}
			Ok(Route::Root)
		}
		"README" => Err(EngineError::bad_request("session://README was removed: the session:// grammar is in xd://read/urls.")),
		"sessions.jsonl" => Err(removed("session://sessions.jsonl")),
		"find" => Err(removed("session://find")),
		"here" => bucket_route(BucketRef::Here, rest, url),
		"projects" => {
			let Some((bucket, more)) = rest.split_first() else { return Ok(Route::Projects) };
			bucket_route(BucketRef::Named(segment(bucket, url)?), more, url)
		}
		_ => {
			if !is_id_prefix(host) {
				return Err(invalid(url, &format!("\"{host}\" is not a session id prefix")));
			}
			session_route(Locator::Prefix(host.to_owned()), rest, String::new(), url)
		}
	}
}

fn hex(b: u8) -> Option<u8> {
	match b {
		b'0'..=b'9' => Some(b - b'0'),
		b'a'..=b'f' => Some(b - b'a' + 10),
		b'A'..=b'F' => Some(b - b'A' + 10),
		_ => None,
	}
}

/// `decodeURIComponent`: None where JavaScript throws `URIError` (a bad
/// escape, or escapes that do not form UTF-8).
pub(crate) fn decode_uri_component(text: &str) -> Option<String> {
	if !text.contains('%') {
		return Some(text.to_owned());
	}
	let bytes = text.as_bytes();
	let mut out = Vec::with_capacity(bytes.len());
	let mut i = 0;
	while i < bytes.len() {
		if bytes[i] == b'%' {
			let high = hex(*bytes.get(i + 1)?)?;
			let low = hex(*bytes.get(i + 2)?)?;
			out.push(high << 4 | low);
			i += 3;
		} else {
			out.push(bytes[i]);
			i += 1;
		}
	}
	String::from_utf8(out).ok()
}

/// Parses a session:// URL's host and path (`url` is the whole href, for messages).
pub fn parse_target(host: &str, pathname: &str, url: &str) -> Result<Target> {
	let rest = pathname
		.split('/')
		.filter(|part| !part.is_empty())
		.map(|part| decode_uri_component(part).ok_or_else(|| invalid(url, &format!("bad percent-encoding in path segment \"{part}\""))))
		.collect::<Result<Vec<_>>>()?;
	if host != "type" {
		return Ok(Target { route: route(host, &rest, url)?, path_types: Vec::new() });
	}
	match rest.as_slice() {
		[filter, inner_host, inner @ ..] => Ok(Target { route: route(inner_host, inner, url)?, path_types: vec![filter.clone()] }),
		_ => Err(invalid(url, "use session://type/<filter>/<path>, e.g. session://type/thinking/here/")),
	}
}

/// The directory of a bucket: `sessions_dir/<name>`, or `here_dir`.
pub fn bucket_dir(bucket: &BucketRef, context: &Context) -> PathBuf {
	match bucket {
		BucketRef::Named(name) => Path::new(&context.sessions_dir).join(name),
		BucketRef::Here => PathBuf::from(&context.here_dir),
	}
}

fn base_name(path: &Path) -> String {
	path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default()
}

/// A parsed `<YYYY-MM-DD>T<HH>-<MM>-<SS>-<mmm>Z_<id>.jsonl` file name: the id and creation time.
pub fn file_stamp(name: &str) -> Option<(String, i64)> {
	let b = name.as_bytes();
	let stem = name.strip_suffix(".jsonl")?;
	if b.len() < 26 || !is_date(&name[..10]) {
		return None;
	}
	let fields = [(10, b'T'), (13, b'-'), (16, b'-'), (19, b'-'), (23, b'Z'), (24, b'_')];
	if fields.iter().any(|&(at, want)| b[at] != want) {
		return None;
	}
	if !is_digits(&b[11..13]) || !is_digits(&b[14..16]) || !is_digits(&b[17..19]) || !is_digits(&b[20..23]) {
		return None;
	}
	// `(.+)` before `\.jsonl$`: a non-empty id without line terminators.
	let sid = stem.get(25..)?;
	if sid.is_empty() || sid.contains(['\n', '\r', '\u{2028}', '\u{2029}']) {
		return None;
	}
	let iso = format!("{}T{}:{}:{}.{}Z", &name[..10], &name[11..13], &name[14..16], &name[17..19], &name[20..23]);
	crate::time::parse_ms(&iso).map(|created| (sid.to_owned(), created))
}

/// JavaScript's default sort order: UTF-16 code units.
fn js_cmp(a: &str, b: &str) -> std::cmp::Ordering {
	a.encode_utf16().cmp(b.encode_utf16())
}

fn listing_error(dir: &Path, error: &io::Error) -> EngineError {
	match error.kind() {
		io::ErrorKind::NotFound | io::ErrorKind::NotADirectory => EngineError::new(ErrorKind::NotFound, format!("{}: {error}", dir.display())),
		_ => EngineError::io(dir, error),
	}
}

/// Entry names of a directory in `read_dir` order, with their paths.
fn dir_names(dir: &Path) -> io::Result<Vec<(String, PathBuf)>> {
	let mut names = Vec::new();
	for entry in fs::read_dir(dir)? {
		let entry = entry?;
		names.push((entry.file_name().to_string_lossy().into_owned(), entry.path()));
	}
	Ok(names)
}

/// Session files directly in a bucket: stamped names first, then header-named
/// files, each kind in file-name order.
pub fn bucket_sessions(dir: &Path) -> Result<Vec<BucketSession>> {
	bucket_sessions_io(dir).map_err(|error| listing_error(dir, &error))
}

fn bucket_sessions_io(dir: &Path) -> io::Result<Vec<BucketSession>> {
	let mut names = dir_names(dir)?;
	names.sort_by(|a, b| js_cmp(&a.0, &b.0));
	let mut stamped = Vec::new();
	let mut other = Vec::new();
	for (name, file) in names {
		if let Some((sid, created_ms)) = file_stamp(&name) {
			stamped.push(BucketSession { sid, created_ms, file });
			continue;
		}
		if !name.ends_with(".jsonl") {
			continue;
		}
		if let Some(info) = crate::info::read_session_info(&file, false)
			&& let Some(created_ms) = info.created_ms
		{
			other.push(BucketSession { sid: info.id, created_ms, file });
		}
	}
	stamped.extend(other);
	Ok(stamped)
}

/// A bucket that cannot be listed (gone, not a directory, not readable) holds no sessions during a scan.
fn unavailable(error: &io::Error) -> bool {
	matches!(error.kind(), io::ErrorKind::NotFound | io::ErrorKind::NotADirectory | io::ErrorKind::PermissionDenied)
}

/// Project bucket directories under the sessions root (symlinked buckets are not followed); none when the root cannot be listed.
fn bucket_dirs(root: &Path) -> Vec<PathBuf> {
	let Ok(entries) = fs::read_dir(root) else { return Vec::new() };
	entries
		.filter_map(|entry| entry.ok())
		.filter(|entry| entry.file_type().is_ok_and(|t| t.is_dir()))
		.map(|entry| entry.path())
		.collect()
}

/// The id of a `<stamp>_<id>.jsonl` file name (omp `sessionIdFromSessionPath`).
fn id_from_name(name: &str) -> Option<&str> {
	let stem = name.strip_suffix(".jsonl")?;
	let sep = stem.rfind('_').filter(|&sep| sep > 0)?;
	Some(&stem[sep + 1..]).filter(|id| !id.is_empty())
}

/// omp `findSessionFiles`: session files under every real bucket whose file-name id starts with `prefix`, each physical file once.
fn find_session_files(root: &Path, prefix: &str) -> Result<Vec<PathBuf>> {
	let entries = match fs::read_dir(root) {
		Ok(entries) => entries,
		Err(error) if matches!(error.kind(), io::ErrorKind::NotFound | io::ErrorKind::NotADirectory) => return Ok(Vec::new()),
		Err(error) => return Err(EngineError::io(root, &error)),
	};
	let mut files = Vec::new();
	for entry in entries {
		let entry = entry.map_err(|error| EngineError::io(root, &error))?;
		if !entry.file_type().is_ok_and(|t| t.is_dir()) {
			continue;
		}
		let dir = entry.path();
		match dir_names(&dir) {
			Ok(names) => {
				for (name, file) in names {
					// `*_<prefix>*.jsonl` without dotfiles, files only (symlinks followed), then the id-prefix filter.
					if name.starts_with('.') || !id_from_name(&name).is_some_and(|id| id.starts_with(prefix)) {
						continue;
					}
					if fs::metadata(&file).is_ok_and(|m| m.is_file()) {
						files.push(file);
					}
				}
			}
			Err(error) if unavailable(&error) => {}
			Err(error) => return Err(EngineError::io(&dir, &error)),
		}
	}
	let mut seen = HashSet::new();
	files.retain(|file| {
		let key = fs::canonicalize(file).unwrap_or_else(|_| std::path::absolute(file).unwrap_or_else(|_| file.clone()));
		seen.insert(key)
	});
	Ok(files)
}

/// Root sessions whose id starts with `prefix`: stamped files by name, then
/// header-named files of every bucket by their header id, leaving out ids a
/// stamped match already holds.
fn prefix_matches(prefix: &str, context: &Context) -> Result<Vec<SessionRef>> {
	if !is_id_prefix(prefix) {
		return Ok(Vec::new());
	}
	let root = Path::new(&context.sessions_dir);
	let mut refs: Vec<SessionRef> = find_session_files(root, prefix)?
		.into_iter()
		.filter_map(|file| {
			let (sid, _) = file_stamp(&base_name(&file))?;
			let bucket = file.parent().map(base_name).unwrap_or_default();
			Some(SessionRef { sid, file, bucket })
		})
		.collect();
	let stamped: HashSet<String> = refs.iter().map(|r| r.sid.clone()).collect();
	for dir in bucket_dirs(root) {
		let sessions = match bucket_sessions_io(&dir) {
			Ok(sessions) => sessions,
			Err(error) if unavailable(&error) => continue,
			Err(error) => return Err(EngineError::io(&dir, &error)),
		};
		for session in sessions {
			if file_stamp(&base_name(&session.file)).is_some() {
				continue;
			}
			if session.sid.starts_with(prefix) && !stamped.contains(&session.sid) {
				refs.push(SessionRef { sid: session.sid, file: session.file, bucket: base_name(&dir) });
			}
		}
	}
	Ok(refs)
}

/// The root session a locator names; `not_found` on no match, `ambiguous` on several.
pub fn locate_session(locator: &Locator, context: &Context) -> Result<SessionRef> {
	match locator {
		Locator::Listed { bucket, date, sid } => {
			let dir = bucket_dir(bucket, context);
			let found = bucket_sessions(&dir)?
				.into_iter()
				.find(|session| &session.sid == sid && crate::time::local_date(session.created_ms).as_deref() == Some(date.as_str()));
			match found {
				Some(session) => Ok(SessionRef { sid: session.sid, file: session.file, bucket: base_name(&dir) }),
				None => Err(EngineError::not_found(format!("No session {sid} dated {date} in {}", base_name(&dir)))),
			}
		}
		Locator::Prefix(prefix) => {
			let mut refs = prefix_matches(prefix, context)?;
			match refs.len() {
				0 => Err(EngineError::not_found(format!("No session id starts with \"{prefix}\". {ARCHIVE_HINT}"))),
				1 => Ok(refs.remove(0)),
				count => {
					let shown: Vec<String> = refs.iter().take(8).map(|r| format!("{} ({})", r.sid, r.bucket)).collect();
					Err(EngineError::new(
						ErrorKind::Ambiguous,
						format!("\"{prefix}\" matches {count} sessions: {}; use a longer prefix.", shown.join(", ")),
					))
				}
			}
		}
	}
}

/// Directory holding a transcript's own children and artifacts: the file path without `.jsonl`.
pub fn child_dir(file: &Path) -> PathBuf {
	file.with_extension("")
}

/// The transcript of subagent `agent` (full id `A.B.C`) under a root session.
pub fn agent_file(root: &SessionRef, agent: &str) -> Result<PathBuf> {
	let parts: Vec<&str> = agent.split('.').collect();
	let mut dir = child_dir(&root.file);
	for index in 1..parts.len() {
		dir.push(parts[..index].join("."));
	}
	let name = format!("{agent}.jsonl");
	let direct = dir.join(&name);
	if fs::metadata(&direct).is_ok_and(|m| m.is_file()) {
		return Ok(direct);
	}
	search_file(&child_dir(&root.file), &name, 0).ok_or_else(|| {
		EngineError::not_found(format!("No subagent {agent} in session {sid}. List them with session://{sid}/agents/", sid = root.sid))
	})
}

/// A file named `name` at most 8 directory levels below `dir` (symlinks not followed).
fn search_file(dir: &Path, name: &str, depth: u32) -> Option<PathBuf> {
	if depth > 8 {
		return None;
	}
	let entries: Vec<fs::DirEntry> = fs::read_dir(dir).ok()?.filter_map(|entry| entry.ok()).collect();
	if entries.iter().any(|entry| entry.file_type().is_ok_and(|t| t.is_file()) && entry.file_name() == name) {
		return Some(dir.join(name));
	}
	entries
		.iter()
		.filter(|entry| entry.file_type().is_ok_and(|t| t.is_dir()))
		.find_map(|entry| search_file(&entry.path(), name, depth + 1))
}

/// Subagent ids directly under a transcript.
pub fn child_agents(transcript: &Path) -> Vec<String> {
	let Ok(names) = dir_names(&child_dir(transcript)) else { return Vec::new() };
	names
		.into_iter()
		.filter_map(|(name, _)| name.strip_suffix(".jsonl").filter(|_| !name.starts_with("__advisor")).map(str::to_owned))
		.collect()
}

#[cfg(test)]
mod tests {
	use super::*;

	fn parse(url: &str) -> Result<Target> {
		let rest = url.strip_prefix("session://").unwrap();
		let (host, path) = rest.split_once('/').map_or((rest, ""), |(h, p)| (h, p));
		parse_target(host, &format!("/{path}"), url)
	}

	fn route_of(url: &str) -> Route {
		parse(url).unwrap().route
	}

	fn message(url: &str) -> (ErrorKind, String) {
		let error = parse(url).unwrap_err();
		(error.kind, error.message)
	}

	fn prefix(at: &str) -> SessionPath {
		SessionPath { locator: Locator::Prefix(at.to_owned()), agent: String::new() }
	}

	#[test]
	fn session_routes() {
		assert_eq!(route_of("session://"), Route::Root);
		assert_eq!(route_of("session://projects"), Route::Projects);
		assert_eq!(route_of("session://here/"), Route::Bucket(BucketRef::Here));
		assert_eq!(route_of("session://01ab/timeline"), Route::Timeline(prefix("01ab")));
		assert_eq!(route_of("session://01ab/entry/@12"), Route::Entry { at: prefix("01ab"), selector: EntrySelector::Line(12) });
		assert_eq!(route_of("session://01ab/entry/%40%2012"), Route::Entry { at: prefix("01ab"), selector: EntrySelector::Line(12) });
		assert_eq!(route_of("session://01ab/artifacts/3"), Route::Artifact { at: prefix("01ab"), id: "3".into() });
		assert_eq!(route_of("session://01ab/local/a/b.md"), Route::Local { at: prefix("01ab"), relative: "a/b.md".into() });
		assert_eq!(
			route_of("session://projects/-w/2026-10-28/01ab/agents/A.B/events.jsonl"),
			Route::Events(SessionPath {
				locator: Locator::Listed { bucket: BucketRef::Named("-w".into()), date: "2026-10-28".into(), sid: "01ab".into() },
				agent: "A.B".into(),
			})
		);
		let typed = parse("session://type/thinking,irc/here/2026-10-28").unwrap();
		assert_eq!(typed.path_types, vec!["thinking,irc".to_owned()]);
		assert_eq!(typed.route, Route::Date { bucket: BucketRef::Here, date: "2026-10-28".into() });
	}

	#[test]
	fn removed_routes_name_their_replacement() {
		assert_eq!(
			message("session://README"),
			(ErrorKind::BadRequest, "session://README was removed: the session:// grammar is in xd://read/urls.".into())
		);
		for (url, named) in [
			("session://find?text=x", "session://find"),
			("session://sessions.jsonl", "session://sessions.jsonl"),
			("session://here/sessions.jsonl", "session://here/sessions.jsonl"),
			("session://projects/-w/sessions.jsonl", "session://projects/-w/sessions.jsonl"),
			("session://type/x/find", "session://find"),
		] {
			let (kind, text) = message(url.split('?').next().unwrap());
			assert_eq!(kind, ErrorKind::BadRequest);
			assert_eq!(text, format!("{named} was removed: {ARCHIVE}"));
		}
	}

	#[test]
	fn grammar_errors_name_the_segment() {
		for (url, why) in [
			("session://01ab/entry/@0", "bad line \"@0\""),
			("session://01ab/entry/@1.5", "bad line \"@1.5\""),
			("session://01ab/nope", "unknown path /nope"),
			("session://01ab/agents/a b/timeline", "bad agent id \"a b\""),
			("session://here/2026-1-1", "expected a YYYY-MM-DD date, got \"2026-1-1\""),
			("session://projects/..", "bad path segment \"..\""),
			("session://01ab/local/a%2Fb", "bad path segment \"a/b\""),
			("session://x.y/timeline", "\"x.y\" is not a session id prefix"),
			("session://type/x", "use session://type/<filter>/<path>, e.g. session://type/thinking/here/"),
			("session://01ab/local/%E0%A4", "bad percent-encoding in path segment \"%E0%A4\""),
		] {
			assert_eq!(message(url), (ErrorKind::BadRequest, format!("Invalid session:// URL {url}: {why}. {GRAMMAR}")), "{url}");
		}
	}

	#[test]
	fn stamps() {
		let (sid, created) = file_stamp("2026-09-30T12-04-17-056Z_01a0f233-8220.jsonl").unwrap();
		assert_eq!(sid, "01a0f233-8220");
		assert_eq!(crate::time::iso_utc(created), "2026-09-30T12:04:17.056Z");
		assert_eq!(file_stamp("2026-13-30T12-04-17-056Z_x.jsonl"), None);
		assert_eq!(file_stamp("2026-09-30T12-04-17-056Z_.jsonl"), None);
		assert_eq!(file_stamp("current.jsonl"), None);
		assert_eq!(id_from_name("2026_a_b.jsonl"), Some("b"));
		assert_eq!(js_number(" 0x1F "), 31.0);
		assert!(js_number("inf").is_nan());
	}

	fn temp_root(name: &str) -> PathBuf {
		let root = std::env::temp_dir().join(format!("omp-sessions-routes-{}-{name}", std::process::id()));
		let _ = fs::remove_dir_all(&root);
		fs::create_dir_all(&root).unwrap();
		root
	}

	fn context(root: &Path) -> Context {
		Context {
			sessions_dir: root.display().to_string(),
			here_dir: root.join("-w").display().to_string(),
			cache_dir: String::new(),
			cwd: String::new(),
			session_id: None,
		}
	}

	#[test]
	fn bucket_order_and_prefix_matching() {
		let root = temp_root("prefix");
		let a = root.join("-w");
		let b = root.join("-v");
		fs::create_dir_all(&a).unwrap();
		fs::create_dir_all(&b).unwrap();
		fs::write(a.join("2026-09-30T12-00-00-000Z_abc1.jsonl"), "").unwrap();
		fs::write(a.join("current.jsonl"), "{\"type\":\"session\",\"id\":\"abc2\",\"timestamp\":\"2026-09-30T10:00:00.000Z\"}\n").unwrap();
		fs::write(a.join("copy.jsonl"), "{\"type\":\"session\",\"id\":\"abc1\",\"timestamp\":\"2026-09-30T10:00:00.000Z\"}\n").unwrap();
		fs::write(b.join("2026-09-29T12-00-00-000Z_abd9.jsonl"), "").unwrap();

		let listed: Vec<String> = bucket_sessions(&a).unwrap().into_iter().map(|s| s.sid).collect();
		assert_eq!(listed, ["abc1", "abc1", "abc2"]);

		let ctx = context(&root);
		let one = locate_session(&Locator::Prefix("abc2".into()), &ctx).unwrap();
		assert_eq!((one.sid.as_str(), one.bucket.as_str()), ("abc2", "-w"));
		// The header-named copy of abc1 is left out: the stamped file holds that id.
		assert_eq!(locate_session(&Locator::Prefix("abc1".into()), &ctx).unwrap().file, a.join("2026-09-30T12-00-00-000Z_abc1.jsonl"));
		let many = locate_session(&Locator::Prefix("ab".into()), &ctx).unwrap_err();
		assert_eq!(many.kind, ErrorKind::Ambiguous);
		assert!(many.message.starts_with("\"ab\" matches 3 sessions: "), "{}", many.message);
		let none = locate_session(&Locator::Prefix("zz".into()), &ctx).unwrap_err();
		assert_eq!(none.kind, ErrorKind::NotFound);
		assert_eq!(none.message, format!("No session id starts with \"zz\". {ARCHIVE_HINT}"));
		let missing = bucket_sessions(&root.join("-gone")).unwrap_err();
		assert_eq!(missing.kind, ErrorKind::NotFound);
		fs::remove_dir_all(&root).unwrap();
	}

	#[test]
	fn subagent_files() {
		let root = temp_root("agents");
		let file = root.join("2026-09-30T12-00-00-000Z_s.jsonl");
		fs::write(&file, "").unwrap();
		let children = child_dir(&file);
		fs::create_dir_all(children.join("A")).unwrap();
		fs::write(children.join("A.jsonl"), "").unwrap();
		fs::write(children.join("__advisor-1.jsonl"), "").unwrap();
		fs::write(children.join("A").join("A.B.jsonl"), "").unwrap();
		fs::create_dir_all(children.join("x").join("y")).unwrap();
		fs::write(children.join("x").join("y").join("Z.jsonl"), "").unwrap();
		let session = SessionRef { sid: "s".into(), file: file.clone(), bucket: String::new() };
		assert_eq!(child_agents(&file), ["A"]);
		assert_eq!(agent_file(&session, "A.B").unwrap(), children.join("A").join("A.B.jsonl"));
		assert_eq!(agent_file(&session, "Z").unwrap(), children.join("x").join("y").join("Z.jsonl"));
		let missing = agent_file(&session, "Q").unwrap_err();
		assert_eq!((missing.kind, missing.message.as_str()), (ErrorKind::NotFound, "No subagent Q in session s. List them with session://s/agents/"));
		fs::remove_dir_all(&root).unwrap();
	}
}
