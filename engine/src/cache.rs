//! The cold cache (decision §16): a transcript's full engine-derived session
//! (active branch events and metadata), kept under the request's `cacheDir`
//! once the file's last `type: "message"` entry is older than 24 hours.
//!
//! An entry is used only while its source file has the identity it recorded
//! (device, inode, size, mtime) and the entry's format is this build's;
//! otherwise the entry is deleted and the source parsed again. A transcript
//! that is not cold is parsed every time and never written. Entries are written
//! to a temporary file and renamed into place. Entries whose source no longer
//! exists are deleted at a process's first use of each cache directory.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};

use crate::error::{EngineError, Result};
use crate::events::{
	self, Build, Call, CallStatus, ChildTask, EventBase, EventKind, ExecLang, LoadedSession, SessionEvent, SessionIndex, SessionMeta, Text,
};
use crate::json::{Clipped, Obj, Value};
use crate::metrics;
use crate::protocol::Context;
use crate::scan::ScanStats;
use crate::time;

/// A transcript is cold once its last message is this old.
const COLD_MS: i64 = 24 * 60 * 60 * 1000;

/// Bytes every entry starts with, then the format version.
const MAGIC: &[u8; 8] = b"OMPSCACH";
/// The entry format; an entry of another format is never read.
const FORMAT: u32 = 1;

/// The cache root of one request.
pub struct Cache {
	dir: PathBuf,
}

impl Cache {
	/// The request's cache, or None when `cacheDir` is empty. A `cacheDir`
	/// inside `sessionsDir` is refused: the cache never writes there.
	pub fn of(context: &Context) -> Result<Option<Cache>> {
		if context.cache_dir.is_empty() {
			return Ok(None);
		}
		let dir = PathBuf::from(&context.cache_dir);
		let sessions = Path::new(&context.sessions_dir);
		let real = |path: &Path| fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
		if !context.sessions_dir.is_empty() && real(&dir).starts_with(real(sessions)) {
			return Err(EngineError::bad_request(format!("cacheDir {} lies inside sessionsDir {}", context.cache_dir, context.sessions_dir)));
		}
		Ok(Some(Cache { dir }))
	}

	fn entry_path(&self, file: &Path) -> PathBuf {
		self.dir.join(format!("{:016x}.entry", fnv1a(file.as_os_str().as_encoded_bytes())))
	}

	/// Deletes entries whose source file no longer exists, at this process's
	/// first use of the directory. Workers arriving meanwhile wait for it.
	fn sweep_once(&self) {
		static SWEPT: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());
		let mut swept = SWEPT.lock().unwrap_or_else(PoisonError::into_inner);
		if swept.contains(&self.dir) {
			return;
		}
		swept.push(self.dir.clone());
		let Ok(listing) = fs::read_dir(&self.dir) else { return };
		for item in listing.flatten() {
			let path = item.path();
			if path.extension().is_none_or(|extension| extension != "entry") {
				continue;
			}
			let Some(prefix) = header_prefix(&path) else { continue };
			let gone = match Header::read(&mut Reader::new(&prefix)) {
				Ok(header) => !Path::new(&header.source).exists(),
				// An entry this build cannot read is kept for the build that wrote it.
				Err(Corrupt) => false,
			};
			if gone && fs::remove_file(&path).is_ok() {
				metrics::cache_delete();
			}
		}
	}

	/// The cached session of `file` when its entry is valid for `identity`.
	/// An entry of `file` that is stale, of another format or unreadable is
	/// deleted; an entry of another source under the same name is left alone.
	fn lookup(&self, file: &Path, identity: &Identity, sid: &str, agent: &str) -> Option<LoadedSession> {
		let path = self.entry_path(file);
		let bytes = fs::read(&path).ok()?;
		let mut reader = Reader::new(&bytes);
		let loaded = match Header::read(&mut reader) {
			Ok(header) if header.source.as_bytes() != file.as_os_str().as_encoded_bytes() => return None,
			Ok(header) if header.identity != *identity => None,
			Ok(_) => read_session(&mut reader, file, sid, agent).ok(),
			Err(Corrupt) => None,
		};
		if loaded.is_none() && fs::remove_file(&path).is_ok() {
			metrics::cache_delete();
		}
		loaded
	}

	/// Writes `loaded` as the entry of `file` with `identity`: a temporary file, then a rename.
	fn store(&self, file: &Path, identity: &Identity, loaded: &LoadedSession) -> Result<()> {
		static SEQUENCE: AtomicU64 = AtomicU64::new(0);
		fs::create_dir_all(&self.dir).map_err(|error| EngineError::io(&self.dir, &error))?;
		let mut writer = Writer::default();
		Header { source: file.as_os_str().to_string_lossy().into_owned(), identity: *identity }.write(&mut writer);
		write_session(&mut writer, loaded);
		let target = self.entry_path(file);
		let temporary = self.dir.join(format!(".{}.{}.tmp", std::process::id(), SEQUENCE.fetch_add(1, Ordering::Relaxed)));
		fs::write(&temporary, &writer.bytes).map_err(|error| EngineError::io(&temporary, &error))?;
		if let Err(error) = fs::rename(&temporary, &target) {
			let _ = fs::remove_file(&temporary);
			return Err(EngineError::io(&target, &error));
		}
		metrics::cache_write();
		Ok(())
	}
}

