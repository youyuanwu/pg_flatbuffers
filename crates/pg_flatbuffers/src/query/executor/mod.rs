//! Reflection-driven query executor (see `docs/design.md` §7.2).
//!
//! Consumes a parsed [`Query`] plus a registered [`Schema`] and a
//! caller-supplied buffer, and produces a list of leaves as
//! `Vec<Option<String>>`:
//!
//! - `Some(text)` for a present scalar / string leaf;
//! - `None` for a leaf that the path traversal short-circuited at
//!   (absent intermediate sub-table, absent string, absent vector
//!   under `[i]`, …);
//! - the list has length **1** for paths without `Step::All`,
//!   length **N** for one `Step::All` over a vector of length N
//!   (depth-first left-to-right for nested `[*]` per §7.2 step 3).
//!   An absent vector or empty vector under `Step::All` produces
//!   length **0**.
//!
//! The single-leaf wrappers (`flatbuffers_query`) take
//! `result.into_iter().next().flatten()`; the array wrapper
//! (`flatbuffers_query_array`) takes `result.into_iter().flatten()
//! .collect()` (skipping `None` per design §4.3 "absent values are
//! skipped").
//!
//! This module is the smallest viable executor: a starting point that
//! the next slices grow into the full design. It supports paths that
//! walk through nested tables and vectors to a scalar/string leaf:
//!
//! - `Step::Field` (by name and by `#id`).
//! - Descent into sub-tables (`BaseType::Obj` where the referenced
//!   `Object` is **not** a struct).
//! - Descent into structs (`BaseType::Obj` where the referenced
//!   `Object` **is** a struct — inline fixed-size record).
//!   Supports nested-struct fields, scalar leaves, and fixed-size
//!   arrays ([`BaseType::Array`], see below); rejects any non-`Field`
//!   step (structs hold no vectors, only scalars, nested structs,
//!   and fixed-size arrays).
//! - Union dispatch (`BaseType::Union`, design §4.3). Reads the
//!   `u8` discriminator from the vtable slot immediately preceding
//!   the union value (flatc convention: discriminator at slot N-2,
//!   value at slot N), looks up the matching `EnumVal` in the
//!   reflected enum, resolves the underlying variant `Object`, and
//!   recursively descends with the remaining steps. Discriminator
//!   value `0` (`NONE`) short-circuits to a single `None` leaf.
//!   Variant-table-only in v0.1; struct or string variants are
//!   rejected loudly. The auto-generated discriminator field
//!   (`<name>_type`, `BaseType::UType`) is queryable as a `u8`
//!   scalar leaf so callers can introspect the active variant.
//! - `Step::UnionType` (the `|type` terminal). Yields the *name* of
//!   the active union variant as a string leaf (e.g. `"TableA"`,
//!   `"NONE"`). Symmetric with the `<name>_type` UType scalar but
//!   enum-name rather than numeric. Absent / NONE returns the name
//!   of the value-0 variant (typically `"NONE"`), not SQL NULL.
//! - `Step::Index` for vector access (§7.2 step 2). Out-of-range
//!   indices short-circuit to a single `None` per design §4.3.
//! - `Step::All` for vector fanout (§7.2 step 3). Emits one entry
//!   per vector element in wire-format order. Supported element
//!   types: scalars, bool, string, tables (descend with the
//!   remaining steps; nested `[*]` accumulates depth-first), and
//!   inline structs (`field.type_().element() == Obj` where the
//!   referenced `Object::is_struct()`; element windows are sliced
//!   with the struct's `bytesize()` and handed to `walk_struct`).
//! - `Step::MapKey` for `(key)`-vector lookup (§7.2 step 4). Linear
//!   scan (the §10 `pg_flatbuffers.key_lookup_strict = off`
//!   fallback) until the verifier sortedness check lands and lets
//!   the strict path switch to binary search. String- and
//!   integer-keyed `Entry` tables supported.
//! - `Step::MapKeys` for fanning out the `(key)` field of every
//!   element of a `(key)`-annotated vector (§7.2 step 5). Like
//!   `[*]` but constrained to the keyed field; tail must be empty
//!   because the keys themselves are the leaves.
//! - Fixed-size arrays inside structs (`BaseType::Array`). Both
//!   `[i]` indexed access and `[*]` fanout descend through the N
//!   inline elements; element stride is the scalar size for scalar
//!   elements and the struct's `bytesize()` for struct elements.
//!   OOB index short-circuits to a `None` leaf (matches the vector
//!   arms). Struct-typed elements descend via [`walk_struct`].
//! - Stringification of scalar (int/uint/float/bool) and string leaves
//!   via [`flatbuffers_reflection::get_any_field_string`].
//!
//! Deliberately deferred to dedicated micro-slices, each returning a
//! clear [`ExecuteError::Unsupported*`] variant today:
//!
//! - Union variants other than table (struct or string variants).
//! - `BaseType::Vector64` — element-offset arithmetic uses 64-bit
//!   offsets and `flatbuffers::Vector<T>` only handles 32-bit;
//!   rejected loudly so we don't silently truncate addresses.
//! - Vectors of unions / vectors of vectors / vectors of arrays.
//! - The `pg_flatbuffers.fill_scalar_defaults` GUC (§10) — today
//!   scalar "absent" returns the schema default (matches the
//!   FlatBuffers reader API). When the GUC is wired and set to
//!   `off`, absent scalars surface as SQL NULL instead.
//!
//! Pure Rust; no `pgrx` dependency. The Postgres SQL wrappers live in
//! `functions.rs` (next slice).

