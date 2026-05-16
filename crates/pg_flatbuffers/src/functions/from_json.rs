//! `flatbuffers_from_json` — build a FlatBuffer from a JSON value
//! via the reflection schema (design §8, inverse of [`super::to_json`]).
//!
//! Always raises `ERROR` on a malformed input (missing required
//! field, unknown key, type mismatch, integer out of range, unknown
//! enum name, invalid base64, nesting limit). The `strict` GUC is
//! NOT consulted — design §8: "JSON conversion functions always
//! raise ERROR regardless of `strict`".

use super::util::split_schema_and_table;
use crate::from_json::json_to_buf;
use crate::guc::current_bounds;
use crate::schema_cache::lookup_schema;
use pgrx::prelude::*;

/// Convert `j` (a JSON object) to a FlatBuffer rooted at the table
/// named in `table_name` (same `[<schema>:]<table>` shape as
/// [`super::flatbuffers_verify`]).
///
/// The schema-cache lookup uses the schema portion (default
/// `"default"`); the root-table portion must match the schema's
/// registered `root_table` or we ERROR.
///
/// JSON nesting is capped at `pg_flatbuffers.max_depth` (reused
/// from the verifier `Bounds`; default 64). The
/// `pg_flatbuffers.from_json_unknown = ignore` GUC envisioned in
/// design §8 is *not* wired in v0.1 — unknown JSON keys always
/// ERROR.
///
/// Returns SQL `NULL` only when `j` itself is NULL (the
/// `STRICT` short-circuit). An empty JSON object `{}` builds a
/// valid empty FlatBuffer rooted at the named table (provided
/// the table has no required fields).
///
/// `STABLE` because the result depends on the schema cache (which
/// can change across statements via `flatbuffers_schemas` writes)
/// but is stable *within* a single query execution. `STRICT` so
/// SQL `NULL` in either argument short-circuits to `NULL` without
/// invoking the function.
#[pg_extern(stable, parallel_safe, strict)]
fn flatbuffers_from_json(table_name: &str, j: pgrx::JsonB) -> Vec<u8> {
    build(table_name, j.0)
}

/// Text-input variant. Parses the JSON itself rather than
/// receiving a pre-parsed `jsonb`. Semantics are otherwise
/// identical to [`flatbuffers_from_json`]; raises `ERROR` on
/// invalid JSON syntax.
#[pg_extern(stable, parallel_safe, strict)]
fn flatbuffers_from_json_text(table_name: &str, j: &str) -> Vec<u8> {
    let value: serde_json::Value = match serde_json::from_str(j) {
        Ok(v) => v,
        Err(e) => error!("flatbuffers_from_json_text: invalid JSON: {e}"),
    };
    build(table_name, value)
}

/// Shared dispatch: resolve schema, verify root-table match,
/// invoke the walker. Diverges via `error!` on any failure (mirrors
/// the to_json pattern).
fn build(table_name: &str, value: serde_json::Value) -> Vec<u8> {
    let (schema_name, expected_root) = split_schema_and_table(table_name).unwrap_or_else(|msg| {
        error!("flatbuffers_from_json: invalid table_name {table_name:?}: {msg}")
    });

    let cached = lookup_schema(schema_name);
    if cached.root_table != expected_root {
        error!(
            "flatbuffers_from_json: schema {schema_name:?} is registered with root \
             table {:?}, not {expected_root:?}",
            cached.root_table
        );
    }
    let schema_view = cached.schema();

    // JSON nesting cap reuses the verifier `max_depth` bound. The
    // design's `max_build_depth` is a separate concept (pre-walk
    // before allocation) — wiring it as a distinct GUC is a
    // follow-up.
    let max_depth = current_bounds().max_depth;

    match json_to_buf(&value, &schema_view, max_depth) {
        Ok(bytes) => bytes,
        Err(e) => error!("flatbuffers_from_json: {e}"),
    }
}