/// What a transcript load starts from.
pub enum Source {
	/// The full session: from the cache, or just built for it.
	Full(LoadedSession),
	/// The index pass of a transcript that is read from its source this time.
	Index(SessionIndex),
}

/// A transcript's session from the cache when a valid entry exists; otherwise
/// its index pass. A cold transcript read from its source is built in full and
/// written to the cache, unless the file changed while it was read.
pub fn source(cache: Option<&Cache>, file: &Path, sid: &str, agent: &str, now_ms: i64) -> Result<Source> {
	let Some(cache) = cache else { return Ok(Source::Index(events::index_session(file, sid, agent)?)) };
	cache.sweep_once();
	let identity = Identity::of(file)?;
	if let Some(loaded) = cache.lookup(file, &identity, sid, agent) {
		metrics::cache_hit();
		return Ok(Source::Full(loaded));
	}
	metrics::cache_miss();
	let index = events::index_session(file, sid, agent)?;
	let cold = index.last_message_t.as_deref().and_then(time::parse_ms).is_some_and(|at| at < now_ms - COLD_MS);
	if !cold {
		return Ok(Source::Index(index));
	}
	let events = events::build_events(&index, &|_| Build::Full)?;
	let loaded = LoadedSession { meta: index.meta, events };
	if Identity::of(file)? == identity {
		cache.store(file, &identity, &loaded)?;
	}
	Ok(Source::Full(loaded))
}

/// The source file's identity an entry is valid for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Identity {
	device: u64,
	inode: u64,
	size: u64,
	/// Modification time in nanoseconds since the epoch.
	mtime_ns: i128,
}

impl Identity {
	fn of(file: &Path) -> Result<Identity> {
		let metadata = fs::metadata(file).map_err(|error| EngineError::io(file, &error))?;
		let mtime_ns = metadata
			.modified()
			.ok()
			.and_then(|at| at.duration_since(std::time::UNIX_EPOCH).ok())
			.map_or(0, |since| since.as_nanos() as i128);
		let (device, inode) = device_and_inode(&metadata);
		Ok(Identity { device, inode, size: metadata.len(), mtime_ns })
	}
}

#[cfg(unix)]
fn device_and_inode(metadata: &fs::Metadata) -> (u64, u64) {
	use std::os::unix::fs::MetadataExt;
	(metadata.dev(), metadata.ino())
}

/// Windows exposes no stable device and file index through std; size and
/// mtime carry the identity there.
#[cfg(not(unix))]
fn device_and_inode(_metadata: &fs::Metadata) -> (u64, u64) {
	(0, 0)
}

