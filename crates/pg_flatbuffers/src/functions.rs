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

use crate::guc::{current_bounds, current_fill_scalar_defaults, current_strict};
use crate::query::{execute_with_options, parse, ExecuteError, ExecuteOptions};
use crate::schema_cache::lookup_schema;
use crate::verify::verify;
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

    verify(buf, &cached.schema(), &current_bounds()).is_ok()
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

/// Classify an [`ExecuteError`] under the current
/// `pg_flatbuffers.strict` setting. Returns `true` when the call
/// site should raise ERROR; `false` when it should substitute the
/// per-shape "no leaves" sentinel.
///
/// Rules (design §10 "strict does not relax bounds"):
///
/// - `strict = on` → always ERROR.
/// - `strict = off` and a verifier *bound* exceedance (depth, table
///   count, apparent size) → still ERROR; `USERSET` cannot weaken a
///   `SUSET`-protected bound.
/// - `strict = off` and a verifier *structural* failure (malformed
///   bytes, missing required field, etc.) → no ERROR; substitute.
/// - `strict = off` and any *non-verifier* failure (`FieldNotFound`,
///   `UnsupportedType`, `UnsupportedStep`, `Internal`) → still
///   ERROR. These are *caller* / *schema* problems, not buffer
///   problems, so silencing them would mask bugs.
fn should_error_on(strict: bool, e: &ExecuteError) -> bool {
    if strict {
        return true;
    }
    match e {
        ExecuteError::Verify(v) => v.is_bound_exceedance(),
        _ => true,
    }
}

/// Apply [`should_error_on`] and either raise Postgres ERROR with a
/// caller-prefixed message, or return the empty `Vec<Option<String>>`
/// that the call site reshapes into its public no-match sentinel
/// (`NULL` / `text[] = '{}'` / zero rows). [`error!`] diverges, so
/// the function appears to "return" `Vec::new()` only on the
/// substitute branch.
fn resolve_execute_error(fn_name: &str, e: ExecuteError, strict: bool) -> Vec<Option<String>> {
    if should_error_on(strict, &e) {
        error!("{fn_name}: {e}");
    }
    Vec::new()
}

