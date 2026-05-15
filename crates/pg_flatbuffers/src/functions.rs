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
//! - [`crate::query::execute`] for the leaf-walking executor.
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
//! - GUC plumbing for [`crate::verify::Bounds`]: today every call
//!   uses [`crate::verify::Bounds::default`]. The GUC slice will
//!   read `pg_flatbuffers.max_depth` / `max_tables` /
//!   `max_apparent_size_mb` and pass an explicit `Bounds` here.
//! - The `pg_flatbuffers.strict` GUC, which would let
//!   [`flatbuffers_query`] return `NULL` instead of `ERROR` on a
//!   structural verifier failure (§10 "strict does not relax
//!   bounds"). Today we always `ERROR`, matching `strict = on`.
//! - Verifier result caching (design §10) — only useful when many
//!   query invocations share a buffer in one statement; we'll add
//!   it once usage shows it matters.

use crate::query::{execute, parse};
use crate::schema_cache::lookup_schema;
use crate::verify::{verify, Bounds};
use pgrx::iter::SetOfIterator;
use pgrx::prelude::*;

/// Schema name used when a query string omits the `schema:` prefix.
/// Matches `docs/design.md` §4.3.
const DEFAULT_SCHEMA: &str = "default";

// ---------------------------------------------------------------------------
// flatbuffers_query
// ---------------------------------------------------------------------------

/// Run `query` against `buf` and return the first leaf as `text`, or
/// `NULL` when the leaf is absent (the executor produced `None`),
/// when the executor produced *no* leaves (e.g. `[*]` over an absent
/// vector, see [`crate::query::execute`]), or when `buf` itself is
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
/// Verifier failure raises `ERROR` (matches `pg_flatbuffers.strict =
/// on`, the only mode supported today; the GUC slice will add the
/// `off` branch that returns `NULL` for *structural* failures while
/// still erroring on bound exceedances — see
/// [`crate::verify::VerifyError::is_bound_exceedance`]).
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

    let leaves = match execute(buf, &schema_view, &parsed, &Bounds::default()) {
        Ok(v) => v,
        Err(e) => error!("flatbuffers_query: {e}"),
    };

    // First leaf, regardless of present/absent. For paths without
    // `Step::All` the Vec has length 1; for `[*]` over an empty /
    // absent vector the Vec is empty and we fall through to `None`.
    leaves.into_iter().next().flatten()
}

// ---------------------------------------------------------------------------
// flatbuffers_query_array
// ---------------------------------------------------------------------------

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

    match execute(buf, &schema_view, &parsed, &Bounds::default()) {
        Ok(v) => v.into_iter().flatten().collect(),
        Err(e) => error!("flatbuffers_query_array: {e}"),
    }
}

// ---------------------------------------------------------------------------
// flatbuffers_query_multi
// ---------------------------------------------------------------------------

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

    let leaves = match execute(buf, &schema_view, &parsed, &Bounds::default()) {
        Ok(v) => v,
        Err(e) => error!("flatbuffers_query_multi: {e}"),
    };

    SetOfIterator::new(leaves.into_iter().flatten())
}

// ---------------------------------------------------------------------------
// flatbuffers_verify
// ---------------------------------------------------------------------------

/// Verify that `buf` parses as `table_name` under the current bounds.
///
/// `table_name` accepts the same `[<schema>:]<table>` shape as the
/// query mini-language minus the path; `:` and a path component is a
/// hard error (use [`flatbuffers_query`] for path traversal).
///
/// Suitable for `CHECK` constraints — never raises on a *buffer*
/// problem, only on caller-error problems (malformed `table_name`,
/// unknown schema):
///
/// | Situation                                | Result   |
/// | ---------------------------------------- | -------- |
/// | `buf` is empty                           | `false`  |
/// | `table_name` is empty / malformed        | `ERROR`  |
/// | Schema not registered                    | `ERROR`  |
/// | Schema's registered root table ≠ `table` | `false`  |
/// | Verifier rejects `buf`                   | `false`  |
/// | Verifier accepts `buf`                   | `true`   |
///
/// Note that bound exceedances (depth / table-count / apparent-size)
/// are reported as `false` rather than `ERROR` here, because the
/// boolean contract is what `CHECK` needs. Operators who want hard
/// failures on bound exceedances should use [`flatbuffers_query`]
/// inside the constraint instead.
#[pg_extern(stable, parallel_safe, strict)]
fn flatbuffers_verify(table_name: &str, buf: &[u8]) -> bool {
    if buf.is_empty() {
        return false;
    }

    let (schema_name, expected_root) = split_schema_and_table(table_name).unwrap_or_else(|msg| {
        error!("flatbuffers_verify: invalid table_name {table_name:?}: {msg}")
    });

    let cached = lookup_schema(schema_name);
    if cached.root_table != expected_root {
        return false;
    }

    verify(buf, &cached.schema(), &Bounds::default()).is_ok()
}

