//! The session:// handler: listings, rendered leaves, host files and jq.
//! Directory routes list (readDirectory, so grep and glob walk the tree);
//! leaves render in memory per request; file-backed leaves (raw.jsonl,
//! artifacts, local files) answer `locate` with the host file.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::error::{EngineError, ErrorKind, Result};
use crate::events::{self, EntrySelector};
use crate::filter::{self, FilteredRead, TypeFilter};
use crate::json::{self, Value};
use crate::protocol::{Context, DirEntry, Reply, UrlParts};
use crate::render;
use crate::routes::{self, Route, SessionPath, SessionRef, Target};
use crate::window::{self, WindowSpec};
use crate::{info, jq, time};

const JQ_KEYS: &[&str] = &["q", "query", "raw", "compact", "offset", "limit"];
const TYPE_KEYS: &[&str] = &["type"];
const TIME_KEYS: &[&str] = &["since", "until"];

/// A located transcript: the root session, the transcript file, and their canonical URLs.
struct Located {
	session: SessionRef,
	file: PathBuf,
	/// `session://<sid>` or `…/agents/<id>`.
	base: String,
	root: String,
}

fn target_of(url: &UrlParts) -> Result<Target> {
	routes::parse_target(&url.host, &url.pathname, &url.href)
}

/// The query string without its leading `?`.
fn query_string(url: &UrlParts) -> &str {
	url.search.strip_prefix('?').unwrap_or(&url.search)
}

/// The URL without its query, for messages.
fn bare_href(url: &UrlParts) -> &str {
	url.href.split('?').next().unwrap_or_default()
}

fn hex(b: u8) -> Option<u8> {
	match b {
		b'0'..=b'9' => Some(b - b'0'),
		b'a'..=b'f' => Some(b - b'a' + 10),
		b'A'..=b'F' => Some(b - b'A' + 10),
		_ => None,
	}
}

/// WHATWG percent-decoding of form data: `+` is a space, a bad escape stays literal, bytes decode lossily.
fn form_decode(text: &str) -> String {
	let bytes = text.as_bytes();
	let mut out = Vec::with_capacity(bytes.len());
	let mut i = 0;
	while i < bytes.len() {
		match bytes[i] {
			b'+' => out.push(b' '),
			b'%' => {
				if let (Some(high), Some(low)) = (bytes.get(i + 1).and_then(|&b| hex(b)), bytes.get(i + 2).and_then(|&b| hex(b))) {
					out.push(high << 4 | low);
					i += 3;
					continue;
				}
				out.push(b'%');
			}
			b => out.push(b),
		}
		i += 1;
	}
	String::from_utf8_lossy(&out).into_owned()
}

/// `new URLSearchParams(query)`: name-value pairs in order.
fn search_params(url: &UrlParts) -> Vec<(String, String)> {
	query_string(url)
		.split('&')
		.filter(|pair| !pair.is_empty())
		.map(|pair| {
			let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
			(form_decode(name), form_decode(value))
		})
		.collect()
}

/// Rejects query keys outside `allowed`, naming them.
fn check_keys(url: &UrlParts, allowed: &[&[&str]]) -> Result<()> {
	for (key, _) in search_params(url) {
		if !allowed.iter().any(|table| table.contains(&key.as_str())) {
			return Err(EngineError::bad_request(format!("session:// does not accept ?{key}= on {}", bare_href(url))));
		}
	}
	Ok(())
}

fn allowed_keys(route: &Route) -> &'static [&'static [&'static str]] {
	match route {
		Route::Timeline(_) => &[TYPE_KEYS, TIME_KEYS],
		Route::Events(_) => &[JQ_KEYS, TYPE_KEYS, TIME_KEYS],
		// Accepted only to answer with the jq-is-not-here error.
		Route::Raw(_) => &[JQ_KEYS],
		Route::Root
		| Route::Projects
		| Route::Bucket(_)
		| Route::Date { .. }
		| Route::SessionDir(_)
		| Route::AgentsDir(_)
		| Route::Entry { .. }
		| Route::Artifact { .. }
		| Route::Local { .. } => &[],
	}
}

/// Whether a route takes a type filter: the event views it filters, and the directories a glob walks to reach them.
fn takes_type(route: &Route) -> bool {
	match route {
		Route::Timeline(_)
		| Route::Events(_)
		| Route::Root
		| Route::Projects
		| Route::Bucket(_)
		| Route::Date { .. }
		| Route::SessionDir(_)
		| Route::AgentsDir(_) => true,
		Route::Raw(_) | Route::Entry { .. } | Route::Artifact { .. } | Route::Local { .. } => false,
	}
}

