//! `omp-sessions-engine query`: the query process of one omp session
//! (decision §3). It holds one in-memory DuckDB database for the process's
//! life, so the tables, views and macros a caller creates in schema `main`
//! stay between calls; schema `src` belongs to the call that builds it.
//!
//! Requests run one at a time, in arrival order, on a worker thread; this
//! thread keeps reading stdin so a `cancel` frame can interrupt the request it
//! names while it runs. A cancelled request answers `aborted` and the process
//! keeps serving. When stdin closes, the requests already read are answered,
//! then the database closes and the spill directory is removed before the
//! process exits.
//!
//! The database is configured as the TypeScript `openEngine` configured it:
//! `memory_limit` 4 GB, spilling into a private temporary directory, SQL
//! statements may read only below the sessions directory and write no file
//! (`allowed_directories`, `enable_external_access` off, extension
//! auto-install and auto-load off), then `lock_configuration`.

pub mod run;
pub mod sources;

use std::collections::HashSet;
use std::io::{BufRead, Write};
use std::process::ExitCode;
use std::sync::mpsc;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;

use tempfile::TempDir;

use crate::duck::{Duck, DuckError, Interrupter};
use crate::error::{EngineError, ErrorKind, Result};
use crate::metrics;
use crate::ops::{Answer, Parsed};
use crate::protocol::{Cancel as CancelFrame, Context, Op, Reply, Request, Response};

/// Rows one `sql` call returns when it names no `limit`.
const SQL_LIMIT: u64 = 1000;
/// The most rows one `sql` call may ask for.
const SQL_LIMIT_MAX: u64 = 100_000;
/// The most memory the database holds; tables, joins and sorts past it spill to disk.
const MEMORY_LIMIT: &str = "4GB";

/// The database of this query process and the directory it spills into.
pub struct Database {
	pub duck: Duck,
	/// Removed when the database is dropped.
	pub spill: TempDir,
	/// omp's sessions root, the one directory SQL may read below.
	pub sessions_dir: String,
}

impl Database {
	fn open(context: &Context) -> Result<Database> {
		let spill = tempfile::Builder::new()
			.prefix("omp-sessions-duckdb-")
			.tempdir()
			.map_err(|error| EngineError::new(ErrorKind::Io, format!("could not create the spill directory: {error}")))?;
		let duck = Duck::open().map_err(query_failed)?;
		let sessions = format!("{}/", context.sessions_dir.trim_end_matches('/'));
		let settings = [
			format!("SET memory_limit = {}", sources::literal(MEMORY_LIMIT)),
			format!("SET temp_directory = {}", sources::literal(&spill.path().to_string_lossy())),
			format!("SET allowed_directories = [{}]", sources::literal(&sessions)),
			"SET autoinstall_known_extensions = false".to_owned(),
			"SET autoload_known_extensions = false".to_owned(),
			"SET enable_external_access = false".to_owned(),
			"SET lock_configuration = true".to_owned(),
		];
		for setting in settings {
			duck.run(&setting).map_err(query_failed)?;
		}
		Ok(Database { duck, spill, sessions_dir: context.sessions_dir.clone() })
	}
}

fn query_failed(error: DuckError) -> EngineError {
	EngineError::new(ErrorKind::QueryFailed, error.message)
}

/// What the stdin thread and the worker share.
#[derive(Default)]
struct State {
	/// The request the worker is running.
	running: Option<u64>,
	/// Requests read but not yet started.
	queued: HashSet<u64>,
	/// Requests a `cancel` frame named while queued or running.
	cancelled: HashSet<u64>,
	/// Interrupts the database once it is open.
	interrupter: Option<Interrupter>,
}

#[derive(Default)]
struct Control {
	state: Mutex<State>,
}

impl Control {
	fn lock(&self) -> MutexGuard<'_, State> {
		self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
	}

	/// Cancels request `id`: interrupts it when it runs, marks it when it waits.
	fn cancel(&self, id: u64) {
		let mut state = self.lock();
		if state.running == Some(id) {
			state.cancelled.insert(id);
			if let Some(interrupter) = &state.interrupter {
				interrupter.interrupt();
			}
		} else if state.queued.contains(&id) {
			state.cancelled.insert(id);
		}
	}
}

/// One request's view of cancellation.
pub struct Cancel {
	control: Arc<Control>,
	id: u64,
}

impl Cancel {
	pub fn is_cancelled(&self) -> bool {
		self.control.lock().cancelled.contains(&self.id)
	}

	/// `aborted` once the request was cancelled; long Rust-side work calls this between steps.
	pub fn check(&self) -> Result<()> {
		if self.is_cancelled() { Err(aborted(self.id)) } else { Ok(()) }
	}
}

fn aborted(id: u64) -> EngineError {
	EngineError::new(ErrorKind::Aborted, format!("request {id} was cancelled"))
}