/// Materialise an [`ExecuteOptions`] from the current per-session
/// USERSET GUC values. Called once per public SQL entry point so a
/// `SET pg_flatbuffers.fill_scalar_defaults = ...` takes effect on
/// the very next call in the same session.
fn current_execute_options() -> ExecuteOptions {
    ExecuteOptions {
        fill_scalar_defaults: current_fill_scalar_defaults(),
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

    // -- should_error_on classifier --

    use crate::query::ExecuteError;
    use crate::verify::VerifyError;

    fn structural_verify_err() -> ExecuteError {
        ExecuteError::Verify(VerifyError::Invalid(
            "Range [0, 4) is out of bounds".to_owned(),
        ))
    }

    fn bound_verify_err() -> ExecuteError {
        ExecuteError::Verify(VerifyError::Invalid("depth 65 exceeds limit 64".to_owned()))
    }

    #[test]
    fn should_error_on_strict_on_structural_is_true() {
        assert!(should_error_on(true, &structural_verify_err()));
    }

    #[test]
    fn should_error_on_strict_on_bound_is_true() {
        assert!(should_error_on(true, &bound_verify_err()));
    }

    #[test]
    fn should_error_on_strict_off_structural_is_false() {
        // strict = off swallows a structural verifier failure into
        // the substitute path.
        assert!(!should_error_on(false, &structural_verify_err()));
    }

    #[test]
    fn should_error_on_strict_off_bound_still_errors() {
        // §10: strict does not relax bounds — depth / tables /
        // apparent size always ERROR even with strict = off.
        assert!(should_error_on(false, &bound_verify_err()));
    }

    #[test]
    fn should_error_on_strict_off_non_verify_still_errors() {
        // Caller / schema errors are not silenced by strict = off.
        let e = ExecuteError::FieldNotFound {
            what: "missing".to_owned(),
            table: "T".to_owned(),
        };
        assert!(should_error_on(false, &e));
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

    /// Build a `Catalog { entries: [Entry]; }` schema where
    /// `Entry { name: string (key); }` exists specifically so the
    /// `flatbuffers_query` SQL tests can exercise `Step::MapKey`.
    /// Object index 0 is `Catalog`, 1 is `Entry` (alphabetical).
    fn build_catalog_schema_bfbs() -> Vec<u8> {
        use flatbuffers::FlatBufferBuilder;
        use flatbuffers_reflection::reflection::{
            BaseType, Enum, Field, FieldArgs, Object, ObjectArgs, Schema, SchemaArgs, Type,
            TypeArgs,
        };

        let mut fbb = FlatBufferBuilder::new();

        // Entry.name: string (key)
        let str_t = Type::create(
            &mut fbb,
            &TypeArgs {
                base_type: BaseType::String,
                ..Default::default()
            },
        );
        let name_n = fbb.create_string("name");
        let name_f = Field::create(
            &mut fbb,
            &FieldArgs {
                name: Some(name_n),
                type_: Some(str_t),
                id: 0,
                offset: 4,
                key: true,
                ..Default::default()
            },
        );
        let entry_fields = fbb.create_vector(&[name_f]);
        let entry_n = fbb.create_string("Entry");
        let entry = Object::create(
            &mut fbb,
            &ObjectArgs {
                name: Some(entry_n),
                fields: Some(entry_fields),
                ..Default::default()
            },
        );

        // Catalog.entries: [Entry]
        let vec_entry_t = Type::create(
            &mut fbb,
            &TypeArgs {
                base_type: BaseType::Vector,
                element: BaseType::Obj,
                index: 1, // Entry's index in the sorted objects vector
                ..Default::default()
            },
        );
        let entries_n = fbb.create_string("entries");
        let entries_f = Field::create(
            &mut fbb,
            &FieldArgs {
                name: Some(entries_n),
                type_: Some(vec_entry_t),
                id: 0,
                offset: 4,
                ..Default::default()
            },
        );
        let cat_fields = fbb.create_vector(&[entries_f]);
        let cat_n = fbb.create_string("Catalog");
        let catalog = Object::create(
            &mut fbb,
            &ObjectArgs {
                name: Some(cat_n),
                fields: Some(cat_fields),
                ..Default::default()
            },
        );

        // Sort by name: Catalog (0), Entry (1).
        let objects = fbb.create_vector(&[catalog, entry]);
        let enums = fbb.create_vector::<flatbuffers::ForwardsUOffset<Enum>>(&[]);
        let schema = Schema::create(
            &mut fbb,
            &SchemaArgs {
                objects: Some(objects),
                enums: Some(enums),
                root_table: Some(catalog),
                ..Default::default()
            },
        );
        fbb.finish(schema, None);
        fbb.finished_data().to_vec()
    }

    /// Build a `Catalog` buffer with one `Entry` per supplied name.
    fn build_catalog_buf(names: &[&str]) -> Vec<u8> {
        use flatbuffers::FlatBufferBuilder;
        let mut fbb = FlatBufferBuilder::new();
        let entry_offs: Vec<_> = names
            .iter()
            .map(|n| {
                let name_off = fbb.create_string(n);
                let t = fbb.start_table();
                fbb.push_slot_always(4, name_off);
                fbb.end_table(t)
            })
            .collect();
        let entries_off = fbb.create_vector(&entry_offs);
        let t = fbb.start_table();
        fbb.push_slot_always(4, entries_off);
        let cat = fbb.end_table(t);
        fbb.finish_minimal(cat);
        fbb.finished_data().to_vec()
    }

    /// Build a minimal struct-descent schema for the SQL smoke:
    ///
    /// ```text
    /// struct Vec3 { x:float; y:float; z:float; }   // bytesize 12, align 4
    /// table  Point { name:string; pos:Vec3; }      // pos at vtable slot 6
    /// ```
    ///
    /// Object indices after alphabetical sort: `Point` = 0, `Vec3` = 1.
    /// Full unit coverage of struct descent (nested structs, scalar
    /// leaves by id, absent / index / keys rejection) lives in the
    /// executor module; this fixture exists purely to exercise the
    /// SQL surface end-to-end.
    fn build_point_schema_bfbs() -> Vec<u8> {
        use flatbuffers::FlatBufferBuilder;
        use flatbuffers_reflection::reflection::{
            BaseType, Enum, Field as RField, FieldArgs, Object as RObject, ObjectArgs,
            Schema as RSchema, SchemaArgs, Type, TypeArgs,
        };
        let mut fbb = FlatBufferBuilder::new();

        let float_t = Type::create(
            &mut fbb,
            &TypeArgs {
                base_type: BaseType::Float,
                ..Default::default()
            },
        );
        let str_t = Type::create(
            &mut fbb,
            &TypeArgs {
                base_type: BaseType::String,
                ..Default::default()
            },
        );

        // -- struct Vec3 --
        let vec3_x_n = fbb.create_string("x");
        let vec3_x = RField::create(
            &mut fbb,
            &FieldArgs {
                name: Some(vec3_x_n),
                type_: Some(float_t),
                id: 0,
                offset: 0,
                ..Default::default()
            },
        );
        let vec3_y_n = fbb.create_string("y");
        let vec3_y = RField::create(
            &mut fbb,
            &FieldArgs {
                name: Some(vec3_y_n),
                type_: Some(float_t),
                id: 1,
                offset: 4,
                ..Default::default()
            },
        );
        let vec3_z_n = fbb.create_string("z");
        let vec3_z = RField::create(
            &mut fbb,
            &FieldArgs {
                name: Some(vec3_z_n),
                type_: Some(float_t),
                id: 2,
                offset: 8,
                ..Default::default()
            },
        );
        let vec3_fields = fbb.create_vector(&[vec3_x, vec3_y, vec3_z]);
        let vec3_n = fbb.create_string("Vec3");
        let vec3 = RObject::create(
            &mut fbb,
            &ObjectArgs {
                name: Some(vec3_n),
                fields: Some(vec3_fields),
                is_struct: true,
                bytesize: 12,
                minalign: 4,
                ..Default::default()
            },
        );

        // -- table Point { name:string @ slot 4; pos:Vec3 @ slot 6 } --
        // Vec3 sorts as object index 1.
        let vec3_obj_t = Type::create(
            &mut fbb,
            &TypeArgs {
                base_type: BaseType::Obj,
                index: 1,
                ..Default::default()
            },
        );
        let point_name_n = fbb.create_string("name");
        let point_name = RField::create(
            &mut fbb,
            &FieldArgs {
                name: Some(point_name_n),
                type_: Some(str_t),
                id: 0,
                offset: 4,
                ..Default::default()
            },
        );
        let point_pos_n = fbb.create_string("pos");
        let point_pos = RField::create(
            &mut fbb,
            &FieldArgs {
                name: Some(point_pos_n),
                type_: Some(vec3_obj_t),
                id: 1,
                offset: 6,
                ..Default::default()
            },
        );
        let point_fields = fbb.create_vector(&[point_name, point_pos]);
        let point_n = fbb.create_string("Point");
        let point = RObject::create(
            &mut fbb,
            &ObjectArgs {
                name: Some(point_n),
                fields: Some(point_fields),
                ..Default::default()
            },
        );

        // Sorted alphabetically: Point (0), Vec3 (1).
        let objects = fbb.create_vector(&[point, vec3]);
        let enums = fbb.create_vector::<flatbuffers::ForwardsUOffset<Enum>>(&[]);
        let schema = RSchema::create(
            &mut fbb,
            &SchemaArgs {
                objects: Some(objects),
                enums: Some(enums),
                root_table: Some(point),
                ..Default::default()
            },
        );
        fbb.finish(schema, None);
        fbb.finished_data().to_vec()
    }

    /// `Vec3` mirror used to push the inline struct into a `Point`
    /// table. `repr(C, packed)` matches the FlatBuffers wire layout
    /// for structs (no compiler padding).
    #[repr(C, packed)]
    #[derive(Clone, Copy)]
    struct TestVec3 {
        x: f32,
        y: f32,
        z: f32,
    }

    // SAFETY: `repr(C, packed)` makes the in-memory bytes match the
    // on-wire little-endian layout on every supported host.
    impl flatbuffers::Push for TestVec3 {
        type Output = TestVec3;
        unsafe fn push(&self, dst: &mut [u8], _written_len: usize) {
            let src = std::slice::from_raw_parts(
                self as *const Self as *const u8,
                std::mem::size_of::<Self>(),
            );
            dst[..src.len()].copy_from_slice(src);
        }
    }

    /// Build a `Point` buffer with the supplied name and pos.
    fn build_point_buf(name: &str, pos: TestVec3) -> Vec<u8> {
        use flatbuffers::FlatBufferBuilder;
        let mut fbb = FlatBufferBuilder::new();
        let name_off = fbb.create_string(name);
        let t = fbb.start_table();
        fbb.push_slot_always(4, name_off); // slot 4 = name
        fbb.push_slot_always(6, pos); // slot 6 = pos (inline struct)
        let p = fbb.end_table(t);
        fbb.finish_minimal(p);
        fbb.finished_data().to_vec()
    }

    /// Build a vector-of-struct schema for the SQL-side smoke.
    /// Mirror of the executor's `build_bag_schema` fixture: a
    /// `table Bag { points: [Vec3] }` over inline `Vec3` structs.
    /// Object ordering: Bag (0), Vec3 (1).
    fn build_bag_schema_bfbs() -> Vec<u8> {
        use flatbuffers::FlatBufferBuilder;
        use flatbuffers_reflection::reflection::{
            BaseType, Enum, Field as RField, FieldArgs, Object as RObject, ObjectArgs,
            Schema as RSchema, SchemaArgs, Type, TypeArgs,
        };
        let mut fbb = FlatBufferBuilder::new();

        let float_t = Type::create(
            &mut fbb,
            &TypeArgs {
                base_type: BaseType::Float,
                ..Default::default()
            },
        );

        // -- struct Vec3 --
        let x_n = fbb.create_string("x");
        let x_f = RField::create(
            &mut fbb,
            &FieldArgs {
                name: Some(x_n),
                type_: Some(float_t),
                id: 0,
                offset: 0,
                ..Default::default()
            },
        );
        let y_n = fbb.create_string("y");
        let y_f = RField::create(
            &mut fbb,
            &FieldArgs {
                name: Some(y_n),
                type_: Some(float_t),
                id: 1,
                offset: 4,
                ..Default::default()
            },
        );
        let z_n = fbb.create_string("z");
        let z_f = RField::create(
            &mut fbb,
            &FieldArgs {
                name: Some(z_n),
                type_: Some(float_t),
                id: 2,
                offset: 8,
                ..Default::default()
            },
        );
        let vec3_fields = fbb.create_vector(&[x_f, y_f, z_f]);
        let vec3_n = fbb.create_string("Vec3");
        let vec3 = RObject::create(
            &mut fbb,
            &ObjectArgs {
                name: Some(vec3_n),
                fields: Some(vec3_fields),
                is_struct: true,
                bytesize: 12,
                minalign: 4,
                ..Default::default()
            },
        );

        // -- table Bag { points: [Vec3] } --
        // Vec3 sorts as object index 1 (Bag (0), Vec3 (1)).
        let points_t = Type::create(
            &mut fbb,
            &TypeArgs {
                base_type: BaseType::Vector,
                element: BaseType::Obj,
                index: 1,
                ..Default::default()
            },
        );
        let points_n = fbb.create_string("points");
        let points_f = RField::create(
            &mut fbb,
            &FieldArgs {
                name: Some(points_n),
                type_: Some(points_t),
                id: 0,
                offset: 4, // vtable slot for the only field
                ..Default::default()
            },
        );
        let bag_fields = fbb.create_vector(&[points_f]);
        let bag_n = fbb.create_string("Bag");
        let bag = RObject::create(
            &mut fbb,
            &ObjectArgs {
                name: Some(bag_n),
                fields: Some(bag_fields),
                ..Default::default()
            },
        );

        let objects = fbb.create_vector(&[bag, vec3]);
        let enums = fbb.create_vector::<flatbuffers::ForwardsUOffset<Enum>>(&[]);
        let schema = RSchema::create(
            &mut fbb,
            &SchemaArgs {
                objects: Some(objects),
                enums: Some(enums),
                root_table: Some(bag),
                ..Default::default()
            },
        );
        fbb.finish(schema, None);
        fbb.finished_data().to_vec()
    }

    /// Build a `Bag` buffer holding the supplied vector of inline
    /// `Vec3` structs.
    fn build_bag_buf(points: &[TestVec3]) -> Vec<u8> {
        use flatbuffers::FlatBufferBuilder;
        let mut fbb = FlatBufferBuilder::new();
        let points_off = fbb.create_vector(points);
        let t = fbb.start_table();
        fbb.push_slot_always(4, points_off); // slot 4 = points
        let bag = fbb.end_table(t);
        fbb.finish_minimal(bag);
        fbb.finished_data().to_vec()
    }

    /// Build a fixed-size-array schema for the SQL-side smoke.
    /// Mirror of the executor's `build_holder_schema` fixture:
    ///
    /// ```text
    /// struct Vec3   { x:float; y:float; z:float; }   // 12 bytes, align 4
    /// struct Bundle {
    ///   xs:  [float:3];   // offset 0,  size 12 (scalar elements)
    ///   pts: [Vec3:2];    // offset 12, size 24 (struct elements)
    /// }                                              // bytesize 36
    /// table Holder { b:Bundle; }
    /// ```
    ///
    /// Object ordering: Bundle (0), Holder (1), Vec3 (2).
    fn build_holder_schema_bfbs() -> Vec<u8> {
        use flatbuffers::FlatBufferBuilder;
        use flatbuffers_reflection::reflection::{
            BaseType, Enum, Field as RField, FieldArgs, Object as RObject, ObjectArgs,
            Schema as RSchema, SchemaArgs, Type, TypeArgs,
        };
        let mut fbb = FlatBufferBuilder::new();

        let float_t = Type::create(
            &mut fbb,
            &TypeArgs {
                base_type: BaseType::Float,
                ..Default::default()
            },
        );

        // -- struct Vec3 --
        let vx_n = fbb.create_string("x");
        let vx = RField::create(
            &mut fbb,
            &FieldArgs {
                name: Some(vx_n),
                type_: Some(float_t),
                id: 0,
                offset: 0,
                ..Default::default()
            },
        );
        let vy_n = fbb.create_string("y");
        let vy = RField::create(
            &mut fbb,
            &FieldArgs {
                name: Some(vy_n),
                type_: Some(float_t),
                id: 1,
                offset: 4,
                ..Default::default()
            },
        );
        let vz_n = fbb.create_string("z");
        let vz = RField::create(
            &mut fbb,
            &FieldArgs {
                name: Some(vz_n),
                type_: Some(float_t),
                id: 2,
                offset: 8,
                ..Default::default()
            },
        );
        let vec3_fields = fbb.create_vector(&[vx, vy, vz]);
        let vec3_n = fbb.create_string("Vec3");
        let vec3 = RObject::create(
            &mut fbb,
            &ObjectArgs {
                name: Some(vec3_n),
                fields: Some(vec3_fields),
                is_struct: true,
                bytesize: 12,
                minalign: 4,
                ..Default::default()
            },
        );

        // -- struct Bundle { xs:[float:3] @0; pts:[Vec3:2] @12; } --
        // Vec3 sorts as object index 2.
        let xs_t = Type::create(
            &mut fbb,
            &TypeArgs {
                base_type: BaseType::Array,
                element: BaseType::Float,
                fixed_length: 3,
                ..Default::default()
            },
        );
        let pts_t = Type::create(
            &mut fbb,
            &TypeArgs {
                base_type: BaseType::Array,
                element: BaseType::Obj,
                index: 2,
                fixed_length: 2,
                ..Default::default()
            },
        );
        let pts_n = fbb.create_string("pts");
        let pts_f = RField::create(
            &mut fbb,
            &FieldArgs {
                name: Some(pts_n),
                type_: Some(pts_t),
                id: 1,
                offset: 12,
                ..Default::default()
            },
        );
        let xs_n = fbb.create_string("xs");
        let xs_f = RField::create(
            &mut fbb,
            &FieldArgs {
                name: Some(xs_n),
                type_: Some(xs_t),
                id: 0,
                offset: 0,
                ..Default::default()
            },
        );
        // Field vector sorted alphabetically: pts (p) < xs (x).
        let bundle_fields = fbb.create_vector(&[pts_f, xs_f]);
        let bundle_n = fbb.create_string("Bundle");
        let bundle = RObject::create(
            &mut fbb,
            &ObjectArgs {
                name: Some(bundle_n),
                fields: Some(bundle_fields),
                is_struct: true,
                bytesize: 36,
                minalign: 4,
                ..Default::default()
            },
        );

        // -- table Holder { b: Bundle } --
        // Bundle sorts as object index 0.
        let bundle_obj_t = Type::create(
            &mut fbb,
            &TypeArgs {
                base_type: BaseType::Obj,
                index: 0,
                ..Default::default()
            },
        );
        let b_n = fbb.create_string("b");
        let b_f = RField::create(
            &mut fbb,
            &FieldArgs {
                name: Some(b_n),
                type_: Some(bundle_obj_t),
                id: 0,
                offset: 4,
                ..Default::default()
            },
        );
        let holder_fields = fbb.create_vector(&[b_f]);
        let holder_n = fbb.create_string("Holder");
        let holder = RObject::create(
            &mut fbb,
            &ObjectArgs {
                name: Some(holder_n),
                fields: Some(holder_fields),
                ..Default::default()
            },
        );

        let objects = fbb.create_vector(&[bundle, holder, vec3]);
        let enums = fbb.create_vector::<flatbuffers::ForwardsUOffset<Enum>>(&[]);
        let schema = RSchema::create(
            &mut fbb,
            &SchemaArgs {
                objects: Some(objects),
                enums: Some(enums),
                root_table: Some(holder),
                ..Default::default()
            },
        );
        fbb.finish(schema, None);
        fbb.finished_data().to_vec()
    }

    /// Mirror of the `Bundle` struct above.
    #[repr(C, packed)]
    #[derive(Clone, Copy)]
    struct TestBundle {
        xs: [f32; 3],
        pts: [TestVec3; 2],
    }

    // SAFETY: see [`TestVec3`].
    impl flatbuffers::Push for TestBundle {
        type Output = TestBundle;
        unsafe fn push(&self, dst: &mut [u8], _written_len: usize) {
            let src = std::slice::from_raw_parts(
                self as *const Self as *const u8,
                std::mem::size_of::<Self>(),
            );
            dst[..src.len()].copy_from_slice(src);
        }
    }

    /// Build a `Holder` buffer containing the supplied `Bundle`.
    fn build_holder_buf(b: TestBundle) -> Vec<u8> {
        use flatbuffers::FlatBufferBuilder;
        let mut fbb = FlatBufferBuilder::new();
        let t = fbb.start_table();
        fbb.push_slot_always(4, b); // slot 4 = b (inline struct)
        let h = fbb.end_table(t);
        fbb.finish_minimal(h);
        fbb.finished_data().to_vec()
    }

    /// Build a tiny union schema for the SQL-side smoke. Same shape
    /// as the executor's `build_union_schema` fixture (table A with
    /// `name:string`, table B with `count:int`, union U over both,
    /// table Msg holding `body:U`). Object ordering: A (0), B (1),
    /// Msg (2). Enum index for U: 0. Full coverage of dispatch /
    /// NONE / not-in-variant / step-shape rejection lives in the
    /// executor module; this fixture only exercises the SQL surface.
    fn build_msg_schema_bfbs() -> Vec<u8> {
        use flatbuffers::FlatBufferBuilder;
        use flatbuffers_reflection::reflection::{
            BaseType, Enum, EnumArgs, EnumVal, EnumValArgs, Field as RField, FieldArgs,
            Object as RObject, ObjectArgs, Schema as RSchema, SchemaArgs, Type, TypeArgs,
        };
        let mut fbb = FlatBufferBuilder::new();

        let int_t = Type::create(
            &mut fbb,
            &TypeArgs {
                base_type: BaseType::Int,
                ..Default::default()
            },
        );
        let str_t = Type::create(
            &mut fbb,
            &TypeArgs {
                base_type: BaseType::String,
                ..Default::default()
            },
        );

        // table A { name:string @ slot 4; }  -> object index 0
        let a_name_n = fbb.create_string("name");
        let a_name = RField::create(
            &mut fbb,
            &FieldArgs {
                name: Some(a_name_n),
                type_: Some(str_t),
                id: 0,
                offset: 4,
                ..Default::default()
            },
        );
        let a_fields = fbb.create_vector(&[a_name]);
        let a_n = fbb.create_string("A");
        let a = RObject::create(
            &mut fbb,
            &ObjectArgs {
                name: Some(a_n),
                fields: Some(a_fields),
                ..Default::default()
            },
        );

        // table B { count:int @ slot 4; }  -> object index 1
        let b_count_n = fbb.create_string("count");
        let b_count = RField::create(
            &mut fbb,
            &FieldArgs {
                name: Some(b_count_n),
                type_: Some(int_t),
                id: 0,
                offset: 4,
                ..Default::default()
            },
        );
        let b_fields = fbb.create_vector(&[b_count]);
        let b_n = fbb.create_string("B");
        let b = RObject::create(
            &mut fbb,
            &ObjectArgs {
                name: Some(b_n),
                fields: Some(b_fields),
                ..Default::default()
            },
        );

        // enum U { NONE=0, A=1, B=2 } as a union (enum index 0).
        let none_t = Type::create(
            &mut fbb,
            &TypeArgs {
                base_type: BaseType::None,
                ..Default::default()
            },
        );
        let none_n = fbb.create_string("NONE");
        let none_ev = EnumVal::create(
            &mut fbb,
            &EnumValArgs {
                name: Some(none_n),
                value: 0,
                union_type: Some(none_t),
                ..Default::default()
            },
        );
        let a_obj_t = Type::create(
            &mut fbb,
            &TypeArgs {
                base_type: BaseType::Obj,
                index: 0,
                ..Default::default()
            },
        );
        let a_variant_n = fbb.create_string("A");
        let a_ev = EnumVal::create(
            &mut fbb,
            &EnumValArgs {
                name: Some(a_variant_n),
                value: 1,
                union_type: Some(a_obj_t),
                ..Default::default()
            },
        );
        let b_obj_t = Type::create(
            &mut fbb,
            &TypeArgs {
                base_type: BaseType::Obj,
                index: 1,
                ..Default::default()
            },
        );
        let b_variant_n = fbb.create_string("B");
        let b_ev = EnumVal::create(
            &mut fbb,
            &EnumValArgs {
                name: Some(b_variant_n),
                value: 2,
                union_type: Some(b_obj_t),
                ..Default::default()
            },
        );
        let u_values = fbb.create_vector(&[none_ev, a_ev, b_ev]);
        let u_underlying = Type::create(
            &mut fbb,
            &TypeArgs {
                base_type: BaseType::UType,
                index: 0,
                ..Default::default()
            },
        );
        let u_n = fbb.create_string("U");
        let u_enum = Enum::create(
            &mut fbb,
            &EnumArgs {
                name: Some(u_n),
                values: Some(u_values),
                is_union: true,
                underlying_type: Some(u_underlying),
                ..Default::default()
            },
        );

        // table Msg { body:U; }  body_type @ slot 4, body @ slot 6.
        let body_type_n = fbb.create_string("body_type");
        let body_utype_t = Type::create(
            &mut fbb,
            &TypeArgs {
                base_type: BaseType::UType,
                index: 0,
                ..Default::default()
            },
        );
        let body_type_f = RField::create(
            &mut fbb,
            &FieldArgs {
                name: Some(body_type_n),
                type_: Some(body_utype_t),
                id: 0,
                offset: 4,
                ..Default::default()
            },
        );
        let body_n = fbb.create_string("body");
        let body_union_t = Type::create(
            &mut fbb,
            &TypeArgs {
                base_type: BaseType::Union,
                index: 0,
                ..Default::default()
            },
        );
        let body_f = RField::create(
            &mut fbb,
            &FieldArgs {
                name: Some(body_n),
                type_: Some(body_union_t),
                id: 1,
                offset: 6,
                ..Default::default()
            },
        );
        // Sorted: "body" < "body_type".
        let msg_fields = fbb.create_vector(&[body_f, body_type_f]);
        let msg_n = fbb.create_string("Msg");
        let msg = RObject::create(
            &mut fbb,
            &ObjectArgs {
                name: Some(msg_n),
                fields: Some(msg_fields),
                ..Default::default()
            },
        );

        let objects = fbb.create_vector(&[a, b, msg]);
        let enums = fbb.create_vector(&[u_enum]);
        let schema = RSchema::create(
            &mut fbb,
            &SchemaArgs {
                objects: Some(objects),
                enums: Some(enums),
                root_table: Some(msg),
                ..Default::default()
            },
        );
        fbb.finish(schema, None);
        fbb.finished_data().to_vec()
    }

    /// Build a `Msg` buffer carrying a `TableA { name }` variant.
    /// Discriminator at slot 4 = 1, value sub-table offset at slot 6.
    fn build_msg_buf_a(name: &str) -> Vec<u8> {
        use flatbuffers::FlatBufferBuilder;
        let mut fbb = FlatBufferBuilder::new();
        let name_off = fbb.create_string(name);
        // Build TableA first so its offset is known before Msg opens.
        let t = fbb.start_table();
        fbb.push_slot_always(4, name_off);
        let a_off = fbb.end_table(t);
        // Msg.
        let t = fbb.start_table();
        fbb.push_slot::<u8>(4, 1, 0); // body_type = A
        fbb.push_slot_always(6, a_off); // body value
        let m = fbb.end_table(t);
        fbb.finish_minimal(m);
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
    /// schema default (today's `fill_scalar_defaults = on` behaviour).
    #[pg_test]
    fn pg_query_absent_scalar_returns_default() {
        register("default", "T", build_t_schema_bfbs());
        let buf = build_t_buf(0); // elided slot
        let v =
            Spi::get_one_with_args::<String>("SELECT flatbuffers_query('T:n', $1)", &[buf.into()])
                .expect("SPI failure");
        assert_eq!(v.as_deref(), Some("0"));
    }

    /// `SET pg_flatbuffers.fill_scalar_defaults = off` flips the
    /// absent-scalar branch from "schema default" to SQL NULL
    /// (design §4.3 / §10). Same fixture as
    /// `pg_query_absent_scalar_returns_default`; only the GUC
    /// changes.
    #[pg_test]
    fn pg_query_absent_scalar_returns_null_when_fill_off() {
        register("default", "T", build_t_schema_bfbs());
        let buf = build_t_buf(0); // elided slot
        Spi::run("SET pg_flatbuffers.fill_scalar_defaults = off")
            .expect("SPI: SET fill_scalar_defaults");
        let v =
            Spi::get_one_with_args::<String>("SELECT flatbuffers_query('T:n', $1)", &[buf.into()])
                .expect("SPI failure");
        assert!(v.is_none(), "expected NULL under fill_off, got {v:?}");
    }

    /// `fill_scalar_defaults = off` must not affect *present*
    /// scalars — including the `n = 42` value from
    /// `pg_query_happy_path_scalar`. Pins that the GUC gates only
    /// the absent branch.
    #[pg_test]
    fn pg_query_present_scalar_unaffected_by_fill_off() {
        register("default", "T", build_t_schema_bfbs());
        let buf = build_t_buf(42); // slot emitted (42 != default 0)
        Spi::run("SET pg_flatbuffers.fill_scalar_defaults = off")
            .expect("SPI: SET fill_scalar_defaults");
        let v =
            Spi::get_one_with_args::<String>("SELECT flatbuffers_query('T:n', $1)", &[buf.into()])
                .expect("SPI failure");
        assert_eq!(v.as_deref(), Some("42"));
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

    /// `strict = off` turns a *structural* verifier failure into a
    /// NULL result for the scalar entry point (design §10).
    #[pg_test]
    fn pg_query_strict_off_garbage_returns_null() {
        register("default", "T", build_t_schema_bfbs());
        Spi::run("SET pg_flatbuffers.strict = off").expect("SET");
        let v = Spi::get_one::<String>("SELECT flatbuffers_query('T:n', '\\x00010203'::bytea)")
            .expect("SPI failure");
        assert!(v.is_none(), "expected NULL under strict=off, got {v:?}");
    }

    /// `strict = off` turns a *structural* verifier failure into an
    /// empty `text[]` for the array entry point.
    #[pg_test]
    fn pg_query_array_strict_off_garbage_returns_empty_array() {
        register("default", "T", build_t_schema_bfbs());
        Spi::run("SET pg_flatbuffers.strict = off").expect("SET");
        let v = Spi::get_one::<Vec<Option<String>>>(
            "SELECT flatbuffers_query_array('T:n', '\\x00010203'::bytea)",
        )
        .expect("SPI failure");
        assert_eq!(
            v.as_deref(),
            Some(&[][..]),
            "expected empty text[] under strict=off, got {v:?}",
        );
    }

    /// `strict = off` turns a *structural* verifier failure into a
    /// zero-row setof for the multi entry point.
    #[pg_test]
    fn pg_query_multi_strict_off_garbage_returns_zero_rows() {
        register("default", "T", build_t_schema_bfbs());
        Spi::run("SET pg_flatbuffers.strict = off").expect("SET");
        let n = Spi::get_one::<i64>(
            "SELECT count(*) FROM flatbuffers_query_multi('T:n', '\\x00010203'::bytea)",
        )
        .expect("SPI failure");
        assert_eq!(n, Some(0));
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

    /// `Step::MapKey` end-to-end: lookup `Catalog:entries[beta].name`
    /// against a `Catalog` whose `Entry.name` is the `(key)` field.
    /// This is the SQL-side smoke for the executor's
    /// `walk_vector_at_map_key` (full unit coverage of hits, misses,
    /// empty / absent vectors, etc. lives in the executor module).
    #[pg_test]
    fn pg_query_map_key_hit() {
        register("default", "Catalog", build_catalog_schema_bfbs());
        let buf = build_catalog_buf(&["alpha", "beta", "gamma"]);
        let v = Spi::get_one_with_args::<String>(
            "SELECT flatbuffers_query('Catalog:entries[beta].name', $1)",
            &[buf.into()],
        )
        .expect("SPI failure");
        assert_eq!(v.as_deref(), Some("beta"));
    }

    /// Map-key miss surfaces as SQL `NULL` rather than `ERROR` —
    /// matches the OOB-index short-circuit so a typo on the SQL
    /// side doesn't abort the statement.
    #[pg_test]
    fn pg_query_map_key_miss_returns_null() {
        register("default", "Catalog", build_catalog_schema_bfbs());
        let buf = build_catalog_buf(&["alpha", "gamma"]);
        let v = Spi::get_one_with_args::<String>(
            "SELECT flatbuffers_query('Catalog:entries[zzz].name', $1)",
            &[buf.into()],
        )
        .expect("SPI failure");
        assert!(v.is_none(), "got {v:?}");
    }

    /// `field|keys` enumerates the `(key)`-annotated field of every
    /// entry in wire order. Surfaces as a `text[]` via
    /// `flatbuffers_query_array`; full unit coverage (duplicates,
    /// empty / absent vectors, scalar-vector rejection) lives in
    /// the executor module.
    #[pg_test]
    fn pg_query_map_keys_array_returns_keys_in_wire_order() {
        register("default", "Catalog", build_catalog_schema_bfbs());
        let buf = build_catalog_buf(&["alpha", "beta", "gamma"]);
        let v = Spi::get_one_with_args::<Vec<Option<String>>>(
            "SELECT flatbuffers_query_array('Catalog:entries|keys', $1)",
            &[buf.into()],
        )
        .expect("SPI failure")
        .expect("non-NULL result");
        assert_eq!(
            v,
            vec![
                Some("alpha".to_owned()),
                Some("beta".to_owned()),
                Some("gamma".to_owned()),
            ],
        );
    }

    /// Same query through `flatbuffers_query_multi` (SETOF text)
    /// yields one row per key in the same wire-format order.
    #[pg_test]
    fn pg_query_map_keys_multi_yields_one_row_per_key() {
        register("default", "Catalog", build_catalog_schema_bfbs());
        let buf = build_catalog_buf(&["alpha", "beta", "gamma"]);
        let v = Spi::get_one_with_args::<Vec<Option<String>>>(
            "SELECT array_agg(t ORDER BY ord) \
             FROM flatbuffers_query_multi('Catalog:entries|keys', $1) \
                 WITH ORDINALITY AS s(t, ord)",
            &[buf.into()],
        )
        .expect("SPI failure")
        .expect("non-NULL result");
        assert_eq!(
            v,
            vec![
                Some("alpha".to_owned()),
                Some("beta".to_owned()),
                Some("gamma".to_owned()),
            ],
        );
    }

    // -- struct descent (design §7.2 / §4.3) --

    /// SQL-side smoke for `walk_struct`: descend into an inline
    /// struct field and stringify a scalar leaf. Full unit coverage
    /// of nested-struct descent, absence handling, and step-shape
    /// rejection lives in the executor module.
    #[pg_test]
    fn pg_query_struct_field_scalar_leaf() {
        register("default", "Point", build_point_schema_bfbs());
        let buf = build_point_buf(
            "p",
            TestVec3 {
                x: 1.5,
                y: 2.5,
                z: 3.5,
            },
        );
        let v = Spi::get_one_with_args::<String>(
            "SELECT flatbuffers_query('Point:pos.y', $1)",
            &[buf.into()],
        )
        .expect("SPI failure");
        assert_eq!(v.as_deref(), Some("2.5"));
    }

    // -- union dispatch (design §7.2 / §4.3) --

    /// SQL-side smoke for `walk_union`: auto-dispatch through the
    /// `body_type` discriminator and read a string scalar from the
    /// active variant table. Full coverage of NONE handling,
    /// not-in-variant FieldNotFound, the discriminator field
    /// scalar, and step-shape rejection lives in the executor
    /// module.
    #[pg_test]
    fn pg_query_union_dispatch_into_variant() {
        register("default", "Msg", build_msg_schema_bfbs());
        let buf = build_msg_buf_a("hello");
        let v = Spi::get_one_with_args::<String>(
            "SELECT flatbuffers_query('Msg:body.name', $1)",
            &[buf.into()],
        )
        .expect("SPI failure");
        assert_eq!(v.as_deref(), Some("hello"));
    }

    /// SQL-side smoke for the `|type` terminal: returns the active
    /// variant's symbolic name as a string. Full coverage of NONE
    /// handling, non-union rejection, and descent-past-leaf
    /// rejection lives in the executor module.
    #[pg_test]
    fn pg_query_union_type_returns_variant_name() {
        register("default", "Msg", build_msg_schema_bfbs());
        let buf = build_msg_buf_a("hello");
        let v = Spi::get_one_with_args::<String>(
            "SELECT flatbuffers_query('Msg:body|type', $1)",
            &[buf.into()],
        )
        .expect("SPI failure");
        assert_eq!(v.as_deref(), Some("A"));
    }

    // -- vector of struct (design §7.2 / §4.3) --

    /// SQL-side smoke for vector-of-struct dispatch: descend into
    /// the i'th inline struct element and stringify a scalar leaf.
    /// Full unit coverage of `[*]` fanout, OOB / absent handling,
    /// and step-shape rejection (struct elements have no `(key)`,
    /// and there's no v0.1 textual leaf form for a struct element)
    /// lives in the executor module.
    #[pg_test]
    fn pg_query_vector_of_struct_index_field() {
        register("default", "Bag", build_bag_schema_bfbs());
        let pts = [
            TestVec3 {
                x: 1.0,
                y: 2.0,
                z: 3.0,
            },
            TestVec3 {
                x: 4.0,
                y: 5.0,
                z: 6.0,
            },
            TestVec3 {
                x: 7.0,
                y: 8.0,
                z: 9.0,
            },
        ];
        let buf = build_bag_buf(&pts);
        // Element 1 is {4, 5, 6} — read its `y` (= 5).
        let v = Spi::get_one_with_args::<String>(
            "SELECT flatbuffers_query('Bag:points[1].y', $1)",
            &[buf.into()],
        )
        .expect("SPI failure");
        assert_eq!(v.as_deref(), Some("5"));
    }

    // -- fixed-size arrays inside structs (design §7.2 / §4.3) --

    /// SQL-side smoke for `walk_array`: descend into a fixed-size
    /// array of inline struct elements and stringify a scalar leaf.
    /// Full unit coverage of scalar-element arrays, `[*]` fanout,
    /// OOB / absent handling, and step-shape rejection lives in the
    /// executor module.
    #[pg_test]
    fn pg_query_array_of_struct_index_field() {
        register("default", "Holder", build_holder_schema_bfbs());
        let buf = build_holder_buf(TestBundle {
            xs: [1.0, 2.0, 3.0],
            pts: [
                TestVec3 {
                    x: 10.0,
                    y: 20.0,
                    z: 30.0,
                },
                TestVec3 {
                    x: 100.0,
                    y: 200.0,
                    z: 300.0,
                },
            ],
        });
        // Element 1 of the struct array is {100, 200, 300} — read y.
        let v = Spi::get_one_with_args::<String>(
            "SELECT flatbuffers_query('Holder:b.pts[1].y', $1)",
            &[buf.into()],
        )
        .expect("SPI failure");
        assert_eq!(v.as_deref(), Some("200"));
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

    // -- GUC plumbing (design §10) --

    /// End-to-end smoke that `SET pg_flatbuffers.max_tables` actually
    /// reaches the verifier on a subsequent SQL call. A `Catalog`
    /// with one `Entry` exposes two tables to the verifier, so a
    /// `max_tables = 1` cap rejects the payload — surfaced through
    /// `flatbuffers_verify`'s boolean contract as `false` (the
    /// function never raises on a buffer-level failure, per
    /// `flatbuffers_verify`'s docstring). The GUC's own
    /// SET / SHOW / range-guard coverage lives in `guc.rs`.
    #[pg_test]
    fn pg_guc_max_tables_plumbs_to_verifier() {
        register("default", "Catalog", build_catalog_schema_bfbs());
        let buf = build_catalog_buf(&["alpha"]);

        // Baseline: default `max_tables = 1_000_000` accepts the
        // 2-table buffer.
        let baseline = Spi::get_one_with_args::<bool>(
            "SELECT flatbuffers_verify('Catalog', $1)",
            &[buf.clone().into()],
        )
        .expect("SPI failure")
        .expect("NULL from happy path");
        assert!(baseline, "baseline verify must accept");

        // Constrained: `max_tables = 1` is below the buffer's
        // 2-table requirement, so the verifier rejects.
        Spi::run("SET pg_flatbuffers.max_tables = 1").expect("SPI: SET max_tables");
        let constrained = Spi::get_one_with_args::<bool>(
            "SELECT flatbuffers_verify('Catalog', $1)",
            &[buf.into()],
        )
        .expect("SPI failure")
        .expect("NULL from constrained path");
        assert!(
            !constrained,
            "max_tables = 1 must reject a 2-table Catalog buffer",
        );
    }
}