/// The type filter of a URL: its `type/<filter>/` path prefix and every `?type=` value, as one union.
fn type_filter(url: &UrlParts, target: &Target) -> Result<Option<TypeFilter>> {
	let params = search_params(url);
	let mut values: Vec<&str> = target.path_types.iter().map(String::as_str).collect();
	values.extend(params.iter().filter(|(key, _)| key == "type").map(|(_, value)| value.as_str()));
	filter::parse_type_filter(&values)
}

/// The `since`/`until` bounds as written, read from the raw query so the `+`
/// of a `+0800` zone stays a `+`.
fn window_spec(url: &UrlParts) -> Result<Option<WindowSpec>> {
	let pairs: Vec<&str> = query_string(url).split('&').collect();
	let value = |key: &str| -> Result<Option<String>> {
		let prefix = format!("{key}=");
		let mut values = Vec::new();
		for pair in pairs.iter().filter(|pair| pair.starts_with(&prefix)) {
			let raw = &pair[prefix.len()..];
			values.push(
				routes::decode_uri_component(raw)
					.ok_or_else(|| EngineError::bad_request(format!("bad percent-encoding in ?{key}={raw}")))?,
			);
		}
		if values.len() > 1 {
			return Err(EngineError::bad_request(format!("Give {key}= once; a time window has one start and one end.")));
		}
		Ok(values.pop())
	};
	let since = value("since")?;
	let until = value("until")?;
	window::parse_window(since.as_deref(), until.as_deref())
}

fn locate_at(at: &SessionPath, context: &Context) -> Result<Located> {
	let session = routes::locate_session(&at.locator, context)?;
	let root = format!("session://{}", session.sid);
	if at.agent.is_empty() {
		let file = session.file.clone();
		return Ok(Located { session, file, base: root.clone(), root });
	}
	let file = routes::agent_file(&session, &at.agent)?;
	Ok(Located { base: format!("{root}/agents/{}", at.agent), session, file, root })
}

fn dir(name: impl Into<String>) -> DirEntry {
	DirEntry { name: name.into(), dir: true }
}

fn file(name: impl Into<String>) -> DirEntry {
	DirEntry { name: name.into(), dir: false }
}

fn local_date(ms: i64) -> String {
	time::local_date(ms).unwrap_or_default()
}

fn read_error(path: &Path, error: &io::Error) -> EngineError {
	match error.kind() {
		io::ErrorKind::NotFound | io::ErrorKind::NotADirectory => EngineError::new(ErrorKind::NotFound, format!("{}: {error}", path.display())),
		_ => EngineError::io(path, error),
	}
}

/// Directory entries for a directory route, or None for a leaf.
fn listing(route: &Route, context: &Context) -> Result<Option<Vec<DirEntry>>> {
	match route {
		Route::Root => Ok(Some(vec![dir("projects")])),
		Route::Projects => {
			let root = Path::new(&context.sessions_dir);
			let mut entries = Vec::new();
			for entry in fs::read_dir(root).map_err(|e| read_error(root, &e))? {
				let entry = entry.map_err(|e| EngineError::io(root, &e))?;
				if entry.file_type().is_ok_and(|t| t.is_dir()) {
					entries.push(dir(entry.file_name().to_string_lossy()));
				}
			}
			Ok(Some(entries))
		}
		Route::Bucket(bucket) => {
			let mut dates: Vec<String> = Vec::new();
			for session in routes::bucket_sessions(&routes::bucket_dir(bucket, context))? {
				let date = local_date(session.created_ms);
				if !dates.contains(&date) {
					dates.push(date);
				}
			}
			dates.sort_unstable();
			Ok(Some(dates.into_iter().map(dir).collect()))
		}
		Route::Date { bucket, date } => {
			// Several files can hold one session id on one day; the listing names the id once, as the URL opens one file.
			let mut sids: Vec<String> = Vec::new();
			for session in routes::bucket_sessions(&routes::bucket_dir(bucket, context))? {
				if &local_date(session.created_ms) == date && !sids.contains(&session.sid) {
					sids.push(session.sid);
				}
			}
			Ok(Some(sids.into_iter().map(dir).collect()))
		}
		Route::SessionDir(at) => {
			let located = locate_at(at, context)?;
			let agents = routes::child_agents(&located.file);
			Ok(Some(if agents.is_empty() { vec![file("timeline")] } else { vec![file("timeline"), dir("agents")] }))
		}
		Route::AgentsDir(at) => {
			let located = locate_at(at, context)?;
			Ok(Some(routes::child_agents(&located.file).into_iter().map(dir).collect()))
		}
		Route::Timeline(_) | Route::Events(_) | Route::Raw(_) | Route::Entry { .. } | Route::Artifact { .. } | Route::Local { .. } => Ok(None),
	}
}

