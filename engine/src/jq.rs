//! jq over JSONL resources (`?q=…`), with omp's selector parsing, paging and
//! continuation hints (omp `tools/read-json.ts` `parseJsonSelector`,
//! `executeJsonQuery`), evaluated by the jaq interpreter over the records as
//! jq's input stream.

use crate::error::Result;
use crate::json::Value;

/// One page of query results requested via `offset`/`limit`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JsonPage {
	pub offset: usize,
	pub limit: usize,
}

/// Options parsed from a `?q=<filter>` query string.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JsonSelector {
	pub query: String,
	pub raw: bool,
	pub compact: bool,
	/// Present when `offset` or `limit` asks for a page instead of the whole output.
	pub page: Option<JsonPage>,
}

/// Parses `q`/`query`, `raw`, `compact`, `offset` and `limit` from a query
/// string without its leading `?`; None when none of them is present.
pub fn parse_selector(query_string: &str) -> Option<JsonSelector> {
	let _ = query_string;
	todo!("slice: jq")
}

/// Runs `selector` over `records` as jq's input stream and renders the output
/// as omp's read does.
pub fn execute(records: &[Value], selector: &JsonSelector) -> Result<String> {
	let _ = (records, selector);
	todo!("slice: jq")
}
