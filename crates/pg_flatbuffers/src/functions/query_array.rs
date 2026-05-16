//! `flatbuffers_query_array` — `text[]` fanout entry point.

use super::DEFAULT_SCHEMA;
use super::util::{current_execute_options, resolve_execute_error};
use crate::guc::{current_bounds, current_strict};
use crate::query::{execute_with_options, parse};
use crate::schema_cache::lookup_schema;
use pgrx::prelude::*;

/// Run `query` against `buf` and return all present leaves as a
/// `text[]`. Absent leaves (the `None` entries in the executor's
/// `Vec<Option<String>>` result) are skipped per design §4.3
/// "absent values are skipped"; the SQL caller sees only present
/// values, in wire-format order.
///
/// Returns the empty array `{}` (not SQL `NULL`) when `buf` is empty
/// or the executor produces no leaves (e.g. `[*]` over an absent
/// vector). This makes `array_length(... , 1)` return `NULL` for
/// "no matches" — matching the way Postgres distinguishes "empty
/// array" from "NULL array".
///
/// Same `STRICT` / `STABLE` / `parallel_safe` rationale as
/// [`flatbuffers_query`]; same `ERROR`-on-verifier-failure contract.
#[pg_extern(stable, parallel_safe, strict)]
fn flatbuffers_query_array(query: &str, buf: &[u8]) -> Vec<String> {
    if buf.is_empty() {
        return Vec::new();
    }

    let parsed = parse(query)
        .unwrap_or_else(|e| error!("flatbuffers_query_array: invalid query {query:?}: {e}"));

    let schema_name = parsed.schema.as_deref().unwrap_or(DEFAULT_SCHEMA);
    let cached = lookup_schema(schema_name);
    let schema_view = cached.schema();

    match execute_with_options(
        buf,
        &schema_view,
        &parsed,
        &current_bounds(),
        &current_execute_options(),
    ) {
        Ok(v) => v.into_iter().flatten().collect(),
        Err(e) => resolve_execute_error("flatbuffers_query_array", e, current_strict())
            .into_iter()
            .flatten()
            .collect(),
    }
}