/// Host file behind a file-backed leaf, or None when the leaf is rendered or carries a query.
fn host_file(route: &Route, url: &UrlParts, context: &Context) -> Result<Option<PathBuf>> {
	match route {
		Route::Raw(at) => {
			if !url.search.is_empty() {
				return Ok(None);
			}
			Ok(Some(locate_at(at, context)?.file))
		}
		Route::Artifact { at, id } => {
			let located = locate_at(at, context)?;
			let children = routes::child_dir(&located.session.file);
			let wanted = format!("{id}.");
			let Ok(entries) = fs::read_dir(&children) else { return Ok(None) };
			Ok(entries
				.filter_map(|entry| entry.ok())
				.find(|entry| entry.file_name().to_string_lossy().starts_with(&wanted))
				.map(|entry| children.join(entry.file_name())))
		}
		Route::Local { at, relative } => {
			let located = locate_at(at, context)?;
			let mut path = routes::child_dir(&located.session.file);
			path.push("local");
			path.extend(relative.split('/'));
			Ok(fs::metadata(&path).is_ok_and(|m| m.is_file()).then_some(path))
		}
		Route::Root
		| Route::Projects
		| Route::Bucket(_)
		| Route::Date { .. }
		| Route::SessionDir(_)
		| Route::AgentsDir(_)
		| Route::Timeline(_)
		| Route::Events(_)
		| Route::Entry { .. } => Ok(None),
	}
}

/// JSONL text of `records`, or jq's output over them when the URL carries a
/// selector. jq's continuation hint names only its own parameters; `keep`
/// (`type=…`, the window) is carried into it so the next page keeps the same input.
fn jsonl_resource(url: &UrlParts, records: &[Value], keep: &str) -> Result<String> {
	let Some(selector) = jq::parse_selector(query_string(url)) else {
		return Ok(records.iter().map(|record| json::stringify(record, None)).collect::<Vec<_>>().join("\n"));
	};
	let output = jq::execute(records, &selector)?;
	Ok(if keep.is_empty() { output } else { output.replace("; append ?", &format!("; append ?{keep}&")) })
}

fn leaf(target: &Target, url: &UrlParts, context: &Context) -> Result<String> {
	match &target.route {
		Route::Timeline(at) => {
			let types = type_filter(url, target)?;
			let spec = window_spec(url)?;
			let located = locate_at(at, context)?;
			let (loaded, filter) =
				filter::load_filtered(&located.file, &located.session.sid, &at.agent, types, spec, time::now_ms(), FilteredRead::Timeline)?;
			// omp keeps generated titles in its session index; the journal's own title slot is often empty.
			let title = if at.agent.is_empty() && loaded.meta.title.as_deref().is_none_or(str::is_empty) {
				info::read_session_info(&located.file, false).and_then(|info| info.title)
			} else {
				None
			};
			Ok(render::render_timeline(&loaded, &located.base, &located.root, title.as_deref(), &filter))
		}
		Route::Events(at) => {
			let types = type_filter(url, target)?;
			let spec = window_spec(url)?;
			let located = locate_at(at, context)?;
			let (loaded, filter) =
				filter::load_filtered(&located.file, &located.session.sid, &at.agent, types, spec, time::now_ms(), FilteredRead::Records)?;
			// jq's next page keeps the same input: the types as written, the window as absolute bounds.
			let keep = [
				filter.types.as_ref().map(|types| format!("type={}", types.source.replace('%', "%25").replace('&', "%26"))),
				filter.window.as_ref().map(window::window_query),
			]
			.into_iter()
			.flatten()
			.filter(|part| !part.is_empty())
			.collect::<Vec<_>>()
			.join("&");
			jsonl_resource(url, &render::event_records(&loaded, &located.base, &filter), &keep)
		}
		Route::Entry { at, selector } => {
			let located = locate_at(at, context)?;
			let Some(detail) = events::load_entry(&located.file, selector)? else {
				let name = match selector {
					EntrySelector::Id(id) => format!("#{id}"),
					EntrySelector::Line(line) => format!("line {line}"),
				};
				return Err(EngineError::not_found(format!("No entry {name} in {}", located.base)));
			};
			Ok(render::render_entry(&detail, &located.base, &located.session.sid))
		}
		Route::Raw(_) => {
			if !url.search.is_empty() {
				return Err(EngineError::bad_request(format!(
					"{} takes no query: jq runs on events.jsonl; grep or read raw.jsonl directly.",
					bare_href(url)
				)));
			}
			Err(EngineError::not_found(format!("Session file for {} not found", url.href)))
		}
		Route::Artifact { .. } | Route::Local { .. } => Err(EngineError::not_found(format!("{} not found in the session's artifacts", url.href))),
		Route::Root | Route::Projects | Route::Bucket(_) | Route::Date { .. } | Route::SessionDir(_) | Route::AgentsDir(_) => {
			Err(EngineError::bad_request(format!("{} is a directory", url.href)))
		}
	}
}

