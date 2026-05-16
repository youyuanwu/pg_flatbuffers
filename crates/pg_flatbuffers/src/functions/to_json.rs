//! `flatbuffers_to_json` — convert a FlatBuffer to its JSON
//! representation via the reflection schema (design §8).
//!
//! Always raises `ERROR` on a verifier failure (including bound
//! exceedances and schema-feature rejections), per design §8:
//! "JSON conversion functions always raise ERROR regardless of
//! `strict`, since a partially-valid document has no defensible
//! JSON encoding". The `strict` GUC is therefore not consulted
//! here.

use super::util::split_schema_and_table;
use crate::guc::current_bounds;
use crate::schema_cache::lookup_schema;
use crate::to_json::buf_to_json;
use crate::verify::verify;
use pgrx::prelude::*;

/// Convert `buf` (a FlatBuffer whose root is the table named in
/// `table_name`) to `jsonb` using `flatc --strict-json` encoding
/// conventions (see [`crate::to_json`] for the per-type table).
///
/// `table_name` uses the same `[<schema>:]<table>` shape as
/// [`super::flatbuffers_verify`]: the schema-cache lookup uses the
/// schema portion (default `"default"`), and the root-table portion
/// must match the schema's registered `root_table` or we ERROR
/// (the caller asked for one table but the schema is registered as
/// another — a config mistake worth surfacing loudly).
///
/// Returns SQL `NULL` when `buf` is empty (the standard "absent
/// payload" contract, §10).
///
/// `STABLE` because the result depends on the schema cache (which
/// can change across statements via `flatbuffers_schemas` writes)
/// but is stable *within* a single query execution. `STRICT` so
/// SQL `NULL` in either argument short-circuits to `NULL` without
/// invoking the function.
#[pg_extern(stable, parallel_safe, strict)]
fn flatbuffers_to_json(table_name: &str, buf: &[u8]) -> Option<pgrx::JsonB> {
    if buf.is_empty() {
        return None;
    }

    let (schema_name, expected_root) = split_schema_and_table(table_name).unwrap_or_else(|msg| {
        error!("flatbuffers_to_json: invalid table_name {table_name:?}: {msg}")
    });

    let cached = lookup_schema(schema_name);
    if cached.root_table != expected_root {
        error!(
            "flatbuffers_to_json: schema {schema_name:?} is registered with root \
             table {:?}, not {expected_root:?}",
            cached.root_table
        );
    }

    let schema_view = cached.schema();

    // Verifier failure → ERROR (§8 contract: JSON conversion never
    // returns NULL for malformed buffers). We deliberately *do not*
    // consult `strict` here.
    if let Err(e) = verify(buf, &schema_view, &current_bounds()) {
        error!("flatbuffers_to_json: {e}");
    }

    match buf_to_json(buf, &schema_view) {
        Ok(value) => Some(pgrx::JsonB(value)),
        Err(e) => error!("flatbuffers_to_json: {e}"),
    }
}

/// Text-returning variant. Renders via `serde_json::to_string`
/// (compact, no pretty-printing — matches `flatc --strict-json`
/// output). Useful when the caller wants the raw text without
/// Postgres re-parsing through `jsonb`.
///
/// All other semantics (verifier behaviour, NULL on empty, ERROR
/// on root-table mismatch) are identical to
/// [`flatbuffers_to_json`].
#[pg_extern(stable, parallel_safe, strict)]
fn flatbuffers_to_json_text(table_name: &str, buf: &[u8]) -> Option<String> {
    flatbuffers_to_json(table_name, buf).map(|j| j.0.to_string())
}