/// A DuckDB error of a request: `aborted` when the request was cancelled
/// (DuckDB then reports the interrupt), else `query_failed` with DuckDB's text.
pub fn failed(error: DuckError, cancel: &Cancel) -> EngineError {
	if error.interrupted || cancel.is_cancelled() { aborted(cancel.id) } else { query_failed(error) }
}

/// What the stdin thread hands the worker.
enum Job {
	Request(Box<Request>),
	Invalid(Answer),
}

/// Answers one `sql` or `export` request on the database, opening it on first use.
fn handle(database: &mut Option<Database>, control: &Control, request: &Request, cancel: &Cancel) -> Result<Reply> {
	let (query, from, tables, params) = match &request.op {
		Op::Sql { query, from, tables, params, .. } | Op::Export { query, from, tables, params, .. } => (query, from, tables, params),
		other => {
			return Err(EngineError::bad_request(format!("{} runs in a read process (omp-sessions-engine read), not in the query process", other.name())));
		}
	};
	let params = run::Bindings::parse(params.as_ref())?;
	let limit = match &request.op {
		Op::Sql { limit, .. } => {
			let limit = limit.unwrap_or(SQL_LIMIT);
			if !(1..=SQL_LIMIT_MAX).contains(&limit) {
				return Err(EngineError::bad_request(format!("limit: between 1 and {SQL_LIMIT_MAX}")));
			}
			limit as usize
		}
		_ => 0,
	};
	if database.is_none() {
		let opened = Database::open(&request.context)?;
		control.lock().interrupter = Some(opened.duck.interrupter());
		*database = Some(opened);
	}
	let Some(database) = database.as_ref() else { return Err(EngineError::new(ErrorKind::QueryFailed, "the database is not open")) };
	let outcome = sources::build(&database.duck, from.as_ref(), tables.as_ref(), query, &request.context, cancel).and_then(|()| match &request.op {
		Op::Export { path, format, .. } => run::export(database, query, params.as_ref(), path, *format, cancel),
		_ => run::run_sql(&database.duck, query, params.as_ref(), limit, cancel),
	});
	sources::drop(&database.duck);
	outcome
}

fn worker(jobs: mpsc::Receiver<Job>, control: Arc<Control>) -> ExitCode {
	let mut stdout = std::io::stdout().lock();
	let mut stderr = std::io::stderr().lock();
	let mut database: Option<Database> = None;
	let mut write = |answer: Answer, ms: u64| -> bool {
		let Ok(frame) = serde_json::to_string(&answer.response) else { return false };
		if writeln!(stdout, "{frame}").and_then(|()| stdout.flush()).is_err() {
			return false;
		}
		// The log line is best effort: a closed stderr must not fail the request.
		let _ = writeln!(stderr, "{}", metrics::log_line(answer.op, answer.id, ms));
		true
	};
	while let Ok(job) = jobs.recv() {
		metrics::reset();
		let started = Instant::now();
		let answer = match job {
			Job::Invalid(answer) => answer,
			Job::Request(request) => {
				let id = request.id;
				let cancel = Cancel { control: Arc::clone(&control), id };
				let waiting_cancelled = {
					let mut state = control.lock();
					state.queued.remove(&id);
					let cancelled = state.cancelled.contains(&id);
					if !cancelled {
						state.running = Some(id);
					}
					cancelled
				};
				let outcome = if waiting_cancelled { Err(aborted(id)) } else { handle(&mut database, &control, &request, &cancel) };
				let was_cancelled = {
					let mut state = control.lock();
					state.running = None;
					state.cancelled.remove(&id)
				};
				let outcome = if was_cancelled { Err(aborted(id)) } else { outcome };
				Answer { op: request.op.name(), id, response: Response::of(id, outcome) }
			}
		};
		let ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
		if !write(answer, ms) {
			return ExitCode::FAILURE;
		}
	}
	// Dropping the database closes DuckDB and removes the spill directory.
	drop(database);
	ExitCode::SUCCESS
}

/// Serves request frames from stdin until it closes.
pub fn serve() -> ExitCode {
	let control = Arc::new(Control::default());
	let (jobs, received) = mpsc::channel::<Job>();
	let worker = {
		let control = Arc::clone(&control);
		std::thread::spawn(move || worker(received, control))
	};
	for line in std::io::stdin().lock().lines() {
		let Ok(line) = line else { break };
		if line.trim().is_empty() {
			continue;
		}
		if Parsed::is_cancel(&line) {
			// A malformed cancel frame names no request to cancel.
			if let Ok(frame) = serde_json::from_str::<CancelFrame>(&line) {
				control.cancel(frame.id);
			}
			continue;
		}
		let job = match Parsed::of(&line) {
			Parsed::Request(request) => {
				control.lock().queued.insert(request.id);
				Job::Request(Box::new(request))
			}
			Parsed::Invalid(answer) => Job::Invalid(answer),
		};
		if jobs.send(job).is_err() {
			break;
		}
	}
	drop(jobs);
	worker.join().unwrap_or(ExitCode::FAILURE)
}
