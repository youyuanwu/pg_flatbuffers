//! `flatbuffers_query` — scalar leaf entry point.
//!
//! See [`super`] for the doc-comment that lists every v0.1 entry
//! point; this file just contains the SQL-level wrapper that returns
//! the *first* present leaf as `text`.

use super::DEFAULT_SCHEMA;
use super::util::{current_execute_options, resolve_execute_error};
use crate::guc::{current_bounds, current_strict};
use crate::query::{execute_with_options, parse};
use crate::schema_cache::lookup_schema;
use pgrx::prelude::*;

/// Run `query` against `buf` and return the first leaf as `text`, or
/// `NULL` when the leaf is absent (the executor produced `None`),
/// when the executor produced *no* leaves (e.g. `[*]` over an absent
/// vector, see [`crate::query::execute_with_options`]), or when `buf` itself is
/// empty (the "absent payload" contract, design §10).
///
/// `STRICT` so SQL `NULL` in either argument short-circuits to `NULL`
/// without invoking the function — both arguments are required for
/// any meaningful work.
///
/// `STABLE` because the result depends on the schema cache (which can
/// change across statements via `flatbuffers_schemas` writes) but is
/// stable *within* a single query execution.
///
/// Verifier failure raises `ERROR` under the default
/// `pg_flatbuffers.strict = on`. Under `strict = off`, *structural*
/// verifier failures return `NULL` (the scan continues) while
/// *bound* exceedances still raise `ERROR` — see
/// [`crate::verify::VerifyError::is_bound_exceedance`].
///
/// Absent scalar fields read back as their schema default under the
/// default `pg_flatbuffers.fill_scalar_defaults = on` (FlatBuffers
/// reader API parity, §4.3). Under `fill_scalar_defaults = off`,
/// absent scalars surface as SQL `NULL` so callers can distinguish
/// presence from default.
#[pg_extern(stable, parallel_safe, strict)]
fn flatbuffers_query(query: &str, buf: &[u8]) -> Option<String> {
    if buf.is_empty() {
        // Absent payload → SQL NULL, never touch the verifier.
        return None;
    }

    let parsed =
        parse(query).unwrap_or_else(|e| error!("flatbuffers_query: invalid query {query:?}: {e}"));

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
        Err(e) => resolve_execute_error("flatbuffers_query", e, current_strict()),
    };

    // First leaf, regardless of present/absent. For paths without
    // `Step::All` the Vec has length 1; for `[*]` over an empty /
    // absent vector the Vec is empty and we fall through to `None`.
    leaves.into_iter().next().flatten()
}