/// The leading bytes of an entry, enough for its header.
fn header_prefix(path: &Path) -> Option<Vec<u8>> {
	const HEADER_BYTES: u64 = 64 * 1024;
	let mut prefix = Vec::new();
	fs::File::open(path).ok()?.take(HEADER_BYTES).read_to_end(&mut prefix).ok()?;
	Some(prefix)
}

/// FNV-1a over the source path: the entry's file name.
fn fnv1a(bytes: &[u8]) -> u64 {
	bytes.iter().fold(0xcbf2_9ce4_8422_2325u64, |hash, &byte| (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3))
}

struct Header {
	source: String,
	identity: Identity,
}

impl Header {
	fn write(&self, writer: &mut Writer) {
		writer.bytes.extend_from_slice(MAGIC);
		writer.u64(u64::from(FORMAT));
		writer.str(&self.source);
		writer.u64(self.identity.device);
		writer.u64(self.identity.inode);
		writer.u64(self.identity.size);
		writer.bytes.extend_from_slice(&self.identity.mtime_ns.to_le_bytes());
	}

	fn read(reader: &mut Reader<'_>) -> Decoded<Header> {
		if reader.take(MAGIC.len())? != MAGIC || reader.u64()? != u64::from(FORMAT) {
			return Err(Corrupt);
		}
		let source = reader.string()?;
		let (device, inode, size) = (reader.u64()?, reader.u64()?, reader.u64()?);
		let mtime_ns = i128::from_le_bytes(reader.array()?);
		Ok(Header { source, identity: Identity { device, inode, size, mtime_ns } })
	}
}

/// An entry this build cannot read: another format, or truncated or damaged bytes.
#[derive(Debug)]
struct Corrupt;

type Decoded<T> = std::result::Result<T, Corrupt>;

/// Little-endian base-128 integers, length-prefixed strings, tagged variants.
#[derive(Default)]
struct Writer {
	bytes: Vec<u8>,
}

impl Writer {
	fn u64(&mut self, mut value: u64) {
		loop {
			let byte = (value & 0x7f) as u8;
			value >>= 7;
			if value == 0 {
				self.bytes.push(byte);
				return;
			}
			self.bytes.push(byte | 0x80);
		}
	}

	fn i64(&mut self, value: i64) {
		self.u64(((value << 1) ^ (value >> 63)) as u64);
	}

	fn tag(&mut self, tag: u8) {
		self.bytes.push(tag);
	}

	fn str(&mut self, text: &str) {
		self.u64(text.len() as u64);
		self.bytes.extend_from_slice(text.as_bytes());
	}

	fn opt_str(&mut self, text: Option<&str>) {
		match text {
			None => self.tag(0),
			Some(text) => {
				self.tag(1);
				self.str(text);
			}
		}
	}

	fn opt_u64(&mut self, value: Option<u64>) {
		match value {
			None => self.tag(0),
			Some(value) => {
				self.tag(1);
				self.u64(value);
			}
		}
	}

	fn opt_i64(&mut self, value: Option<i64>) {
		match value {
			None => self.tag(0),
			Some(value) => {
				self.tag(1);
				self.i64(value);
			}
		}
	}
}

struct Reader<'a> {
	bytes: &'a [u8],
	at: usize,
}

impl<'a> Reader<'a> {
	fn new(bytes: &'a [u8]) -> Self {
		Reader { bytes, at: 0 }
	}

