//! Per-request observation (decision §13): counters the readers bump while a
//! request runs, written as one JSON log line on stderr when it completes.
//! A process serves one request at a time, so the counters are reset at the
//! start of each request and read at its end.

use std::sync::atomic::{AtomicU64, Ordering};

use crate::json::{Obj, Value, stringify};

static TRANSCRIPTS: AtomicU64 = AtomicU64::new(0);
static BYTES: AtomicU64 = AtomicU64::new(0);
static CACHE_HIT: AtomicU64 = AtomicU64::new(0);
static CACHE_MISS: AtomicU64 = AtomicU64::new(0);
static CACHE_WRITE: AtomicU64 = AtomicU64::new(0);
static CACHE_DELETE: AtomicU64 = AtomicU64::new(0);

const ALL: [&AtomicU64; 6] = [&TRANSCRIPTS, &BYTES, &CACHE_HIT, &CACHE_MISS, &CACHE_WRITE, &CACHE_DELETE];

pub fn reset() {
	for counter in ALL {
		counter.store(0, Ordering::Relaxed);
	}
}

/// One transcript file opened for scanning.
pub fn transcript() {
	TRANSCRIPTS.fetch_add(1, Ordering::Relaxed);
}

/// Bytes of session JSONL read from disk.
pub fn scanned(bytes: u64) {
	BYTES.fetch_add(bytes, Ordering::Relaxed);
}

pub fn cache_hit() {
	CACHE_HIT.fetch_add(1, Ordering::Relaxed);
}

pub fn cache_miss() {
	CACHE_MISS.fetch_add(1, Ordering::Relaxed);
}

pub fn cache_write() {
	CACHE_WRITE.fetch_add(1, Ordering::Relaxed);
}

pub fn cache_delete() {
	CACHE_DELETE.fetch_add(1, Ordering::Relaxed);
}

/// Peak resident set size of this process so far, in bytes.
#[cfg(unix)]
pub fn peak_rss_bytes() -> u64 {
	// SAFETY: getrusage writes into the zeroed struct we pass and reads nothing else.
	let usage = unsafe {
		let mut usage: libc::rusage = std::mem::zeroed();
		if libc::getrusage(libc::RUSAGE_SELF, &raw mut usage) != 0 {
			return 0;
		}
		usage
	};
	let max = u64::try_from(usage.ru_maxrss).unwrap_or(0);
	// macOS reports bytes, Linux kibibytes.
	if cfg!(target_os = "macos") { max } else { max * 1024 }
}

/// Peak resident set size of this process so far, in bytes.
#[cfg(windows)]
pub fn peak_rss_bytes() -> u64 {
	use windows_sys::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
	use windows_sys::Win32::System::Threading::GetCurrentProcess;
	// SAFETY: the counters struct is sized by `cb` and written by the call only.
	unsafe {
		let mut counters: PROCESS_MEMORY_COUNTERS = std::mem::zeroed();
		counters.cb = u32::try_from(std::mem::size_of::<PROCESS_MEMORY_COUNTERS>()).unwrap_or(0);
		if GetProcessMemoryInfo(GetCurrentProcess(), &raw mut counters, counters.cb) == 0 {
			return 0;
		}
		u64::try_from(counters.PeakWorkingSetSize).unwrap_or(0)
	}
}

fn num(value: u64) -> Value {
	// Counts stay far below 2^53.
	Value::Num(value as f64)
}

/// The log line of one completed request: op, request id, duration, scan and cache counters, peak RSS.
pub fn log_line(op: &str, id: u64, ms: u64) -> String {
	let mut cache = Obj::new();
	cache.insert("hit", num(CACHE_HIT.load(Ordering::Relaxed)));
	cache.insert("miss", num(CACHE_MISS.load(Ordering::Relaxed)));
	cache.insert("write", num(CACHE_WRITE.load(Ordering::Relaxed)));
	cache.insert("delete", num(CACHE_DELETE.load(Ordering::Relaxed)));
	let mut line = Obj::new();
	line.insert("op", Value::Str(op.to_string()));
	line.insert("id", num(id));
	line.insert("ms", num(ms));
	line.insert("transcripts", num(TRANSCRIPTS.load(Ordering::Relaxed)));
	line.insert("bytes", num(BYTES.load(Ordering::Relaxed)));
	line.insert("cache", Value::Obj(cache));
	line.insert("peakRssBytes", num(peak_rss_bytes()));
	stringify(&Value::Obj(line), None)
}