use super::ast::Query;
use crate::verify::VerifyError;
use crate::verify::{Bounds, verify};
use flatbuffers_reflection::get_any_root;
use flatbuffers_reflection::reflection::Schema;
use thiserror::Error;

/// Read-side knobs threaded through every recursive walker. Bundled
/// into a struct so adding future per-session settings doesn't
/// re-churn every walker signature.
///
/// The defaults match the pre-GUC executor behaviour byte-for-byte
/// so the pure-Rust unit tests — which call [`execute_with_options`] directly
/// rather than going through the GUC layer — keep passing
/// unchanged.
#[derive(Debug, Clone, Copy)]
pub struct ExecuteOptions {
    /// When `true` (the design §10 default, matching the FlatBuffers
    /// reader API), an absent scalar table field reads back as its
    /// schema-declared default. When `false`, the field surfaces as
    /// SQL `NULL` so callers can distinguish presence from default
    /// (§4.3). The Postgres-side knob is
    /// `pg_flatbuffers.fill_scalar_defaults` — see
    /// [`crate::guc::current_fill_scalar_defaults`].
    pub fill_scalar_defaults: bool,

    /// When `true` (the design §10 default, matching the FlatBuffers
    /// reader API's `LookupByKey`), `field[key]` lookups bisect a
    /// `(key)`-annotated vector under the FlatBuffers contract that
    /// the vector is key-sorted. When `false`, falls back to a
    /// linear scan that is correct on any vector but O(n) (§7.2
    /// step 4). The Postgres-side knob is
    /// `pg_flatbuffers.key_lookup_strict` — see
    /// [`crate::guc::current_key_lookup_strict`].
    pub key_lookup_strict: bool,
}

impl Default for ExecuteOptions {
    fn default() -> Self {
        Self {
            fill_scalar_defaults: true,
            key_lookup_strict: true,
        }
    }
}

/// Errors produced by [`execute_with_options`].
#[derive(Error, Debug)]
pub enum ExecuteError {
    /// The buffer failed verification under the current bounds.
    /// Always raised before any field access; callers can match on
    /// this variant to apply `pg_flatbuffers.strict = off` semantics
    /// (substitute `NULL` instead of `ERROR`) — but only when
    /// `VerifyError::is_bound_exceedance()` is *false*, since
    /// "strict does not relax bounds" (design §10).
    #[error("buffer rejected by verifier: {0}")]
    Verify(#[from] VerifyError),

    /// A `Field` step did not match any field on the current table.
    /// `what` is the human-readable selector (the name string, or
    /// `"#<id>"` for the field-id form).
    #[error("field {what:?} not found on table {table:?}")]
    FieldNotFound { what: String, table: String },

    /// The path tries to descend through a field whose schema type
    /// the v0.1 executor does not yet handle (struct, vector, union,
    /// etc.). Distinct from [`ExecuteError::UnsupportedStep`] so that
    /// log scraping can tell *schema-shape* limitations apart from
    /// *query-shape* limitations.
    #[error(
        "field {field:?} has type {type_name}; the v0.1 executor handles only \
         scalar/string leaves and nested tables (see docs/design.md §15)"
    )]
    UnsupportedType {
        field: String,
        type_name: &'static str,
    },

    /// A path step (`[idx]`, `[*]`, `[key]`, `|keys`) is not yet
    /// implemented. Returned eagerly so callers using
    /// `flatbuffers_query` against an unsupported syntax get a clear
    /// message rather than silently truncating the path.
    #[error(
        "path step `{what}` is not yet implemented in the v0.1 executor \
         (see docs/design.md §15)"
    )]
    UnsupportedStep { what: &'static str },

    /// Anything else from the underlying reflection accessors. We
    /// preserve the upstream message verbatim because its shape is
    /// part of the upstream crate's public contract.
    #[error("internal reflection error: {0}")]
    Internal(String),
}

/// Run `query` against `buf`. Returns one entry per leaf produced by
/// the path:
///
/// - For paths *without* `Step::All`, the result has length 1; the
///   single entry is `Some(text)` for a present leaf or `None` when
///   the path short-circuited at an absent intermediate / absent
///   string / out-of-range index.
/// - For paths with one `Step::All` over a vector of length N, the
///   result has length N (or 0 if the vector is absent / empty).
///
/// `bounds` plumbs the per-call resource limits from
/// `docs/design.md` §10. `options` carries the per-call read-side
/// knobs ([`ExecuteOptions`]). Postgres entry points source both
/// from the GUC layer (see [`crate::guc::current_bounds`] and
/// [`crate::guc::current_fill_scalar_defaults`]); pure-Rust tests
/// typically pass [`Bounds::default`] and [`ExecuteOptions::default`].
pub fn execute_with_options(
    buf: &[u8],
    schema: &Schema<'_>,
    query: &Query,
    bounds: &Bounds,
    options: &ExecuteOptions,
) -> Result<Vec<Option<String>>, ExecuteError> {
    // Verify first; every subsequent unchecked accessor relies on
    // this returning `Ok`.
    verify(buf, schema, bounds)?;

    // SAFETY: `verify` confirmed that `buf` is a well-formed
    // FlatBuffer whose root table matches `schema.root_table()`. The
    // unchecked accessors below read offsets that the verifier just
    // proved were in-bounds.
    let root_table = unsafe { get_any_root(buf) };
    let root_object = schema
        .root_table()
        .expect("verify() rejects schemas with no root_table");

    walk_table(&root_table, &root_object, schema, &query.steps, options)
}

mod leaf;
mod map_key;
mod pg_text;
mod struct_;
mod union;
mod util;
mod vector;
mod walk;

use walk::walk_table;

#[cfg(test)]
mod tests;