	fn take(&mut self, size: usize) -> Decoded<&'a [u8]> {
		let end = self.at.checked_add(size).filter(|end| *end <= self.bytes.len()).ok_or(Corrupt)?;
		let slice = &self.bytes[self.at..end];
		self.at = end;
		Ok(slice)
	}

	fn array<const N: usize>(&mut self) -> Decoded<[u8; N]> {
		self.take(N)?.try_into().map_err(|_| Corrupt)
	}

	fn u64(&mut self) -> Decoded<u64> {
		let mut value = 0u64;
		for shift in (0..64).step_by(7) {
			let byte = self.take(1)?[0];
			value |= u64::from(byte & 0x7f) << shift;
			if byte & 0x80 == 0 {
				return Ok(value);
			}
		}
		Err(Corrupt)
	}

	fn usize(&mut self) -> Decoded<usize> {
		usize::try_from(self.u64()?).map_err(|_| Corrupt)
	}

	fn i64(&mut self) -> Decoded<i64> {
		let raw = self.u64()?;
		Ok(((raw >> 1) as i64) ^ -((raw & 1) as i64))
	}

	fn tag(&mut self) -> Decoded<u8> {
		Ok(self.take(1)?[0])
	}

	fn string(&mut self) -> Decoded<String> {
		let length = self.usize()?;
		String::from_utf8(self.take(length)?.to_vec()).map_err(|_| Corrupt)
	}

	fn opt_string(&mut self) -> Decoded<Option<String>> {
		match self.tag()? {
			0 => Ok(None),
			1 => self.string().map(Some),
			_ => Err(Corrupt),
		}
	}

	fn opt_u64(&mut self) -> Decoded<Option<u64>> {
		match self.tag()? {
			0 => Ok(None),
			1 => self.u64().map(Some),
			_ => Err(Corrupt),
		}
	}

	fn opt_i64(&mut self) -> Decoded<Option<i64>> {
		match self.tag()? {
			0 => Ok(None),
			1 => self.i64().map(Some),
			_ => Err(Corrupt),
		}
	}
}

/// The session without its file, sid and agent: the caller supplies those.
fn write_session(writer: &mut Writer, loaded: &LoadedSession) {
	let meta = &loaded.meta;
	writer.str(&meta.cwd);
	writer.opt_str(meta.title.as_deref());
	writer.str(&meta.created);
	writer.opt_str(meta.parent_session.as_deref());
	writer.opt_str(meta.leaf.as_deref());
	writer.u64(meta.entries as u64);
	writer.u64(meta.on_branch as u64);
	let stats = &meta.stats;
	writer.u64(stats.records);
	writer.u64(stats.invalid_lines.len() as u64);
	for line in &stats.invalid_lines {
		writer.u64(*line);
	}
	writer.opt_u64(stats.incomplete_tail);
	writer.u64(stats.bytes);
	writer.u64(loaded.events.len() as u64);
	for event in &loaded.events {
		write_event(writer, event);
	}
}

fn read_session(reader: &mut Reader<'_>, file: &Path, sid: &str, agent: &str) -> Decoded<LoadedSession> {
	let cwd = reader.string()?;
	let title = reader.opt_string()?;
	let created = reader.string()?;
	let parent_session = reader.opt_string()?;
	let leaf = reader.opt_string()?;
	let entries = reader.usize()?;
	let on_branch = reader.usize()?;
	let records = reader.u64()?;
	let invalid_count = reader.usize()?;
	let invalid_lines = (0..invalid_count).map(|_| reader.u64()).collect::<Decoded<Vec<_>>>()?;
	let incomplete_tail = reader.opt_u64()?;
	let bytes = reader.u64()?;
	let count = reader.usize()?;
	let events = (0..count).map(|_| read_event(reader)).collect::<Decoded<Vec<_>>>()?;
	if reader.at != reader.bytes.len() {
		return Err(Corrupt);
	}
	Ok(LoadedSession {
		meta: SessionMeta {
			sid: sid.to_owned(),
			agent: agent.to_owned(),
			file: file.to_path_buf(),
			cwd,
			title,
			created,
			parent_session,
			leaf,
			entries,
			on_branch,
			stats: ScanStats { records, invalid_lines, incomplete_tail, bytes },
		},
		events,
	})
}

fn write_text(writer: &mut Writer, text: &Text) {
	match text {
		Text::Plain(plain) => {
			writer.tag(0);
			writer.str(plain);
		}
		Text::Clipped(clipped) => {
			writer.tag(1);
			write_clipped(writer, clipped);
		}
	}
}

