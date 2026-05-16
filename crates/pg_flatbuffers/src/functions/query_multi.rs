//! `flatbuffers_query_multi` — `SETOF text` fanout entry point.

use super::util::{current_execute_options, resolve_execute_error};
use super::DEFAULT_SCHEMA;
use crate::guc::{current_bounds, current_strict};
use crate::query::{execute_with_options, parse};
use crate::schema_cache::lookup_schema;
use pgrx::iter::SetOfIterator;
use pgrx::prelude::*;

/// Run `query` against `buf` and return all present leaves as a
/// `SETOF text`, one row per leaf in wire-format order. Equivalent
/// to [`flatbuffers_query_array`] up to the row-vs-array shape
/// (skipping absent leaves the same way) but suitable for `LATERAL`
/// joins, `unnest`-free aggregation, and `WITH ORDINALITY` (where
/// the ordinal numbers each *present* leaf 1..N in wire order).
///
/// Empty `bytea` and the no-match cases (`[*]` over an absent /
/// empty vector) yield zero rows. Same `STRICT` / `STABLE` /
/// `parallel_safe` rationale as [`flatbuffers_query`]; same
/// `ERROR`-on-verifier-failure contract.
///
/// The returned [`SetOfIterator`] owns its data (`'static`), so the
/// underlying `buf` is free to be dropped between rows \u2014 today we
/// materialise the whole [`Vec<Option<String>>`] up-front and stream
/// from it. Streaming directly off the [`Table`] would save a copy
/// for huge fanouts; that's a future micro-slice once profiling
/// shows it matters (the design's "verifier result caching" note in
/// \u00a710 carves out the same scope).
#[pg_extern(stable, parallel_safe, strict)]
fn flatbuffers_query_multi(query: &str, buf: &[u8]) -> SetOfIterator<'static, String> {
    if buf.is_empty() {
        return SetOfIterator::empty();
    }

    let parsed = parse(query)
        .unwrap_or_else(|e| error!("flatbuffers_query_multi: invalid query {query:?}: {e}"));

    let schema_name = parsed.schema.as_deref().unwrap_or(DEFAULT_SCHEMA);
    let cached = lookup_schema(schema_name);
    let schema_view = cached.schema();

    let leaves = match execute_with_options(
        buf,
        &schema_view,
        &parsed,
        &current_bounds(),
        &current_execute_options(),
    ) {
        Ok(v) => v,
        Err(e) => resolve_execute_error("flatbuffers_query_multi", e, current_strict()),
    };

    SetOfIterator::new(leaves.into_iter().flatten())
}
