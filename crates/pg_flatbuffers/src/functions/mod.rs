//! SQL-exposed query / verify / introspection functions
//! (see `docs/design.md` §4.2).
//!
//! This module is the thin pgrx layer that turns SQL calls into the
//! pure-Rust building blocks already in place:
//!
//! - [`crate::query::parse`] for the path mini-language,
//! - [`crate::schema_cache::lookup_schema`] for the registered
//!   `.bfbs` blob,
//! - [`crate::verify::verify`] for bounded reflection-driven
//!   verification, and
//! - [`crate::query::execute_with_options`] for the leaf-walking executor.
//!
//! ## Scope of this slice
//!
//! All v0.1 query / verify / introspection entry points whose
//! backing logic exists today:
//!
//! - [`flatbuffers_query`] — single leaf, `text`.
//! - [`flatbuffers_query_array`] — fanout / multi-leaf, `text[]`.
//! - [`flatbuffers_query_multi`] — fanout / multi-leaf, `SETOF text`,
//!   suitable for `LATERAL` joins and `WITH ORDINALITY`.
//! - [`flatbuffers_verify`] — boolean, suitable for `CHECK`.
//! - [`flatbuffers_root_type`] — diagnostic helper returning the
//!   registered root-table name.
//!
//! ### Deliberately deferred (each gets its own micro-slice)
//!
//! - `flatbuffers_to_json{,_text}` and
//!   `flatbuffers_from_json{,_text}` — live in a future
//!   `json.rs` slice.
//! - GUC plumbing for [`crate::verify::Bounds`]: each entry point
//!   calls [`crate::guc::current_bounds`], which materialises a
//!   [`crate::verify::Bounds`] from the three `SUSET` GUCs
//!   (`pg_flatbuffers.max_depth`, `max_tables`,
//!   `max_apparent_size_mb`). Registration lives in [`crate::guc::init`].
//! - GUC plumbing for [`crate::guc::current_strict`]
//!   (`pg_flatbuffers.strict`, `USERSET`, default `on`): each query
//!   entry point that today raises ERROR on a verifier failure
//!   instead substitutes the per-shape "no leaves" sentinel when
//!   `strict = off`, *except* for bound exceedances which always
//!   ERROR (§10 "strict does not relax bounds"). The classification
//!   uses [`crate::verify::VerifyError::is_bound_exceedance`].
//! - Verifier result caching (design §10) — only useful when many
//!   query invocations share a buffer in one statement; we'll add
//!   it once usage shows it matters.

/// Schema name used when a query string omits the `schema:` prefix.
/// Matches `docs/design.md` §4.3.
pub(super) const DEFAULT_SCHEMA: &str = "default";

mod query;
mod query_array;
mod query_multi;
mod root_type;
mod util;
mod verify;

// `#[pg_schema]` requires an inline `mod { ... }` body (it's a
// proc-macro that rewrites the module token tree), so we can't use
// `mod tests;`. Each per-area file is `include!`d as a chunk of
// code pasted into the inline body, keeping the test surface split
// across files while pgrx-tests still sees a single `tests` mod
// (it issues `SELECT tests.<fn>()` to drive each `#[pg_test]`).
//
// `preamble.rs` brings `use pgrx::prelude::*;` into scope for the
// whole module; `fixtures.rs` defines the schema / buffer builders
// and the `register` SPI helper shared by every entry-point file.
#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
mod tests {
    include!("tests/preamble.rs");
    include!("tests/fixtures.rs");
    include!("tests/query.rs");
    include!("tests/query_array.rs");
    include!("tests/query_multi.rs");
    include!("tests/verify.rs");
    include!("tests/root_type.rs");
    include!("tests/guc.rs");
    include!("tests/vector64.rs");
}
