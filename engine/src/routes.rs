//! session:// URL grammar → Route, and resolution of routes to session files.
//! One traversable tree (projects/<bucket>/<date>/<sid>/…) so grep never
//! visits a session twice; `session://<id-prefix>/…`, `here/…` and detail
//! leaves resolve directly but are not listed. The routes removed by decision
//! §8 parse to a `bad_request` naming their replacement.

use std::path::{Path, PathBuf};

use crate::error::Result;
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

/// Parses a session:// URL's host and path (`url` is the whole href, for messages).
pub fn parse_target(host: &str, pathname: &str, url: &str) -> Result<Target> {
	let _ = (host, pathname, url);
	todo!("slice: routes")
}

/// The directory of a bucket: `sessions_dir/<name>`, or `here_dir`.
pub fn bucket_dir(bucket: &BucketRef, context: &Context) -> PathBuf {
	let _ = (bucket, context);
	todo!("slice: routes")
}

/// Session files directly in a bucket: stamped names first, then header-named
/// files, each kind in file-name order.
pub fn bucket_sessions(dir: &Path) -> Result<Vec<BucketSession>> {
	let _ = dir;
	todo!("slice: routes")
}

/// The root session a locator names; `not_found` on no match, `ambiguous` on several.
pub fn locate_session(locator: &Locator, context: &Context) -> Result<SessionRef> {
	let _ = (locator, context);
	todo!("slice: routes")
}

/// Directory holding a transcript's own children and artifacts: the file path without `.jsonl`.
pub fn child_dir(file: &Path) -> PathBuf {
	let _ = file;
	todo!("slice: routes")
}

/// The transcript of subagent `agent` (full id `A.B.C`) under a root session.
pub fn agent_file(root: &SessionRef, agent: &str) -> Result<PathBuf> {
	let _ = (root, agent);
	todo!("slice: routes")
}

/// Subagent ids directly under a transcript.
pub fn child_agents(transcript: &Path) -> Vec<String> {
	let _ = transcript;
	todo!("slice: routes")
}