// ---------------------------------------------------------------------------
// flatbuffers_root_type
// ---------------------------------------------------------------------------

/// Return the registered root-table name for `schema_name`. Raises
/// `ERROR` when the schema is not registered (same contract as
/// [`crate::schema_cache::lookup_schema`]). Provided as a diagnostic
/// helper so operators can confirm a schema is loaded without dumping
/// the `bfbs` blob.
#[pg_extern(stable, parallel_safe, strict)]
fn flatbuffers_root_type(schema_name: &str) -> String {
    lookup_schema(schema_name).root_table.clone()
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Parse a `[<schema>:]<table>` shape into its parts. `<table>` may
/// itself contain dots (FlatBuffers namespaces) but never `:`. The
/// query mini-language's full parser is overkill here because there
/// is no path component to handle.
fn split_schema_and_table(input: &str) -> Result<(&str, &str), &'static str> {
    if input.is_empty() {
        return Err("table_name is empty");
    }
    match input.split_once(':') {
        None => Ok((DEFAULT_SCHEMA, input)),
        Some((schema, table)) => {
            if schema.is_empty() {
                return Err("schema name is empty (before the ':')");
            }
            if table.is_empty() {
                return Err("table name is empty (after the ':')");
            }
            if table.contains(':') {
                // Catches three-component inputs like
                // `default:Foo:bar` — guard against operators
                // accidentally pasting a full query string.
                return Err("expected `[schema:]table`, found extra `:`");
            }
            Ok((schema, table))
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod unit_tests {
    use super::*;

    #[test]
    fn split_no_colon_uses_default_schema() {
        assert_eq!(
            split_schema_and_table("Order").unwrap(),
            ("default", "Order")
        );
    }

    #[test]
    fn split_with_schema() {
        assert_eq!(
            split_schema_and_table("myco:Order").unwrap(),
            ("myco", "Order")
        );
    }

    #[test]
    fn split_namespaced_table_no_schema_prefix() {
        // `myco.orders.Order` is a fully-qualified FB table name
        // (dots, not colons) — no schema prefix.
        assert_eq!(
            split_schema_and_table("myco.orders.Order").unwrap(),
            ("default", "myco.orders.Order"),
        );
    }

    #[test]
    fn split_rejects_empty_input() {
        assert!(split_schema_and_table("").is_err());
    }

    #[test]
    fn split_rejects_empty_schema() {
        assert!(split_schema_and_table(":Order").is_err());
    }

    #[test]
    fn split_rejects_empty_table() {
        assert!(split_schema_and_table("myco:").is_err());
    }

    #[test]
    fn split_rejects_three_components() {
        // Likely a user pasted a full `flatbuffers_query` argument.
        assert!(split_schema_and_table("default:Order:id").is_err());
    }
}

#[cfg(any(test, feature = "pg_test"))]
#[pg_schema]
mod tests {
    use pgrx::prelude::*;

    // -- fixtures (built inline so this test module has no shared
    //    state with the executor's pure-Rust fixtures) --

    /// Build a trivial schema with one table `T { n: int = 0; }` and
    /// `T` as the root. Returns the `.bfbs` bytes.
    fn build_t_schema_bfbs() -> Vec<u8> {
        use flatbuffers::FlatBufferBuilder;
        use flatbuffers_reflection::reflection::{
            BaseType, Enum, Field, FieldArgs, Object, ObjectArgs, Schema, SchemaArgs, Type,
            TypeArgs,
        };

        let mut fbb = FlatBufferBuilder::new();
        let int_t = Type::create(
            &mut fbb,
            &TypeArgs {
                base_type: BaseType::Int,
                ..Default::default()
            },
        );
        let n_name = fbb.create_string("n");
        let n_field = Field::create(
            &mut fbb,
            &FieldArgs {
                name: Some(n_name),
                type_: Some(int_t),
                id: 0,
                offset: 4,
                ..Default::default()
            },
        );
        let fields = fbb.create_vector(&[n_field]);
        let t_name = fbb.create_string("T");
        let t_obj = Object::create(
            &mut fbb,
            &ObjectArgs {
                name: Some(t_name),
                fields: Some(fields),
                ..Default::default()
            },
        );
        let objects = fbb.create_vector(&[t_obj]);
        let enums = fbb.create_vector::<flatbuffers::ForwardsUOffset<Enum>>(&[]);
        let schema = Schema::create(
            &mut fbb,
            &SchemaArgs {
                objects: Some(objects),
                enums: Some(enums),
                root_table: Some(t_obj),
                ..Default::default()
            },
        );
        fbb.finish(schema, None);
        fbb.finished_data().to_vec()
    }

    /// Build a `T { n: <value> }` buffer. When `value` equals the
    /// schema default (0), the field is *elided* — used to exercise
    /// the absent-scalar default path through the SQL layer.
    fn build_t_buf(value: i32) -> Vec<u8> {
        use flatbuffers::FlatBufferBuilder;
        let mut fbb = FlatBufferBuilder::new();
        let t = fbb.start_table();
        fbb.push_slot::<i32>(4, value, 0);
        let off = fbb.end_table(t);
        fbb.finish_minimal(off);
        fbb.finished_data().to_vec()
    }

    /// Build a `B { tags: [string]; }` schema rooted at `B`. Used by
    /// the `flatbuffers_query_array` tests to exercise `Step::All`
    /// fanout end-to-end through SQL.
    fn build_b_schema_bfbs() -> Vec<u8> {
        use flatbuffers::FlatBufferBuilder;
        use flatbuffers_reflection::reflection::{
            BaseType, Enum, Field, FieldArgs, Object, ObjectArgs, Schema, SchemaArgs, Type,
            TypeArgs,
        };

        let mut fbb = FlatBufferBuilder::new();
        let tags_t = Type::create(
            &mut fbb,
            &TypeArgs {
                base_type: BaseType::Vector,
                element: BaseType::String,
                ..Default::default()
            },
        );
        let tags_name = fbb.create_string("tags");
        let tags_field = Field::create(
            &mut fbb,
            &FieldArgs {
                name: Some(tags_name),
                type_: Some(tags_t),
                id: 0,
                offset: 4,
                ..Default::default()
            },
        );
        let fields = fbb.create_vector(&[tags_field]);
        let b_name = fbb.create_string("B");
        let b_obj = Object::create(
            &mut fbb,
            &ObjectArgs {
                name: Some(b_name),
                fields: Some(fields),
                ..Default::default()
            },
        );
        let objects = fbb.create_vector(&[b_obj]);
        let enums = fbb.create_vector::<flatbuffers::ForwardsUOffset<Enum>>(&[]);
        let schema = Schema::create(
            &mut fbb,
            &SchemaArgs {
                objects: Some(objects),
                enums: Some(enums),
                root_table: Some(b_obj),
                ..Default::default()
            },
        );
        fbb.finish(schema, None);
        fbb.finished_data().to_vec()
    }

    /// Build a `B { tags: <tags?> }` buffer. `None` elides the
    /// vector; `Some(&[])` emits an empty vector.
    fn build_b_buf(tags: Option<&[&str]>) -> Vec<u8> {
        use flatbuffers::FlatBufferBuilder;
        let mut fbb = FlatBufferBuilder::new();
        let tags_off = tags.map(|ts| {
            let offs: Vec<_> = ts.iter().map(|s| fbb.create_string(s)).collect();
            fbb.create_vector(&offs)
        });
        let t = fbb.start_table();
        if let Some(off) = tags_off {
            fbb.push_slot_always(4, off);
        }
        let b = fbb.end_table(t);
        fbb.finish_minimal(b);
        fbb.finished_data().to_vec()
    }

    /// Register `bfbs` as schema `name` rooted at `root_table` via
    /// SPI. Tests run as superuser (pgrx-tests default), so the role
    /// check on `flatbuffers_schemas` is bypassed.
    fn register(name: &str, root_table: &str, bfbs: Vec<u8>) {
        Spi::run_with_args(
            "INSERT INTO flatbuffers_schemas (name, bfbs, root_table) \
             VALUES ($1, $2, $3)",
            &[
                name.to_string().into(),
                bfbs.into(),
                root_table.to_string().into(),
            ],
        )
        .expect("SPI insert");
    }

    // -- flatbuffers_query --

    /// `STRICT` short-circuits NULL inputs without touching the
    /// function body. Verifies pgrx wired the attribute correctly.
    #[pg_test]
    fn pg_query_null_buf_returns_null() {
        let v = Spi::get_one::<String>("SELECT flatbuffers_query('T:n', NULL::bytea)")
            .expect("SPI failure");
        assert!(v.is_none(), "expected NULL, got {v:?}");
    }

    /// Empty `bytea` is the "absent payload" sentinel from §10 — must
    /// be NULL, not an `ERROR` from the verifier (which would otherwise
    /// reject zero-length input).
    #[pg_test]
    fn pg_query_empty_buf_returns_null() {
        let v = Spi::get_one::<String>("SELECT flatbuffers_query('T:n', ''::bytea)")
            .expect("SPI failure");
        assert!(v.is_none(), "expected NULL, got {v:?}");
    }

    #[pg_test]
    fn pg_query_happy_path_scalar() {
        register("default", "T", build_t_schema_bfbs());
        let buf = build_t_buf(42);
        let v =
            Spi::get_one_with_args::<String>("SELECT flatbuffers_query('T:n', $1)", &[buf.into()])
                .expect("SPI failure");
        assert_eq!(v.as_deref(), Some("42"));
    }

    /// Absent scalar (value == default == 0) round-trips through the
    /// executor's `scalar_default_string` path and back to SQL as the
    /// schema default.
    #[pg_test]
    fn pg_query_absent_scalar_returns_default() {
        register("default", "T", build_t_schema_bfbs());
        let buf = build_t_buf(0); // elided slot
        let v =
            Spi::get_one_with_args::<String>("SELECT flatbuffers_query('T:n', $1)", &[buf.into()])
                .expect("SPI failure");
        assert_eq!(v.as_deref(), Some("0"));
    }

    /// Garbage bytes must reach the verifier and ERROR (default
    /// `strict = on` semantics). The doubled "buffer rejected by
    /// verifier" prefix comes from `ExecuteError::Verify`'s
    /// Display layered on `VerifyError::Invalid`'s Display; the
    /// `Range [50462976, 50462980)` substring is the verifier's
    /// deterministic decoding of `\x00010203` as a root offset of
    /// `0x03020100 = 50462976`. pgrx-tests does exact equality and
    /// appends `\n\n` for the backtrace separator.
    #[pg_test(
        error = "flatbuffers_query: buffer rejected by verifier: buffer rejected by FlatBuffers verifier: Range [50462976, 50462980) is out of bounds.\n\n"
    )]
    fn pg_query_garbage_buf_errors() {
        register("default", "T", build_t_schema_bfbs());
        Spi::get_one::<String>("SELECT flatbuffers_query('T:n', '\\x00010203'::bytea)")
            .expect("SPI failure");
    }

    /// Unknown schema name surfaces the `lookup_schema` ERROR
    /// verbatim (so operators can grep for typos).
    #[pg_test(error = "flatbuffers schema \"nope\" is not registered")]
    fn pg_query_unknown_schema_errors() {
        Spi::get_one_with_args::<String>(
            "SELECT flatbuffers_query('nope:T:n', $1)",
            &[build_t_buf(1).into()],
        )
        .expect("SPI failure");
    }

    // -- flatbuffers_query_array --

    /// `STRICT` short-circuits NULL inputs.
    #[pg_test]
    fn pg_query_array_null_buf_returns_null() {
        // Result is the SQL NULL array, *not* an empty array — STRICT
        // bypasses the function body entirely. (`text[]` arrives back
        // as `Vec<Option<String>>` so the outer Option is the
        // array-level NULL.)
        let v = Spi::get_one::<Vec<Option<String>>>(
            "SELECT flatbuffers_query_array('B:tags[*]', NULL::bytea)",
        )
        .expect("SPI failure");
        assert!(v.is_none(), "expected NULL, got {v:?}");
    }

    /// Empty `bytea` short-circuits to the empty array (not NULL,
    /// not ERROR). Mirrors §10's "absent payload" sentinel; downstream
    /// SQL using `array_length(... , 1)` will see `NULL` for the
    /// "no matches" cardinality.
    #[pg_test]
    fn pg_query_array_empty_buf_returns_empty_array() {
        let v = Spi::get_one::<Vec<Option<String>>>(
            "SELECT flatbuffers_query_array('B:tags[*]', ''::bytea)",
        )
        .expect("SPI failure")
        .expect("empty bytea must produce empty array, not NULL");
        assert!(v.is_empty(), "got {v:?}");
    }

    /// Happy-path fanout: three tags → a three-element `text[]`,
    /// preserving wire order.
    #[pg_test]
    fn pg_query_array_happy_path_strings() {
        register("default", "B", build_b_schema_bfbs());
        let buf = build_b_buf(Some(&["red", "green", "blue"]));
        let v = Spi::get_one_with_args::<Vec<Option<String>>>(
            "SELECT flatbuffers_query_array('B:tags[*]', $1)",
            &[buf.into()],
        )
        .expect("SPI failure")
        .expect("NULL from happy path");
        assert_eq!(
            v,
            vec![
                Some("red".to_owned()),
                Some("green".to_owned()),
                Some("blue".to_owned()),
            ],
        );
    }

    /// Absent vector under `[*]` must be the empty array (no items
    /// to fan out over) — distinct from `[i]` which would yield a
    /// one-element NULL array.
    #[pg_test]
    fn pg_query_array_absent_vector_is_empty_array() {
        register("default", "B", build_b_schema_bfbs());
        let buf = build_b_buf(None);
        let v = Spi::get_one_with_args::<Vec<Option<String>>>(
            "SELECT flatbuffers_query_array('B:tags[*]', $1)",
            &[buf.into()],
        )
        .expect("SPI failure")
        .expect("absent vector must be empty array, not NULL");
        assert!(v.is_empty(), "got {v:?}");
    }

    /// `flatbuffers_query` returning the *first* leaf — we now
    /// share the executor result with `flatbuffers_query_array`, so
    /// pin the contract that `_query` against `[*]` collapses to the
    /// first element.
    #[pg_test]
    fn pg_query_first_leaf_under_all() {
        register("default", "B", build_b_schema_bfbs());
        let buf = build_b_buf(Some(&["alpha", "beta"]));
        let v = Spi::get_one_with_args::<String>(
            "SELECT flatbuffers_query('B:tags[*]', $1)",
            &[buf.into()],
        )
        .expect("SPI failure");
        assert_eq!(v.as_deref(), Some("alpha"));
    }

    // -- flatbuffers_query_multi --

    /// `STRICT` short-circuits NULL inputs \u2014 zero rows, never the
    /// function body.
    #[pg_test]
    fn pg_query_multi_null_buf_returns_zero_rows() {
        let n = Spi::get_one::<i64>(
            "SELECT count(*) FROM flatbuffers_query_multi('B:tags[*]', NULL::bytea)",
        )
        .expect("SPI failure")
        .expect("count is non-null");
        assert_eq!(n, 0);
    }

    /// Empty `bytea` short-circuits to zero rows (mirrors the empty
    /// array `flatbuffers_query_array` returns; just a different
    /// shape).
    #[pg_test]
    fn pg_query_multi_empty_buf_returns_zero_rows() {
        let n = Spi::get_one::<i64>(
            "SELECT count(*) FROM flatbuffers_query_multi('B:tags[*]', ''::bytea)",
        )
        .expect("SPI failure")
        .expect("count is non-null");
        assert_eq!(n, 0);
    }

    /// Happy-path fanout: three tags \u2192 three rows in wire-format
    /// order. `array_agg` round-trips so we can pin the order
    /// assertion in one [`Spi::get_one`] call without needing a
    /// cursor.
    #[pg_test]
    fn pg_query_multi_happy_path_strings() {
        register("default", "B", build_b_schema_bfbs());
        let buf = build_b_buf(Some(&["red", "green", "blue"]));
        let v = Spi::get_one_with_args::<Vec<Option<String>>>(
            "SELECT array_agg(t ORDER BY ord) \
             FROM flatbuffers_query_multi('B:tags[*]', $1) \
                 WITH ORDINALITY AS s(t, ord)",
            &[buf.into()],
        )
        .expect("SPI failure")
        .expect("NULL from happy path");
        assert_eq!(
            v,
            vec![
                Some("red".to_owned()),
                Some("green".to_owned()),
                Some("blue".to_owned()),
            ],
        );
    }

    /// Absent vector under `[*]` \u2192 zero rows.
    #[pg_test]
    fn pg_query_multi_absent_vector_is_zero_rows() {
        register("default", "B", build_b_schema_bfbs());
        let buf = build_b_buf(None);
        let n = Spi::get_one_with_args::<i64>(
            "SELECT count(*) FROM flatbuffers_query_multi('B:tags[*]', $1)",
            &[buf.into()],
        )
        .expect("SPI failure")
        .expect("count is non-null");
        assert_eq!(n, 0);
    }

    /// `LATERAL` against a single-row source: the SETOF wrapper is
    /// the whole reason this function exists, so pin a tiny
    /// representative shape.
    #[pg_test]
    fn pg_query_multi_lateral_join() {
        register("default", "B", build_b_schema_bfbs());
        let buf = build_b_buf(Some(&["a", "b"]));
        let v = Spi::get_one_with_args::<Vec<Option<String>>>(
            "SELECT array_agg(tag) \
             FROM (SELECT $1::bytea AS payload) src, \
                  LATERAL flatbuffers_query_multi('B:tags[*]', src.payload) AS tag",
            &[buf.into()],
        )
        .expect("SPI failure")
        .expect("NULL from happy path");
        assert_eq!(v, vec![Some("a".to_owned()), Some("b".to_owned())]);
    }

    // -- flatbuffers_verify --

    #[pg_test]
    fn pg_verify_empty_buf_is_false() {
        register("default", "T", build_t_schema_bfbs());
        let v = Spi::get_one::<bool>("SELECT flatbuffers_verify('T', ''::bytea)")
            .expect("SPI failure")
            .expect("NULL from non-strict-on-empty path");
        assert!(!v);
    }

    #[pg_test]
    fn pg_verify_happy_path_is_true() {
        register("default", "T", build_t_schema_bfbs());
        let buf = build_t_buf(7);
        let v = Spi::get_one_with_args::<bool>("SELECT flatbuffers_verify('T', $1)", &[buf.into()])
            .expect("SPI failure")
            .expect("NULL from happy path");
        assert!(v);
    }

    /// A schema-prefixed table name must agree with the registered
    /// root table.
    #[pg_test]
    fn pg_verify_root_table_mismatch_is_false() {
        register("default", "T", build_t_schema_bfbs());
        let buf = build_t_buf(7);
        let v =
            Spi::get_one_with_args::<bool>("SELECT flatbuffers_verify('NotT', $1)", &[buf.into()])
                .expect("SPI failure")
                .expect("NULL");
        assert!(!v);
    }

    #[pg_test]
    fn pg_verify_garbage_buf_is_false() {
        register("default", "T", build_t_schema_bfbs());
        let v = Spi::get_one::<bool>("SELECT flatbuffers_verify('T', '\\x00010203'::bytea)")
            .expect("SPI failure")
            .expect("NULL");
        assert!(!v);
    }

    /// Malformed `table_name` (three components — common copy/paste
    /// of a full `flatbuffers_query` argument string) is a *user*
    /// error and must `ERROR`. A single-colon input like `"T:n"` is
    /// *not* malformed: it parses as `(schema=T, table=n)`, which
    /// then surfaces as an unknown-schema error from the cache —
    /// covered by [`pg_query_unknown_schema_errors`].
    #[pg_test(
        error = "flatbuffers_verify: invalid table_name \"default:T:n\": expected `[schema:]table`, found extra `:`"
    )]
    fn pg_verify_malformed_table_name_errors() {
        register("default", "T", build_t_schema_bfbs());
        Spi::get_one_with_args::<bool>(
            "SELECT flatbuffers_verify('default:T:n', $1)",
            &[build_t_buf(0).into()],
        )
        .expect("SPI failure");
    }

    // -- flatbuffers_root_type --

    #[pg_test]
    fn pg_root_type_returns_registered_value() {
        register("default", "T", build_t_schema_bfbs());
        let v = Spi::get_one::<String>("SELECT flatbuffers_root_type('default')")
            .expect("SPI failure")
            .expect("NULL from registered schema");
        assert_eq!(v, "T");
    }

    #[pg_test(error = "flatbuffers schema \"missing\" is not registered")]
    fn pg_root_type_unknown_schema_errors() {
        Spi::get_one::<String>("SELECT flatbuffers_root_type('missing')").expect("SPI failure");
    }
}