fn read_text(reader: &mut Reader<'_>) -> Decoded<Text> {
	match reader.tag()? {
		0 => Ok(Text::Plain(reader.string()?)),
		1 => Ok(Text::Clipped(read_clipped(reader)?)),
		_ => Err(Corrupt),
	}
}

fn write_clipped(writer: &mut Writer, clipped: &Clipped) {
	writer.str(&clipped.text);
	writer.u64(clipped.chars as u64);
	writer.u64(clipped.lines as u64);
}

fn read_clipped(reader: &mut Reader<'_>) -> Decoded<Clipped> {
	Ok(Clipped { text: reader.string()?, chars: reader.usize()?, lines: reader.usize()? })
}

fn write_value(writer: &mut Writer, value: &Value) {
	match value {
		Value::Null => writer.tag(0),
		Value::Bool(flag) => writer.tag(if *flag { 2 } else { 1 }),
		Value::Num(number) => {
			writer.tag(3);
			writer.bytes.extend_from_slice(&number.to_le_bytes());
		}
		Value::Str(text) => {
			writer.tag(4);
			writer.str(text);
		}
		Value::Clipped(clipped) => {
			writer.tag(5);
			write_clipped(writer, clipped);
		}
		Value::Arr(items) => {
			writer.tag(6);
			writer.u64(items.len() as u64);
			for item in items {
				write_value(writer, item);
			}
		}
		Value::Obj(obj) => {
			writer.tag(7);
			writer.u64(obj.len() as u64);
			for (key, item) in obj.iter() {
				writer.str(key);
				write_value(writer, item);
			}
		}
	}
}

fn read_value(reader: &mut Reader<'_>) -> Decoded<Value> {
	Ok(match reader.tag()? {
		0 => Value::Null,
		1 => Value::Bool(false),
		2 => Value::Bool(true),
		3 => Value::Num(f64::from_le_bytes(reader.array()?)),
		4 => Value::Str(reader.string()?),
		5 => Value::Clipped(read_clipped(reader)?),
		6 => {
			let count = reader.usize()?;
			Value::Arr((0..count).map(|_| read_value(reader)).collect::<Decoded<Vec<_>>>()?)
		}
		7 => {
			let count = reader.usize()?;
			let mut obj = Obj::new();
			// Entries were written in the object's order, so inserting them in turn rebuilds it.
			for _ in 0..count {
				let key = reader.string()?;
				obj.insert(key, read_value(reader)?);
			}
			Value::Obj(obj)
		}
		_ => return Err(Corrupt),
	})
}

fn write_call(writer: &mut Writer, call: &Call) {
	writer.str(&call.id);
	writer.str(&call.tool);
	write_value(writer, &call.args);
	writer.opt_str(call.intent.as_deref());
	writer.tag(match call.status {
		CallStatus::Pending => 0,
		CallStatus::Ok => 1,
		CallStatus::Error => 2,
	});
	writer.opt_str(call.result_entry.as_deref());
	writer.opt_u64(call.result_chars);
	writer.opt_i64(call.dur_ms);
}

fn read_call(reader: &mut Reader<'_>) -> Decoded<Call> {
	Ok(Call {
		id: reader.string()?,
		tool: reader.string()?,
		args: read_value(reader)?,
		intent: reader.opt_string()?,
		status: match reader.tag()? {
			0 => CallStatus::Pending,
			1 => CallStatus::Ok,
			2 => CallStatus::Error,
			_ => return Err(Corrupt),
		},
		result_entry: reader.opt_string()?,
		result_chars: reader.opt_u64()?,
		dur_ms: reader.opt_i64()?,
	})
}

