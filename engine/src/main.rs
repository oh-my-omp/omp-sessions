//! `omp-sessions-engine read`: answers request frames from stdin until it closes.
//! `omp-sessions-engine query`: the query process of one omp session (`engine::query`).
//! Stdout carries only response frames; stderr carries one log line per request.

use std::io::{BufRead, Write};
use std::process::ExitCode;
use std::time::Instant;

use engine::{metrics, ops};

fn read() -> ExitCode {
	let stdin = std::io::stdin().lock();
	let mut stdout = std::io::stdout().lock();
	let mut stderr = std::io::stderr().lock();
	for line in stdin.lines() {
		let Ok(line) = line else { return ExitCode::FAILURE };
		if line.trim().is_empty() {
			continue;
		}
		metrics::reset();
		let started = Instant::now();
		let answer = ops::answer(&line);
		let frame = match serde_json::to_string(&answer.response) {
			Ok(frame) => frame,
			Err(_) => return ExitCode::FAILURE,
		};
		if writeln!(stdout, "{frame}").and_then(|()| stdout.flush()).is_err() {
			return ExitCode::FAILURE;
		}
		let ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
		// The log line is best effort: a closed stderr must not fail the request.
		let _ = writeln!(stderr, "{}", metrics::log_line(answer.op, answer.id, ms));
	}
	ExitCode::SUCCESS
}

fn main() -> ExitCode {
	match std::env::args().nth(1).as_deref() {
		Some("read") => read(),
		Some("query") => engine::query::serve(),
		_ => {
			eprintln!("usage: omp-sessions-engine read|query   (request frames on stdin, one JSON object per line)");
			ExitCode::from(2)
		}
	}
}