/// `ProtocolHandler.resolve`: a directory's listing text, or a leaf rendered.
pub fn resolve(url: &UrlParts, skip_directory_listing: bool, context: &Context) -> Result<Reply> {
	let target = target_of(url)?;
	check_keys(url, allowed_keys(&target.route))?;
	if !target.path_types.is_empty() && !takes_type(&target.route) {
		return Err(EngineError::bad_request(format!(
			"session://type/<filter>/ applies to timeline and events.jsonl (and the directories above them), not {}",
			url.href
		)));
	}
	// Parse the filter here too, so a mistyped one fails on the directory a glob starts from.
	type_filter(url, &target)?;
	if let Some(entries) = listing(&target.route, context)? {
		let content = if skip_directory_listing {
			String::new()
		} else {
			entries.iter().map(|e| if e.dir { format!("{}/", e.name) } else { e.name.clone() }).collect::<Vec<_>>().join("\n")
		};
		return Ok(Reply::Directory { content });
	}
	Ok(Reply::Resource { content: leaf(&target, url, context)? })
}

/// `ProtocolHandler.readDirectory`.
pub fn read_directory(url: &UrlParts, context: &Context) -> Result<Reply> {
	match listing(&target_of(url)?.route, context)? {
		Some(entries) => Ok(Reply::Entries { entries }),
		None => Err(EngineError::bad_request(format!("{} is not a directory", url.href))),
	}
}

/// `ProtocolHandler.locate`: the host file of a file-backed leaf; a malformed,
/// unknown or ambiguous URL locates nothing (resolve reports the reason).
pub fn locate(url: &UrlParts, context: &Context) -> Reply {
	let file = target_of(url)
		.ok()
		// A type prefix makes the leaf a filtered view; resolve rejects it where no view applies.
		.filter(|target| target.path_types.is_empty())
		.and_then(|target| host_file(&target.route, url, context).ok().flatten());
	Reply::Located { file: file.map(|path| path.to_string_lossy().into_owned()) }
}

#[cfg(test)]
mod tests {
	use super::*;

	fn url(href: &str) -> UrlParts {
		let rest = href.strip_prefix("session://").unwrap();
		let (before, search) = rest.split_once('?').map_or((rest, String::new()), |(b, q)| (b, format!("?{q}")));
		let (host, path) = before.split_once('/').map_or((before, ""), |(h, p)| (h, p));
		UrlParts { href: href.to_owned(), host: host.to_owned(), pathname: format!("/{path}"), search }
	}

	#[test]
	fn query_keys_are_checked_per_route() {
		let timeline = url("session://01ab/timeline?type=say&q=.");
		let error = check_keys(&timeline, allowed_keys(&target_of(&timeline).unwrap().route)).unwrap_err();
		assert_eq!(error.message, "session:// does not accept ?q= on session://01ab/timeline");
		let events = url("session://01ab/events.jsonl?q=.&since=2h&type=say");
		assert!(check_keys(&events, allowed_keys(&target_of(&events).unwrap().route)).is_ok());
	}

	#[test]
	fn search_params_decode_as_forms() {
		assert_eq!(
			search_params(&url("session://x/timeline?type=a+b&type=c%2Cd&&bad=%zz")),
			vec![("type".into(), "a b".into()), ("type".into(), "c,d".into()), ("bad".into(), "%zz".into())]
		);
	}

	#[test]
	fn repeated_window_bound_is_refused() {
		let error = window_spec(&url("session://x/timeline?since=1h&since=2h")).unwrap_err();
		assert_eq!(error.message, "Give since= once; a time window has one start and one end.");
		let error = window_spec(&url("session://x/timeline?until=%E0")).unwrap_err();
		assert_eq!(error.kind, ErrorKind::BadRequest);
	}
}