fn write_event(writer: &mut Writer, event: &SessionEvent) {
	let base = &event.base;
	writer.str(&base.entry);
	writer.opt_u64(base.block.map(|block| block as u64));
	writer.str(&base.t);
	writer.u64(base.line);
	writer.u64(u64::from(base.turn));
	match &event.kind {
		EventKind::User { text, injected } => {
			writer.tag(0);
			write_text(writer, text);
			writer.tag(u8::from(*injected));
		}
		EventKind::Developer { text } => {
			writer.tag(1);
			write_text(writer, text);
		}
		EventKind::Thinking { text } => {
			writer.tag(2);
			write_text(writer, text);
		}
		EventKind::Say { text } => {
			writer.tag(3);
			write_text(writer, text);
		}
		EventKind::Call { call } => {
			writer.tag(4);
			write_call(writer, call);
		}
		EventKind::IrcOut { to, text, call } => {
			writer.tag(5);
			writer.str(to);
			write_text(writer, text);
			write_call(writer, call);
		}
		EventKind::Dispatch { call, context, children } => {
			writer.tag(6);
			write_call(writer, call);
			match context {
				None => writer.tag(0),
				Some(context) => {
					writer.tag(1);
					write_text(writer, context);
				}
			}
			writer.u64(children.len() as u64);
			for child in children {
				writer.str(&child.name);
				writer.str(&child.agent);
				write_text(writer, &child.task);
			}
		}
		EventKind::IrcIn { from, to, text } => {
			writer.tag(7);
			writer.str(from);
			writer.str(to);
			write_text(writer, text);
		}
		EventKind::Subresult { id, agent, status, duration, text } => {
			writer.tag(8);
			writer.str(id);
			writer.str(agent);
			writer.str(status);
			writer.str(duration);
			write_text(writer, text);
		}
		EventKind::Exec { lang, source, status } => {
			writer.tag(9);
			writer.tag(match lang {
				ExecLang::Bash => 0,
				ExecLang::Python => 1,
			});
			write_text(writer, source);
			writer.str(status);
		}
		EventKind::Marker { what, detail } => {
			writer.tag(10);
			writer.str(what);
			writer.str(detail);
		}
	}
}

fn read_event(reader: &mut Reader<'_>) -> Decoded<SessionEvent> {
	let base = EventBase {
		entry: reader.string()?,
		block: reader.opt_u64()?.map(|block| usize::try_from(block).map_err(|_| Corrupt)).transpose()?,
		t: reader.string()?,
		line: reader.u64()?,
		turn: u32::try_from(reader.u64()?).map_err(|_| Corrupt)?,
	};
	let kind = match reader.tag()? {
		0 => EventKind::User {
			text: read_text(reader)?,
			injected: match reader.tag()? {
				0 => false,
				1 => true,
				_ => return Err(Corrupt),
			},
		},
		1 => EventKind::Developer { text: read_text(reader)? },
		2 => EventKind::Thinking { text: read_text(reader)? },
		3 => EventKind::Say { text: read_text(reader)? },
		4 => EventKind::Call { call: read_call(reader)? },
		5 => EventKind::IrcOut { to: reader.string()?, text: read_text(reader)?, call: read_call(reader)? },
		6 => {
			let call = read_call(reader)?;
			let context = match reader.tag()? {
				0 => None,
				1 => Some(read_text(reader)?),
				_ => return Err(Corrupt),
			};
			let count = reader.usize()?;
			let children = (0..count)
				.map(|_| Ok(ChildTask { name: reader.string()?, agent: reader.string()?, task: read_text(reader)? }))
				.collect::<Decoded<Vec<_>>>()?;
			EventKind::Dispatch { call, context, children }
		}
		7 => EventKind::IrcIn { from: reader.string()?, to: reader.string()?, text: read_text(reader)? },
		8 => EventKind::Subresult {
			id: reader.string()?,
			agent: reader.string()?,
			status: reader.string()?,
			duration: reader.string()?,
			text: read_text(reader)?,
		},
		9 => EventKind::Exec {
			lang: match reader.tag()? {
				0 => ExecLang::Bash,
				1 => ExecLang::Python,
				_ => return Err(Corrupt),
			},
			source: read_text(reader)?,
			status: reader.string()?,
		},
		10 => EventKind::Marker { what: reader.string()?, detail: reader.string()? },
		_ => return Err(Corrupt),
	};
	Ok(SessionEvent { base, kind })
}

#[cfg(test)]
mod tests {
	use super::*;

	fn call() -> Call {
		let mut args = Obj::new();
		args.insert("path", Value::Str("a".into()));
		args.insert("2", Value::Num(-0.5));
		args.insert("n", Value::Arr(vec![Value::Null, Value::Bool(true), Value::Clipped(Clipped { text: "x".into(), chars: 9, lines: 2 })]));
		Call {
			id: "c1".into(),
			tool: "read".into(),
			args: Value::Obj(args),
			intent: Some("Reading".into()),
			status: CallStatus::Error,
			result_entry: None,
			result_chars: Some(12),
			dur_ms: Some(-3),
		}
	}

	#[test]
	fn sessions_round_trip_through_an_entry() {
		let base = |line: u64| EventBase { entry: format!("e{line}"), block: Some(1), t: "2026-11-01T10:00:00.000Z".into(), line, turn: 2 };
		let clipped = Text::Clipped(Clipped { text: "abc".into(), chars: 5000, lines: 40 });
		let kinds = vec![
			EventKind::User { text: Text::Plain("hi".into()), injected: true },
			EventKind::Developer { text: clipped.clone() },
			EventKind::Thinking { text: Text::Plain(String::new()) },
			EventKind::Say { text: Text::Plain("é".into()) },
			EventKind::Call { call: call() },
			EventKind::IrcOut { to: "Main".into(), text: clipped.clone(), call: call() },
			EventKind::Dispatch {
				call: call(),
				context: Some(Text::Plain("ctx".into())),
				children: vec![ChildTask { name: "A".into(), agent: "0-A".into(), task: clipped.clone() }],
			},
			EventKind::IrcIn { from: "A".into(), to: "Main".into(), text: Text::Plain("x".into()) },
			EventKind::Subresult { id: "0-A".into(), agent: "a".into(), status: "ok".into(), duration: "1s".into(), text: clipped },
			EventKind::Exec { lang: ExecLang::Python, source: Text::Plain("print(1)".into()), status: "ok".into() },
			EventKind::Marker { what: "compaction".into(), detail: String::new() },
		];
		let loaded = LoadedSession {
			meta: SessionMeta {
				sid: "s".into(),
				agent: "Kid".into(),
				file: PathBuf::from("/f.jsonl"),
				cwd: "/w".into(),
				title: None,
				created: "2026-11-01".into(),
				parent_session: Some("p".into()),
				leaf: Some("e9".into()),
				entries: 10,
				on_branch: 9,
				stats: ScanStats { records: 10, invalid_lines: vec![3, 7], incomplete_tail: Some(11), bytes: 999 },
			},
			events: kinds.into_iter().enumerate().map(|(line, kind)| SessionEvent { base: base(line as u64), kind }).collect(),
		};
		let mut writer = Writer::default();
		let identity = Identity { device: 1, inode: 2, size: 3, mtime_ns: -4 };
		Header { source: "/f.jsonl".into(), identity }.write(&mut writer);
		write_session(&mut writer, &loaded);
		let mut reader = Reader::new(&writer.bytes);
		let header = Header::read(&mut reader).unwrap();
		assert_eq!((header.source.as_str(), header.identity), ("/f.jsonl", identity));
		assert_eq!(read_session(&mut reader, Path::new("/f.jsonl"), "s", "Kid").unwrap(), loaded);
		// A truncated entry is unreadable, never a shorter session.
		let mut short = Reader::new(&writer.bytes[..writer.bytes.len() - 1]);
		Header::read(&mut short).unwrap();
		assert!(read_session(&mut short, Path::new("/f.jsonl"), "s", "Kid").is_err());
	}
}
