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
//!   Supports nested-struct fields and scalar leaves; rejects any
//!   non-`Field` step (structs hold no vectors, only scalars,
//!   nested structs, and — deferred — fixed-size arrays).
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
//!   types: scalars, bool, string, and tables (descend with the
//!   remaining steps; nested `[*]` accumulates depth-first).
//! - `Step::MapKey` for `(key)`-vector lookup (§7.2 step 4). Linear
//!   scan (the §10 `pg_flatbuffers.key_lookup_strict = off`
//!   fallback) until the verifier sortedness check lands and lets
//!   the strict path switch to binary search. String- and
//!   integer-keyed `Entry` tables supported.
//! - `Step::MapKeys` for fanning out the `(key)` field of every
//!   element of a `(key)`-annotated vector (§7.2 step 5). Like
//!   `[*]` but constrained to the keyed field; tail must be empty
//!   because the keys themselves are the leaves.
//! - Stringification of scalar (int/uint/float/bool) and string leaves
//!   via [`flatbuffers_reflection::get_any_field_string`].
//!
//! Deliberately deferred to dedicated micro-slices, each returning a
//! clear [`ExecuteError::Unsupported*`] variant today:
//!
//! - Vectors of structs — walks need to slice the vector body into
//!   inline element windows; reuse `walk_struct` once that lands.
//! - Fixed-size arrays inside structs (`BaseType::Array`) — same
//!   element-window math as vectors-of-structs; deferred together.
//! - Union variants other than table (struct or string variants).
//! - `BaseType::Vector64` — element-offset arithmetic uses 64-bit
//!   offsets and `flatbuffers::Vector<T>` only handles 32-bit;
//!   rejected loudly so we don't silently truncate addresses.
//! - Vectors of unions / vectors of vectors / vectors of arrays.
//! - The `pg_flatbuffers.proto3_defaults` GUC (§10) — today scalar
//!   "absent" returns the schema default (postgres-protobuf compat).
//!
//! Pure Rust; no `pgrx` dependency. The Postgres SQL wrappers live in
//! `functions.rs` (next slice).

use super::ast::{FieldRef, MapKey, Query, Step};
use crate::verify::VerifyError;
use crate::verify::{verify, Bounds};
use flatbuffers::{read_scalar_at, ForwardsUOffset, Table, Vector};
use flatbuffers_reflection::reflection::{BaseType, Field, Object, Schema};
use flatbuffers_reflection::{
    get_any_field_string, get_any_root, get_field_table, get_field_vector, FlatbufferError,
};
use thiserror::Error;

/// Errors produced by [`execute`].
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
/// `docs/design.md` §10. The caller is responsible for sourcing them
/// from the GUCs (the GUC slice ships next); for now most callers
/// pass [`Bounds::default`].
pub fn execute(
    buf: &[u8],
    schema: &Schema<'_>,
    query: &Query,
    bounds: &Bounds,
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

    walk_table(&root_table, &root_object, schema, &query.steps)
}

// ---------------------------------------------------------------------------
// Recursive walker
// ---------------------------------------------------------------------------

/// Walk the path `steps` starting from `table` (whose schema shape is
/// `object`). Returns one or more leaves in wire-format order; see
/// [`execute`] for the length contract.
///
/// # Safety contract
///
/// The caller has already verified the underlying buffer (see
/// [`execute`]). Every unsafe block in this function is sound under
/// that precondition.
fn walk_table(
    table: &Table,
    object: &Object,
    schema: &Schema,
    steps: &[Step],
) -> Result<Vec<Option<String>>, ExecuteError> {
    let (head, tail) = steps
        .split_first()
        .expect("parser guarantees at least one step");

    let field = match head {
        Step::Field(field_ref) => find_field(object, field_ref)?,
        Step::Index(_) => return Err(ExecuteError::UnsupportedStep { what: "[index]" }),
        Step::All => return Err(ExecuteError::UnsupportedStep { what: "[*]" }),
        Step::MapKey(_) => return Err(ExecuteError::UnsupportedStep { what: "[map-key]" }),
        Step::MapKeys => return Err(ExecuteError::UnsupportedStep { what: "|keys" }),
        Step::UnionType => return Err(ExecuteError::UnsupportedStep { what: "|type" }),
    };

    let field_name = field.name();
    let base_type = field.type_().base_type();

    // `|type` is a tail-position leaf marker on a union value
    // field: it yields the *name* of the active variant ("TableA",
    // "NONE", …) rather than its numeric discriminator (which is
    // available on the auto-generated `<field>_type` UType scalar).
    // Intercepted ahead of the absent-nullable short-circuit so
    // that an absent / NONE union still returns the variant name
    // ("NONE") rather than SQL NULL — symmetric with how
    // `<field>_type` returns "0" for an absent discriminator.
    if let [Step::UnionType, more @ ..] = tail {
        if base_type != BaseType::Union {
            return Err(ExecuteError::UnsupportedStep {
                what: "|type (only valid on union fields)",
            });
        }
        if !more.is_empty() {
            return Err(ExecuteError::UnsupportedStep {
                what: "|type (terminal — cannot descend further)",
            });
        }
        return read_union_type_leaf(table, &field, schema);
    }

    // Presence check via vtable: vtable.get(offset) == 0 means the
    // field is absent in this table instance. This is how we map
    // "field not set" to SQL NULL for nullable types (string,
    // sub-table, vector). Scalar types are handled separately
    // below: an absent scalar yields its schema default per the
    // postgres-protobuf-compatible default for `pg_flatbuffers
    // .proto3_defaults = off` (§4.3, §10). When that GUC slice
    // lands, the proto3 mode will branch here.
    let is_present = table.vtable().get(field.offset()) != 0;
    let is_nullable_type = matches!(
        base_type,
        BaseType::String
            | BaseType::Obj
            | BaseType::Vector
            | BaseType::Vector64
            | BaseType::Union
            | BaseType::Array
    );

    // Vector dispatch must come before *both* the absent-nullable
    // short-circuit and the leaf/descent fork, because the
    // `[i]` vs `[*]` distinction lives in `walk_vector`: an absent
    // vector under `[i]` is `vec![None]` (one virtual entry), but
    // under `[*]` it's `vec![]` (no items to fan out over).
    // `walk_vector` handles both cases internally.
    if matches!(base_type, BaseType::Vector | BaseType::Vector64) {
        return walk_vector(table, &field, schema, tail);
    }

    if !is_present && is_nullable_type {
        // One virtual leaf, value `None`. (Vector dispatch above
        // already covered the vector-typed nullable case.)
        return Ok(vec![None]);
    }

    if tail.is_empty() {
        if !is_present {
            // Absent scalar at leaf — synthesize the schema default.
            // Upstream `get_any_field_string` returns "" for absent
            // *anything*, so we must source the default from
            // `Field::default_integer()` / `default_real()`
            // ourselves. (Required is enforced by the verifier; we
            // never reach here for `field.required() == true`
            // because verification would have failed.)
            return Ok(vec![Some(scalar_default_string(&field, base_type))]);
        }
        return read_leaf(table, &field, schema, base_type).map(|opt| vec![opt]);
    }

    // Descent — only nested tables are supported in this slice.
    match base_type {
        BaseType::Obj => {
            let child_index = field.type_().index();
            if child_index < 0 {
                return Err(ExecuteError::Internal(format!(
                    "schema field {field_name:?} has BaseType::Obj but negative \
                     object index ({child_index})"
                )));
            }
            let child_object = schema.objects().get(
                usize::try_from(child_index)
                    .expect("non-negative after the explicit < 0 guard above"),
            );
            if child_object.is_struct() {
                // Struct descent. Structs are inline fixed-size; the
                // vtable slot stores the byte offset within the
                // parent table's data area where the struct begins
                // (mirroring `Table::get`'s prelude). Slot 0 means
                // the field is absent — emit one virtual `None` leaf,
                // matching the absent-sub-table case below.
                if tail.is_empty() {
                    return Err(ExecuteError::UnsupportedType {
                        field: field_name.to_string(),
                        type_name: "struct (no v0.1 textual leaf form — descend with `.field`)",
                    });
                }
                let slot = table.vtable().get(field.offset()) as usize;
                if slot == 0 {
                    return Ok(vec![None]);
                }
                let struct_loc = table.loc() + slot;
                return walk_struct(table.buf(), struct_loc, &child_object, schema, tail);
            }
            // SAFETY: see `execute`; the buffer was verified, and
            // `field` came from the schema (so its offset is the
            // verified vtable slot).
            let child_table_opt =
                unsafe { get_field_table(table, &field) }.map_err(map_reflection_err)?;
            match child_table_opt {
                Some(child_table) => walk_table(&child_table, &child_object, schema, tail),
                // Defensive: vtable said present, but the deref
                // returned None. Treat as absent rather than
                // panicking.
                None => Ok(vec![None]),
            }
        }
        BaseType::Union => walk_union(table, &field, schema, tail),
        other => Err(ExecuteError::UnsupportedType {
            field: field_name.to_string(),
            type_name: base_type_name(other),
        }),
    }
}

// ---------------------------------------------------------------------------
// Vector dispatch
// ---------------------------------------------------------------------------

/// Walk a vector field. `field` is the [`Field`] whose
/// [`BaseType`] is `Vector` (or `Vector64` — currently rejected,
/// since the upstream verifier doesn't yet validate Vector64
/// payloads). `steps` is the *remaining* path after the vector field
/// itself; it must lead with one of `Step::Index`, `Step::All`,
/// `Step::MapKey`, or `Step::MapKeys`. The latter two are deferred
/// to dedicated micro-slices and rejected here with
/// [`ExecuteError::UnsupportedStep`].
///
/// Length contract:
///
/// - `[i]` with absent / OOB / empty vector → `vec![None]` (one
///   virtual entry; path traversal short-circuits).
/// - `[*]` with absent / empty vector → `vec![]` (no items to fan
///   out over, zero entries).
/// - `[*]` with present length-N vector → N entries (depth-first
///   left-to-right for nested `[*]` per §7.2 step 3).
fn walk_vector(
    table: &Table,
    field: &Field,
    schema: &Schema,
    steps: &[Step],
) -> Result<Vec<Option<String>>, ExecuteError> {
    let field_name = field.name();

    // Vector64 is in scope for the design but not for v0.1: the
    // upstream verifier still treats it the same as Vector for length
    // bookkeeping, but element-offset arithmetic uses 64-bit offsets
    // and our `flatbuffers::Vector<T>` accessors only handle 32-bit.
    // Reject loudly so we don't silently truncate addresses.
    if field.type_().base_type() == BaseType::Vector64 {
        return Err(ExecuteError::UnsupportedType {
            field: field_name.to_string(),
            type_name: "vector64",
        });
    }

    // A bare `Order:items` (no indexer) cannot be stringified in
    // v0.1: the design doesn't define a textual form for whole
    // vectors, and the JSON path lives in a future slice.
    let (head, tail) = match steps.split_first() {
        Some(pair) => pair,
        None => {
            return Err(ExecuteError::UnsupportedType {
                field: field_name.to_string(),
                type_name: "vector (use [i] / [*] / [key] / |keys to access elements)",
            });
        }
    };

    let element_base_type = field.type_().element();

    match head {
        Step::Index(idx) => {
            walk_vector_at_index(table, field, schema, *idx, tail, element_base_type)
        }
        Step::All => walk_vector_all(table, field, schema, tail, element_base_type),
        Step::MapKey(key) => {
            walk_vector_at_map_key(table, field, schema, key, tail, element_base_type)
        }
        Step::MapKeys => walk_vector_map_keys(table, field, schema, tail, element_base_type),
        // `|type` makes no sense on a vector field — vectors aren't
        // unions. Reject explicitly so the AST stays exhaustive.
        Step::UnionType => Err(ExecuteError::UnsupportedStep {
            what: "|type (only valid on union fields)",
        }),
        // A `Step::Field` after a vector field is a parser bug — the
        // grammar requires an indexer right after the vector. Be
        // defensive in case the AST grows new arms.
        Step::Field(_) => Err(ExecuteError::Internal(format!(
            "expected `[i]` / `[*]` / `[key]` / `|keys` after vector field {field_name:?}, \
             found a `Field` step"
        ))),
    }
}

/// Materialise a single indexed element. Out-of-range / absent →
/// `vec![None]`. Element type `Obj` may consume more `tail` steps
/// for descent; scalar / string element types must be at leaf.
fn walk_vector_at_index(
    table: &Table,
    field: &Field,
    schema: &Schema,
    idx: usize,
    tail: &[Step],
    element_base_type: BaseType,
) -> Result<Vec<Option<String>>, ExecuteError> {
    let field_name = field.name();

    if element_base_type == BaseType::Obj {
        let child_object = lookup_vector_element_object(field, schema)?;
        if child_object.is_struct() {
            // Vectors of inline structs need the same dedicated
            // walk_struct cursor as bare struct descent (deferred).
            return Err(ExecuteError::UnsupportedType {
                field: field_name.to_string(),
                type_name: "vector of struct",
            });
        }

        // SAFETY: see `execute`; the buffer was verified, and `field`
        // came from the schema. The verifier asserts the vector slot
        // resolves to a well-formed Vector<ForwardsUOffset<Table>>
        // when `field.type_().element() == Obj`.
        let vec_opt = unsafe {
            table.get::<ForwardsUOffset<Vector<ForwardsUOffset<Table>>>>(field.offset(), None)
        };
        let vec = match vec_opt {
            Some(v) => v,
            // Defensive: vtable said present but follow returned
            // None. Match the absent-vector contract.
            None => return Ok(vec![None]),
        };
        if idx >= vec.len() {
            return Ok(vec![None]);
        }
        let elem_table = vec.get(idx);

        if tail.is_empty() {
            // `items[3]` on a vector of tables produces a sub-table
            // *value*, which has no v0.1 textual form. Same rationale
            // as bare-table-at-leaf in `read_leaf`.
            return Err(ExecuteError::UnsupportedType {
                field: field_name.to_string(),
                type_name: "vector-of-table element (sub-table at leaf)",
            });
        }
        return walk_table(&elem_table, &child_object, schema, tail);
    }

    // Scalar / string element types: this *must* be the terminal
    // step. `items[3].sub` against a Vector<int> is a path error.
    if !tail.is_empty() {
        return Err(ExecuteError::UnsupportedType {
            field: field_name.to_string(),
            type_name: base_type_name(element_base_type),
        });
    }
    read_vector_element(table, field, idx, element_base_type).map(|opt| vec![opt])
}

/// Fan out across every element of `field` in wire-format order.
/// Absent / empty vector → `vec![]`. For tables, recurse with `tail`
/// per-element and concatenate (depth-first left-to-right per
/// §7.2 step 3); for scalars / strings, `tail` must be empty and we
/// stringify each element.
fn walk_vector_all(
    table: &Table,
    field: &Field,
    schema: &Schema,
    tail: &[Step],
    element_base_type: BaseType,
) -> Result<Vec<Option<String>>, ExecuteError> {
    let field_name = field.name();

    if element_base_type == BaseType::Obj {
        let child_object = lookup_vector_element_object(field, schema)?;
        if child_object.is_struct() {
            return Err(ExecuteError::UnsupportedType {
                field: field_name.to_string(),
                type_name: "vector of struct",
            });
        }

        // SAFETY: see `execute`. Same Vector<ForwardsUOffset<Table>>
        // shape as `walk_vector_at_index`.
        let vec_opt = unsafe {
            table.get::<ForwardsUOffset<Vector<ForwardsUOffset<Table>>>>(field.offset(), None)
        };
        let vec = match vec_opt {
            Some(v) => v,
            // Absent vector under `[*]` → no fanout, zero leaves.
            None => return Ok(vec![]),
        };

        if tail.is_empty() {
            return Err(ExecuteError::UnsupportedType {
                field: field_name.to_string(),
                type_name: "vector-of-table element (sub-table at leaf)",
            });
        }

        // Pre-size to vec.len() as a floor; nested `[*]` may grow
        // the result further per element.
        let mut out: Vec<Option<String>> = Vec::with_capacity(vec.len());
        for elem_table in vec.iter() {
            let mut sub = walk_table(&elem_table, &child_object, schema, tail)?;
            out.append(&mut sub);
        }
        return Ok(out);
    }

    // Scalar / string element types: must be the terminal step.
    if !tail.is_empty() {
        return Err(ExecuteError::UnsupportedType {
            field: field_name.to_string(),
            type_name: base_type_name(element_base_type),
        });
    }
    read_vector_all(table, field, element_base_type)
}

/// Resolve the `Object` referenced by a vector-of-table field's
/// `field.type_().index()`. Factored out because both the indexed
/// and the fanout paths need it.
fn lookup_vector_element_object<'a>(
    field: &Field,
    schema: &'a Schema,
) -> Result<Object<'a>, ExecuteError> {
    let child_index = field.type_().index();
    if child_index < 0 {
        return Err(ExecuteError::Internal(format!(
            "vector field {:?} has element BaseType::Obj but negative object index ({})",
            field.name(),
            child_index
        )));
    }
    Ok(schema.objects().get(
        usize::try_from(child_index).expect("non-negative after the explicit < 0 guard above"),
    ))
}

/// Resolve the `(key)`-annotated field on `child_object` (the table
/// type of vector elements). FlatBuffers guarantees at most one
/// `(key)` field per table; we still error explicitly if zero are
/// found, so the caller can surface a "not a keyed vector" message
/// rather than silently returning no match.
fn lookup_keyed_field<'a>(child_object: &'a Object) -> Result<Field<'a>, ExecuteError> {
    let fields = child_object.fields();
    let mut found: Option<Field<'a>> = None;
    for i in 0..fields.len() {
        let f = fields.get(i);
        if f.key() {
            // flatc rejects multi-key tables at schema-compile time,
            // so we only need to defend against malformed `.bfbs`.
            if found.is_some() {
                return Err(ExecuteError::Internal(format!(
                    "table {:?} has more than one (key)-annotated field",
                    child_object.name()
                )));
            }
            found = Some(f);
        }
    }
    found.ok_or_else(|| ExecuteError::UnsupportedType {
        field: child_object.name().to_string(),
        type_name: "vector of tables with no (key)-annotated field",
    })
}

/// Compare the `(key)`-annotated field on `elem` against the AST
/// literal `key`. Returns `Ok(true)` on a match, `Ok(false)` on a
/// non-match (including type-mismatch combinations like a textual
/// key against an integer field that fails to parse), and `Err` for
/// genuinely unsupported keyed-field types.
///
/// This slice supports `(key)` fields whose type is `String` or any
/// signed/unsigned integer width. `Float` / `Double` / `Bool` keyed
/// fields are deferred — they're permitted by the FlatBuffers spec
/// but vanishingly rare in practice and would need additional
/// stringification rules to round-trip cleanly.
fn key_matches(
    elem: &Table,
    keyed_field: &Field,
    schema: &Schema,
    key: &MapKey,
) -> Result<bool, ExecuteError> {
    let kbase = keyed_field.type_().base_type();

    // For both string and integer keyed fields the `(key)` field is
    // by convention `required` (flatc enforces this), so the
    // schema-default fallback in `get_any_field_string` never fires
    // for a well-formed buffer.
    //
    // SAFETY: the buffer was verified by `execute`, and `keyed_field`
    // came from the schema's reflected `Object.fields()`.
    let actual = unsafe { get_any_field_string(elem, keyed_field, schema) };

    match (kbase, key) {
        (BaseType::String, MapKey::Text(s)) => Ok(actual == *s),
        // A textual key against a string field that the AST already
        // promoted to `Int` is a non-match, not an error: the user
        // asked for a numeric value where the schema demands a string.
        (BaseType::String, MapKey::Int(_)) => Ok(false),

        // Integer keyed field. The parser only ever emits
        // `MapKey::Text` (see `ast.rs`), so the common path is text
        // → int parse. We compare numerically (rather than as
        // formatted decimals) to dodge leading-zero / sign-formatting
        // ambiguities — `[042]` and `[42]` both match a stored 42.
        (b, MapKey::Text(s)) if is_integer_base(b) => {
            let want: i64 = match s.parse::<i64>() {
                Ok(n) => n,
                // Non-numeric key against an integer field: the user
                // asked for a key the field cannot hold. Treat as
                // "no match" rather than an error so a typo at the
                // SQL site surfaces as `NULL` (consistent with the
                // OOB-index short-circuit) rather than aborting the
                // whole statement.
                Err(_) => return Ok(false),
            };
            // ULong values above i64::MAX would underflow this parse
            // and silently no-match; that's a known v0.1 limitation
            // documented at the call site.
            Ok(actual.parse::<i64>().map(|v| v == want).unwrap_or(false))
        }
        (b, MapKey::Int(n)) if is_integer_base(b) => {
            Ok(actual.parse::<i64>().map(|v| v == *n).unwrap_or(false))
        }

        // Float / Double / Bool / nested table / vector / union /
        // array keyed fields are out of scope for this slice.
        _ => Err(ExecuteError::UnsupportedType {
            field: keyed_field.name().to_string(),
            type_name: base_type_name(kbase),
        }),
    }
}

/// All integer-width FlatBuffers scalars (signed and unsigned). The
/// `(key)` annotation supports any of them per the FlatBuffers spec.
fn is_integer_base(b: BaseType) -> bool {
    matches!(
        b,
        BaseType::Byte
            | BaseType::UByte
            | BaseType::Short
            | BaseType::UShort
            | BaseType::Int
            | BaseType::UInt
            | BaseType::Long
            | BaseType::ULong
    )
}

/// `field[abc]` — find the entry whose `(key)`-annotated field
/// equals `key` (linear scan, first match in wire order). The
/// design (§7.2 step 4) calls for binary search over `(key)`-sorted
/// vectors, but that's gated on a verifier check that the vector is
/// actually sorted. Until that check lands, we use the
/// `pg_flatbuffers.key_lookup_strict = off` semantics from §10
/// unconditionally — correct on any buffer, just O(n) per lookup.
///
/// On miss / absent / empty vector, returns `vec![None]` (path
/// short-circuits, matching `Step::Index`'s OOB behaviour).
fn walk_vector_at_map_key(
    table: &Table,
    field: &Field,
    schema: &Schema,
    key: &MapKey,
    tail: &[Step],
    element_base_type: BaseType,
) -> Result<Vec<Option<String>>, ExecuteError> {
    let field_name = field.name();

    // Map-key lookup is only defined over vectors of `(key)`-
    // annotated tables. Scalar / string vectors don't have a "key"
    // distinct from the element value, so refuse rather than
    // silently treat the literal as an index.
    if element_base_type != BaseType::Obj {
        return Err(ExecuteError::UnsupportedType {
            field: field_name.to_string(),
            type_name: base_type_name(element_base_type),
        });
    }

    let child_object = lookup_vector_element_object(field, schema)?;
    if child_object.is_struct() {
        return Err(ExecuteError::UnsupportedType {
            field: field_name.to_string(),
            type_name: "vector of struct",
        });
    }

    let keyed_field = lookup_keyed_field(&child_object)?;

    if tail.is_empty() {
        // `items[abc]` lands at a sub-table value with no v0.1
        // textual form. Same rationale as `items[3]` /
        // `items[*]` (no `.field` continuation).
        return Err(ExecuteError::UnsupportedType {
            field: field_name.to_string(),
            type_name: "vector-of-table element (sub-table at leaf)",
        });
    }

    // SAFETY: see `execute`. Same `Vector<ForwardsUOffset<Table>>`
    // shape used by `walk_vector_at_index` / `walk_vector_all`.
    let vec_opt = unsafe {
        table.get::<ForwardsUOffset<Vector<ForwardsUOffset<Table>>>>(field.offset(), None)
    };
    let vec = match vec_opt {
        Some(v) => v,
        None => return Ok(vec![None]),
    };

    for elem in vec.iter() {
        if key_matches(&elem, &keyed_field, schema, key)? {
            return walk_table(&elem, &child_object, schema, tail);
        }
    }
    // No element matched — short-circuit, same as out-of-range index.
    Ok(vec![None])
}

/// `field|keys` — fan out the `(key)`-annotated field of every
/// element of a vector of tables, in wire-format order. The keys
/// **are** the leaves: `tail` must be empty, since there is nothing
/// to descend into past a stringified key.
///
/// Symmetric to [`walk_vector_all`] but constrained to the keyed
/// field rather than recursing with arbitrary `tail`. Absent /
/// empty vector → `vec![]` (no items to fan out, matching the
/// `[*]` semantics).
fn walk_vector_map_keys(
    table: &Table,
    field: &Field,
    schema: &Schema,
    tail: &[Step],
    element_base_type: BaseType,
) -> Result<Vec<Option<String>>, ExecuteError> {
    let field_name = field.name();

    if element_base_type != BaseType::Obj {
        return Err(ExecuteError::UnsupportedType {
            field: field_name.to_string(),
            type_name: base_type_name(element_base_type),
        });
    }

    let child_object = lookup_vector_element_object(field, schema)?;
    if child_object.is_struct() {
        return Err(ExecuteError::UnsupportedType {
            field: field_name.to_string(),
            type_name: "vector of struct",
        });
    }

    let keyed_field = lookup_keyed_field(&child_object)?;

    if !tail.is_empty() {
        // The parser allows `items|keys.foo` (see `parser.rs`); the
        // executor rejects it here because `|keys` already produced
        // a leaf — there's nothing to descend into.
        return Err(ExecuteError::UnsupportedType {
            field: field_name.to_string(),
            type_name: "trailing path step after `|keys` (keys are terminal leaves)",
        });
    }

    // SAFETY: see `execute`. Same `Vector<ForwardsUOffset<Table>>`
    // shape as `walk_vector_at_map_key`.
    let vec_opt = unsafe {
        table.get::<ForwardsUOffset<Vector<ForwardsUOffset<Table>>>>(field.offset(), None)
    };
    let vec = match vec_opt {
        Some(v) => v,
        None => return Ok(vec![]),
    };

    // Pre-size to vec.len(); each element contributes exactly one
    // key entry (the keyed field is `(key)` and conventionally
    // required, so it's always present).
    let mut out: Vec<Option<String>> = Vec::with_capacity(vec.len());
    for elem in vec.iter() {
        // SAFETY: see `key_matches`. The keyed field came from the
        // schema's reflected `Object.fields()`.
        let key = unsafe { get_any_field_string(&elem, &keyed_field, schema) };
        out.push(Some(key));
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Struct dispatch
// ---------------------------------------------------------------------------

/// Walk an inline fixed-size struct at byte offset `struct_loc`
/// inside `buf`. Mirrors [`walk_table`] but without vtables: every
/// struct field is unconditionally present and lives at
/// `struct_loc + field.offset()` (the reflection metadata stores
/// byte offsets *within the struct* for struct fields, not
/// vtable slots).
///
/// Structs may contain only scalars, nested structs, and fixed-size
/// arrays — no vectors, strings, sub-tables, or unions — so all the
/// vector / map-key step variants are rejected up-front. Fixed-size
/// arrays (`BaseType::Array`) are deferred to a future slice.
///
/// Empty step list at a struct is a hard error: structs have no
/// v0.1 textual leaf form (each field is queryable individually,
/// but the struct as a whole would need a JSON-shaped output that
/// belongs to the §8 round-trip slice).
fn walk_struct(
    buf: &[u8],
    struct_loc: usize,
    object: &Object,
    schema: &Schema,
    steps: &[Step],
) -> Result<Vec<Option<String>>, ExecuteError> {
    let object_name = object.name();
    let (head, tail) = match steps.split_first() {
        Some(p) => p,
        // Caller (`walk_table`) already short-circuits empty `tail`
        // before handing off to us; this is purely defensive against
        // future direct callers.
        None => {
            return Err(ExecuteError::UnsupportedType {
                field: object_name.to_string(),
                type_name: "struct (no v0.1 textual leaf form — descend with `.field`)",
            });
        }
    };

    let field_ref = match head {
        Step::Field(fr) => fr,
        // Structs hold no vectors, so all of these are static
        // type-system mismatches at this position.
        Step::Index(_) => {
            return Err(ExecuteError::UnsupportedType {
                field: object_name.to_string(),
                type_name: "[index] inside a struct (structs hold no vectors)",
            });
        }
        Step::All => {
            return Err(ExecuteError::UnsupportedType {
                field: object_name.to_string(),
                type_name: "[*] inside a struct (structs hold no vectors)",
            });
        }
        Step::MapKey(_) => {
            return Err(ExecuteError::UnsupportedType {
                field: object_name.to_string(),
                type_name: "[map-key] inside a struct (structs hold no vectors)",
            });
        }
        Step::MapKeys => {
            return Err(ExecuteError::UnsupportedType {
                field: object_name.to_string(),
                type_name: "|keys inside a struct (structs hold no vectors)",
            });
        }
        Step::UnionType => {
            return Err(ExecuteError::UnsupportedType {
                field: object_name.to_string(),
                type_name: "|type inside a struct (structs hold no unions)",
            });
        }
    };

    let field = find_field(object, field_ref)?;
    let field_name = field.name();
    let field_offset = usize::from(field.offset());
    let base_type = field.type_().base_type();

    if tail.is_empty() {
        // Scalar leaf inside the struct. Strings / sub-tables /
        // vectors / unions cannot appear in structs, so reject any
        // non-scalar leaf with the same `UnsupportedType` shape we
        // use for non-leaf scalars in `read_leaf`.
        return read_struct_scalar_leaf(buf, struct_loc + field_offset, &field, base_type)
            .map(|opt| vec![opt]);
    }

    match base_type {
        BaseType::Obj => {
            let child_index = field.type_().index();
            if child_index < 0 {
                return Err(ExecuteError::Internal(format!(
                    "schema struct field {field_name:?} has BaseType::Obj but \
                     negative object index ({child_index})"
                )));
            }
            let child_object = schema.objects().get(
                usize::try_from(child_index)
                    .expect("non-negative after the explicit < 0 guard above"),
            );
            // Defensive: an `Obj` field inside a struct must point
            // at another struct (flatc rejects non-struct nesting at
            // compile time). If a malformed `.bfbs` slips a table
            // index in here, surface it as an internal error rather
            // than silently calling `walk_struct` on a table.
            if !child_object.is_struct() {
                return Err(ExecuteError::Internal(format!(
                    "schema struct {object_name:?} has Obj field {field_name:?} \
                     pointing at non-struct object {:?}",
                    child_object.name()
                )));
            }
            walk_struct(buf, struct_loc + field_offset, &child_object, schema, tail)
        }
        BaseType::Array => Err(ExecuteError::UnsupportedType {
            field: field_name.to_string(),
            type_name: "fixed-size array (deferred to a future slice)",
        }),
        // Scalar with a non-empty tail: same "can't descend into a
        // scalar" error `walk_table` raises.
        other => Err(ExecuteError::UnsupportedType {
            field: field_name.to_string(),
            type_name: base_type_name(other),
        }),
    }
}

/// Read and stringify a single scalar at `loc` inside `buf`,
/// dispatching on `base_type`. Mirrors the scalar-arm formatting
/// used by [`read_leaf`] / [`read_vector_element`] (integers via
/// `Display`, floats via `Display`, bool rendered as `0`/`1`) so a
/// value rendered out of a struct field matches the same value
/// rendered out of a table or vector element.
fn read_struct_scalar_leaf(
    buf: &[u8],
    loc: usize,
    field: &Field,
    base_type: BaseType,
) -> Result<Option<String>, ExecuteError> {
    // Helper: the buffer was verified, so `loc + size_of::<T>()` is
    // in bounds; `read_scalar_at` is `unsafe` solely on that bound.
    macro_rules! read {
        ($t:ty) => {{
            // SAFETY: see `execute`. Verifier validated that the
            // struct's enclosing field has the schema-declared
            // bytesize, which is the sum of its field offsets +
            // their scalar sizes. `read_scalar_at` already performs
            // little-endian-to-host conversion internally.
            let v: $t = unsafe { read_scalar_at::<$t>(buf, loc) };
            v.to_string()
        }};
    }

    let s = match base_type {
        // Bool is wire-encoded as `u8`; render as `0`/`1` to match
        // `read_leaf`'s upstream `get_any_field_string` output.
        BaseType::Bool | BaseType::UByte => read!(u8),
        BaseType::Byte => read!(i8),
        BaseType::Short => read!(i16),
        BaseType::UShort => read!(u16),
        BaseType::Int => read!(i32),
        BaseType::UInt => read!(u32),
        BaseType::Long => read!(i64),
        BaseType::ULong => read!(u64),
        BaseType::Float => read!(f32),
        BaseType::Double => read!(f64),
        // Structs cannot legally contain strings, vectors,
        // sub-tables, or unions per the FlatBuffers schema rules
        // (flatc rejects them at compile time). Surface a
        // user-facing error if a malformed `.bfbs` does anyway.
        other => {
            return Err(ExecuteError::UnsupportedType {
                field: field.name().to_string(),
                type_name: base_type_name(other),
            });
        }
    };
    Ok(Some(s))
}

// ---------------------------------------------------------------------------
// Union dispatch
// ---------------------------------------------------------------------------

/// Resolve a `BaseType::Union` field to its active variant and
/// recursively descend with the remaining steps. The wire layout
/// (design §4.3, "Union types"):
///
/// - The union value sits at vtable slot `N` (== `value_field.offset()`).
/// - The auto-generated `u8` discriminator (`<name>_type`,
///   `BaseType::UType`) sits at slot `N - 2` (flatc convention).
/// - Discriminator `0` is the synthesized `NONE` variant: no value,
///   short-circuits to `Ok(vec![None])` — same shape as an absent
///   sub-table.
///
/// Variant lookup goes through the reflected `Enum` (sorted by
/// `value`, so [`flatbuffers::Vector::lookup_by_key`] is O(log N)).
/// `EnumVal::union_type()` then resolves the underlying `Type`.
///
/// v0.1 supports **table-typed variants only**: struct or string
/// variants are rejected with [`ExecuteError::UnsupportedType`].
/// Empty `tail` is rejected too — the value table itself has no v0.1
/// textual leaf form (matches the absent-tail behaviour for nested
/// tables; descend with `.field` to get a value).
fn walk_union(
    table: &Table,
    value_field: &Field,
    schema: &Schema,
    tail: &[Step],
) -> Result<Vec<Option<String>>, ExecuteError> {
    let field_name = value_field.name();
    let value_slot = value_field.offset();

    // Discriminator slot is the immediately preceding vtable slot.
    // A vtable slot < 2 would mean the union value is the very first
    // field, leaving no room for the discriminator — flatc would
    // never emit such a layout, so treat it as a corrupted `.bfbs`.
    let disc_slot = match value_slot.checked_sub(2) {
        Some(s) => s,
        None => {
            return Err(ExecuteError::Internal(format!(
                "union value field {field_name:?} has vtable offset {value_slot}; \
                 expected ≥2 to leave room for the discriminator slot"
            )));
        }
    };

    // SAFETY: see `execute`. The verifier validated that the
    // discriminator slot, when present, holds a valid `u8` union
    // tag for `value_field.type_().index()`'s enum.
    let disc = unsafe { table.get::<u8>(disc_slot, Some(0)) }.unwrap_or(0);
    if disc == 0 {
        // `NONE` variant (or discriminator absent altogether) —
        // there is no value to descend into; one virtual `None`
        // leaf, matching the absent-sub-table shape.
        return Ok(vec![None]);
    }

    // Resolve the enum and the matching variant.
    let enum_index = value_field.type_().index();
    if enum_index < 0 {
        return Err(ExecuteError::Internal(format!(
            "schema field {field_name:?} has BaseType::Union but negative \
             enum index ({enum_index})"
        )));
    }
    let enum_idx =
        usize::try_from(enum_index).expect("non-negative after the explicit < 0 guard above");
    let enums = schema.enums();
    if enum_idx >= enums.len() {
        return Err(ExecuteError::Internal(format!(
            "schema field {field_name:?} references enum index {enum_idx} \
             but schema has only {} enums",
            enums.len()
        )));
    }
    let enum_def = enums.get(enum_idx);

    // EnumVal vector is sorted by `value` (per the upstream
    // `key_compare_*` impls), so lookup is O(log N).
    let variant = enum_def
        .values()
        .lookup_by_key(disc as i64, |v, k| v.key_compare_with_value(*k))
        .ok_or_else(|| {
            ExecuteError::Internal(format!(
                "union field {field_name:?} discriminator {disc} not found in \
                 enum {:?}",
                enum_def.name()
            ))
        })?;

    // Resolve the variant's underlying type.
    let variant_type = variant.union_type().ok_or_else(|| {
        ExecuteError::Internal(format!(
            "union variant {:?} has no union_type",
            variant.name()
        ))
    })?;
    if variant_type.base_type() != BaseType::Obj {
        return Err(ExecuteError::UnsupportedType {
            field: field_name.to_string(),
            type_name: "union with non-table variant (v0.1 supports table variants only)",
        });
    }
    let variant_obj_idx = variant_type.index();
    if variant_obj_idx < 0 {
        return Err(ExecuteError::Internal(format!(
            "union variant {:?} has Obj union_type but negative object index ({variant_obj_idx})",
            variant.name()
        )));
    }
    let variant_object = schema.objects().get(
        usize::try_from(variant_obj_idx).expect("non-negative after the explicit < 0 guard above"),
    );
    if variant_object.is_struct() {
        // Same as above: struct variants need a separate slice
        // (would dispatch to `walk_struct` once we settle the
        // wire-format check — flatc allows struct unions only via
        // the `(native_inline)` extension, which v0.1 doesn't model).
        return Err(ExecuteError::UnsupportedType {
            field: field_name.to_string(),
            type_name: "union with struct variant (v0.1 supports table variants only)",
        });
    }

    // Note: `tail` is guaranteed non-empty here. `walk_table`'s
    // leaf short-circuit fires before the descent match dispatches
    // to `walk_union`, so the empty-tail case is handled by
    // `read_leaf` (which rejects `BaseType::Union` loudly).

    // SAFETY: see `execute`. Verifier validated that, when the
    // discriminator is non-zero, slot N holds a valid
    // `ForwardsUOffset<Table>` referencing a buffer-bounded table
    // matching `variant_object`. We bypass `get_field_table` here
    // because that helper guards on `field.type_().base_type() ==
    // BaseType::Obj` and would reject a `BaseType::Union` field —
    // but the wire layout for a union value is identical to a
    // sub-table (a forward 32-bit offset to a table), so reading
    // it directly via `Table::get` is sound.
    let value_table_opt =
        unsafe { table.get::<ForwardsUOffset<Table>>(value_field.offset(), None) };
    match value_table_opt {
        Some(value_table) => walk_table(&value_table, &variant_object, schema, tail),
        // Defensive: discriminator non-zero but value slot absent.
        // Verifier would normally have rejected this; treat as `None`
        // rather than panicking on a misclassified malformed buffer.
        None => Ok(vec![None]),
    }
}

/// Read the active variant *name* of a union field as a single
/// string leaf (e.g. `"TableA"`, `"NONE"`). Used by the `|type`
/// terminal step. Mirrors the discriminator-resolution prelude of
/// [`walk_union`] but never descends into the value sub-table.
///
/// Unlike the descent path, an absent / NONE union does **not**
/// short-circuit to SQL NULL: it returns the variant name `"NONE"`
/// (or whatever the schema's value-0 enum entry is named) so callers
/// can `WHERE flatbuffers_query('Msg:body|type', buf) = 'NONE'`
/// without juggling NULLs.
fn read_union_type_leaf(
    table: &Table,
    value_field: &Field,
    schema: &Schema,
) -> Result<Vec<Option<String>>, ExecuteError> {
    let field_name = value_field.name();
    let value_slot = value_field.offset();
    let disc_slot = match value_slot.checked_sub(2) {
        Some(s) => s,
        None => {
            return Err(ExecuteError::Internal(format!(
                "union value field {field_name:?} has vtable offset {value_slot}; \
                 expected ≥2 to leave room for the discriminator slot"
            )));
        }
    };

    // SAFETY: see `execute`. The verifier validated the discriminator
    // slot, when present, holds a valid `u8` union tag for the enum.
    let disc = unsafe { table.get::<u8>(disc_slot, Some(0)) }.unwrap_or(0);

    let enum_index = value_field.type_().index();
    if enum_index < 0 {
        return Err(ExecuteError::Internal(format!(
            "schema field {field_name:?} has BaseType::Union but negative \
             enum index ({enum_index})"
        )));
    }
    let enum_idx =
        usize::try_from(enum_index).expect("non-negative after the explicit < 0 guard above");
    let enums = schema.enums();
    if enum_idx >= enums.len() {
        return Err(ExecuteError::Internal(format!(
            "schema field {field_name:?} references enum index {enum_idx} \
             but schema has only {} enums",
            enums.len()
        )));
    }
    let enum_def = enums.get(enum_idx);
    let variant = enum_def
        .values()
        .lookup_by_key(disc as i64, |v, k| v.key_compare_with_value(*k))
        .ok_or_else(|| {
            ExecuteError::Internal(format!(
                "union field {field_name:?} discriminator {disc} not found in \
                 enum {:?}",
                enum_def.name()
            ))
        })?;

    Ok(vec![Some(variant.name().to_string())])
}

/// Stringify the `idx`-th element of a vector whose elements are
/// scalars or strings. Returns `Ok(None)` when the index is past the
/// end or the vector itself is absent.
///
/// The match arm per scalar type is verbose but unavoidable:
/// [`get_field_vector`] is generic over `T: Follow`, so we must pick
/// a concrete `T` per [`BaseType`] to read individual elements.
fn read_vector_element(
    table: &Table,
    field: &Field,
    idx: usize,
    element_base_type: BaseType,
) -> Result<Option<String>, ExecuteError> {
    // Helper: read a typed scalar vector and stringify the indexed
    // element via `Display`. Mirrors the `i64::Display` / `f64::Display`
    // formatting used by `read_leaf` / `scalar_default_string` so an
    // element value matches what the same value would render as if
    // stored directly in a scalar field.
    macro_rules! scalar {
        ($t:ty) => {{
            // SAFETY: see `execute`; the buffer was verified, and
            // `field.type_().element()` matches `$t`. The
            // `get_field_vector` helper additionally checks that
            // `field.type_().base_type() == BaseType::Vector`.
            let vec_opt =
                unsafe { get_field_vector::<$t>(table, field) }.map_err(map_reflection_err)?;
            let vec = match vec_opt {
                Some(v) => v,
                None => return Ok(None),
            };
            if idx >= vec.len() {
                return Ok(None);
            }
            Ok(Some(vec.get(idx).to_string()))
        }};
    }

    match element_base_type {
        // Bool is wire-encoded as `u8`; stringify as `0`/`1` to
        // match the way present bool *fields* render through the
        // upstream `get_any_field_string` (see `read_leaf`).
        BaseType::Bool | BaseType::UByte => scalar!(u8),
        BaseType::Byte => scalar!(i8),
        BaseType::Short => scalar!(i16),
        BaseType::UShort => scalar!(u16),
        BaseType::Int => scalar!(i32),
        BaseType::UInt => scalar!(u32),
        BaseType::Long => scalar!(i64),
        BaseType::ULong => scalar!(u64),
        BaseType::Float => scalar!(f32),
        BaseType::Double => scalar!(f64),
        BaseType::String => {
            // SAFETY: see `execute`. The schema asserts the element
            // type is `String`, so the vector slot is a
            // `ForwardsUOffset<Vector<ForwardsUOffset<&str>>>`. We
            // can't use the `get_field_vector` helper here because
            // its `T: Follow<Inner = T>` bound rejects
            // `ForwardsUOffset<&str>` (whose `Inner` is `&str`).
            let vec_opt = unsafe {
                table.get::<ForwardsUOffset<Vector<ForwardsUOffset<&str>>>>(field.offset(), None)
            };
            let vec = match vec_opt {
                Some(v) => v,
                None => return Ok(None),
            };
            if idx >= vec.len() {
                return Ok(None);
            }
            Ok(Some(vec.get(idx).to_string()))
        }
        // Vectors of unions / vectors-of-vectors / vector-of-array
        // need their own slices.
        other => Err(ExecuteError::UnsupportedType {
            field: field.name().to_string(),
            type_name: base_type_name(other),
        }),
    }
}

/// Stringify *every* element of a vector whose elements are scalars
/// or strings, in wire-format order. Returns `Ok(vec![])` for an
/// absent vector. Mirrors [`read_vector_element`] arm-for-arm so
/// element formatting is identical.
fn read_vector_all(
    table: &Table,
    field: &Field,
    element_base_type: BaseType,
) -> Result<Vec<Option<String>>, ExecuteError> {
    macro_rules! scalar {
        ($t:ty) => {{
            // SAFETY: see `read_vector_element`.
            let vec_opt =
                unsafe { get_field_vector::<$t>(table, field) }.map_err(map_reflection_err)?;
            let vec = match vec_opt {
                Some(v) => v,
                None => return Ok(vec![]),
            };
            Ok(vec.iter().map(|e| Some(e.to_string())).collect())
        }};
    }

    match element_base_type {
        BaseType::Bool | BaseType::UByte => scalar!(u8),
        BaseType::Byte => scalar!(i8),
        BaseType::Short => scalar!(i16),
        BaseType::UShort => scalar!(u16),
        BaseType::Int => scalar!(i32),
        BaseType::UInt => scalar!(u32),
        BaseType::Long => scalar!(i64),
        BaseType::ULong => scalar!(u64),
        BaseType::Float => scalar!(f32),
        BaseType::Double => scalar!(f64),
        BaseType::String => {
            // SAFETY: see `read_vector_element` — same direct
            // `table.get::<ForwardsUOffset<Vector<ForwardsUOffset<&str>>>>`
            // workaround for the `Follow<Inner = Self>` bound.
            let vec_opt = unsafe {
                table.get::<ForwardsUOffset<Vector<ForwardsUOffset<&str>>>>(field.offset(), None)
            };
            let vec = match vec_opt {
                Some(v) => v,
                None => return Ok(vec![]),
            };
            Ok(vec.iter().map(|s| Some(s.to_string())).collect())
        }
        other => Err(ExecuteError::UnsupportedType {
            field: field.name().to_string(),
            type_name: base_type_name(other),
        }),
    }
}

/// Stringify a leaf (scalar, bool, or string).
fn read_leaf(
    table: &Table,
    field: &Field,
    schema: &Schema,
    base_type: BaseType,
) -> Result<Option<String>, ExecuteError> {
    let field_name = field.name();
    match base_type {
        BaseType::Bool
        | BaseType::Byte
        | BaseType::UByte
        | BaseType::UType
        | BaseType::Short
        | BaseType::UShort
        | BaseType::Int
        | BaseType::UInt
        | BaseType::Long
        | BaseType::ULong
        | BaseType::Float
        | BaseType::Double
        | BaseType::String => {
            // SAFETY: see `execute`. `get_any_field_string` reads via
            // the same offset accessors the verifier validated. For
            // scalars: returns the schema default if absent (that's
            // the §4.3 behaviour we want under default
            // `proto3_defaults = off`). For strings: we already
            // returned `Ok(None)` above for absent strings, so a
            // returned empty string here means an explicit empty
            // string in the buffer.
            let s = unsafe { get_any_field_string(table, field, schema) };
            Ok(Some(s))
        }
        BaseType::Obj
        | BaseType::Vector
        | BaseType::Vector64
        | BaseType::Union
        | BaseType::Array
        | BaseType::None => Err(ExecuteError::UnsupportedType {
            field: field_name.to_string(),
            type_name: base_type_name(base_type),
        }),
        // `BaseType` is a `pub struct BaseType(pub i8)` newtype, so
        // the compiler can't prove the match above is exhaustive.
        // Treat any out-of-range value as an internal corruption.
        _ => Err(ExecuteError::Internal(format!(
            "schema field {field_name:?} has unknown BaseType ({})",
            base_type.0
        ))),
    }
}

// ---------------------------------------------------------------------------
// Field lookup
// ---------------------------------------------------------------------------

fn find_field<'a>(object: &'a Object<'a>, field_ref: &FieldRef) -> Result<Field<'a>, ExecuteError> {
    let fields = object.fields();
    let table_name = object.name();
    match field_ref {
        FieldRef::Name(name) => fields
            // FlatBuffers schemas store `Object.fields` sorted by
            // name (the upstream crate also relies on this for
            // `lookup_by_key` binary search), so this is O(log N).
            .lookup_by_key(name.as_str(), |f, key| f.key_compare_with_value(key))
            .ok_or_else(|| ExecuteError::FieldNotFound {
                what: name.clone(),
                table: table_name.to_string(),
            }),
        FieldRef::Id(id) => {
            // Field IDs are dense and small in practice (typically
            // 0..N), so a linear scan is fine. We can't binary-search
            // because the vector is sorted by name, not by id.
            for f in &fields {
                if f.id() == *id {
                    return Ok(f);
                }
            }
            Err(ExecuteError::FieldNotFound {
                what: format!("#{id}"),
                table: table_name.to_string(),
            })
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn map_reflection_err(e: FlatbufferError) -> ExecuteError {
    match e {
        FlatbufferError::FieldNotFound => ExecuteError::FieldNotFound {
            what: "<unknown>".to_owned(),
            table: "<unknown>".to_owned(),
        },
        other => ExecuteError::Internal(other.to_string()),
    }
}

/// Stringify a scalar field's schema default, matching the
/// formatter used by `flatbuffers_reflection::get_any_field_string`
/// for *present* values: `i64::Display` for integral/bool and
/// `f64::Display` for floats. We deliberately do not special-case
/// bool to `"true"` / `"false"` because the upstream stringifier
/// emits `"0"` / `"1"` for present bools and we want absent
/// bools to round-trip identically.
fn scalar_default_string(field: &Field, base_type: BaseType) -> String {
    match base_type {
        BaseType::Float | BaseType::Double => field.default_real().to_string(),
        // Integral types and bool: `default_integer()` is i64.
        _ => field.default_integer().to_string(),
    }
}

fn base_type_name(b: BaseType) -> &'static str {
    match b {
        BaseType::None => "none",
        BaseType::UType => "union-discriminator",
        BaseType::Bool => "bool",
        BaseType::Byte => "byte",
        BaseType::UByte => "ubyte",
        BaseType::Short => "short",
        BaseType::UShort => "ushort",
        BaseType::Int => "int",
        BaseType::UInt => "uint",
        BaseType::Long => "long",
        BaseType::ULong => "ulong",
        BaseType::Float => "float",
        BaseType::Double => "double",
        BaseType::String => "string",
        BaseType::Vector => "vector",
        BaseType::Vector64 => "vector64",
        BaseType::Obj => "object",
        BaseType::Union => "union",
        BaseType::Array => "array",
        _ => "unknown",
    }
}

// ---------------------------------------------------------------------------
// Tests (pure Rust)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::ast::{MapKey, Query, Step};
    use crate::query::parse;
    use flatbuffers::FlatBufferBuilder;
    use flatbuffers_reflection::reflection::{
        root_as_schema, Enum, EnumArgs, EnumVal, EnumValArgs, Field as RField, FieldArgs,
        Object as RObject, ObjectArgs, Schema as RSchema, SchemaArgs, Type, TypeArgs,
    };

    // -- fixtures --

    /// Build a two-table schema:
    ///
    /// ```text
    /// table Customer {
    ///   email: string;   // id 0, vtable offset 4
    ///   name:  string;   // id 1, vtable offset 6
    /// }
    /// table Order {
    ///   customer: Customer;  // id 0, vtable offset 4
    ///   id:       int;       // id 1, vtable offset 6
    ///   note:     string;    // id 2, vtable offset 8 (nullable)
    /// }
    /// root_type Order;
    /// ```
    ///
    /// Field vectors are sorted alphabetically by name (FlatBuffers
    /// convention for `lookup_by_key` binary search). Object vector
    /// is also sorted (`Customer` < `Order`).
    fn build_schema() -> Vec<u8> {
        let mut fbb = FlatBufferBuilder::new();

        // Types we'll reuse.
        let str_t = Type::create(
            &mut fbb,
            &TypeArgs {
                base_type: BaseType::String,
                ..Default::default()
            },
        );
        let int_t = Type::create(
            &mut fbb,
            &TypeArgs {
                base_type: BaseType::Int,
                ..Default::default()
            },
        );

        // -- Customer fields (sorted by name: email, name) --
        let email_n = fbb.create_string("email");
        let email_f = RField::create(
            &mut fbb,
            &FieldArgs {
                name: Some(email_n),
                type_: Some(str_t),
                id: 0,
                offset: 4,
                ..Default::default()
            },
        );
        let cname_n = fbb.create_string("name");
        let cname_f = RField::create(
            &mut fbb,
            &FieldArgs {
                name: Some(cname_n),
                type_: Some(str_t),
                id: 1,
                offset: 6,
                ..Default::default()
            },
        );
        let cust_fields = fbb.create_vector(&[email_f, cname_f]);
        let cust_n = fbb.create_string("Customer");
        let customer = RObject::create(
            &mut fbb,
            &ObjectArgs {
                name: Some(cust_n),
                fields: Some(cust_fields),
                ..Default::default()
            },
        );

        // Sub-table type referring to Customer (object index 0 once
        // sorted; we'll sort by name below: Customer < Order).
        let cust_obj_t = Type::create(
            &mut fbb,
            &TypeArgs {
                base_type: BaseType::Obj,
                index: 0,
                ..Default::default()
            },
        );

        // -- Order fields (sorted by name: customer, id, note) --
        let cust_field_n = fbb.create_string("customer");
        let cust_field = RField::create(
            &mut fbb,
            &FieldArgs {
                name: Some(cust_field_n),
                type_: Some(cust_obj_t),
                id: 0,
                offset: 4,
                ..Default::default()
            },
        );
        let id_n = fbb.create_string("id");
        let id_f = RField::create(
            &mut fbb,
            &FieldArgs {
                name: Some(id_n),
                type_: Some(int_t),
                id: 1,
                offset: 6,
                ..Default::default()
            },
        );
        let note_n = fbb.create_string("note");
        let note_f = RField::create(
            &mut fbb,
            &FieldArgs {
                name: Some(note_n),
                type_: Some(str_t),
                id: 2,
                offset: 8,
                ..Default::default()
            },
        );
        let order_fields = fbb.create_vector(&[cust_field, id_f, note_f]);
        let order_n = fbb.create_string("Order");
        let order = RObject::create(
            &mut fbb,
            &ObjectArgs {
                name: Some(order_n),
                fields: Some(order_fields),
                ..Default::default()
            },
        );

        // Objects sorted by name: Customer (0), Order (1).
        let objects = fbb.create_vector(&[customer, order]);
        let enums = fbb.create_vector::<flatbuffers::ForwardsUOffset<Enum>>(&[]);
        let schema = RSchema::create(
            &mut fbb,
            &SchemaArgs {
                objects: Some(objects),
                enums: Some(enums),
                root_table: Some(order),
                ..Default::default()
            },
        );
        fbb.finish(schema, None);
        fbb.finished_data().to_vec()
    }

    /// Build an `Order` buffer matching the schema above.
    /// `customer` may be `None` (sub-table absent), or
    /// `Some((email, name))` to build a present `Customer`.
    /// `note` may be `None` (string absent) or `Some(text)`.
    fn build_order(customer: Option<(&str, &str)>, id: i32, note: Option<&str>) -> Vec<u8> {
        let mut fbb = FlatBufferBuilder::new();

        // Build the (optional) Customer first so its offset is known
        // before we open the Order table.
        let customer_off = customer.map(|(email, name)| {
            let email_off = fbb.create_string(email);
            let name_off = fbb.create_string(name);
            let t = fbb.start_table();
            // vtable slot 4 = email (string offset)
            fbb.push_slot_always(4, email_off);
            // vtable slot 6 = name (string offset)
            fbb.push_slot_always(6, name_off);
            fbb.end_table(t)
        });
        // `note` similarly built before opening Order.
        let note_off = note.map(|n| fbb.create_string(n));

        let t = fbb.start_table();
        // slot 4 = customer (sub-table offset)
        if let Some(off) = customer_off {
            fbb.push_slot_always(4, off);
        }
        // slot 6 = id (int with default 0)
        fbb.push_slot::<i32>(6, id, 0);
        // slot 8 = note (string offset, nullable)
        if let Some(off) = note_off {
            fbb.push_slot_always(8, off);
        }
        let order = fbb.end_table(t);
        fbb.finish_minimal(order);
        fbb.finished_data().to_vec()
    }

    /// Test helper: execute and unwrap to the **first** leaf, the
    /// shape that all the pre-`Step::All` tests assert against. The
    /// new fanout tests use [`run_all`] to inspect the full vec.
    fn run(query_str: &str, buf: &[u8], bfbs: &[u8]) -> Result<Option<String>, ExecuteError> {
        run_all(query_str, buf, bfbs).map(|v| v.into_iter().next().flatten())
    }

    /// Test helper: execute and return the full leaf vec. Mirrors
    /// what `flatbuffers_query_array` would surface to SQL.
    fn run_all(
        query_str: &str,
        buf: &[u8],
        bfbs: &[u8],
    ) -> Result<Vec<Option<String>>, ExecuteError> {
        let schema = root_as_schema(bfbs).expect("test schema verifies");
        let query = parse(query_str).expect("test query parses");
        execute(buf, &schema, &query, &Bounds::default())
    }

    // -- happy path: scalar leaves --

    #[test]
    fn scalar_leaf_by_name() {
        let bfbs = build_schema();
        let buf = build_order(None, 42, None);
        let v = run("Order:id", &buf, &bfbs).unwrap();
        assert_eq!(v.as_deref(), Some("42"));
    }

    #[test]
    fn scalar_leaf_by_id() {
        let bfbs = build_schema();
        let buf = build_order(None, 7, None);
        // Order.id has field id 1.
        let v = run("Order:#1", &buf, &bfbs).unwrap();
        assert_eq!(v.as_deref(), Some("7"));
    }

    #[test]
    fn scalar_absent_returns_default() {
        let bfbs = build_schema();
        // Build a buffer where `id` is at its default (0) and not
        // explicitly set. push_slot::<i32>(6, 0, 0) elides the slot
        // when value == default.
        let buf = build_order(None, 0, None);
        let v = run("Order:id", &buf, &bfbs).unwrap();
        // Per design §4.3 with `proto3_defaults = off` (today's
        // baseline), the default value is returned, NOT NULL.
        assert_eq!(v.as_deref(), Some("0"));
    }

    // -- happy path: nested table --

    #[test]
    fn descend_into_subtable_then_string_leaf() {
        let bfbs = build_schema();
        let buf = build_order(Some(("alice@example.com", "Alice")), 1, None);
        let v = run("Order:customer.name", &buf, &bfbs).unwrap();
        assert_eq!(v.as_deref(), Some("Alice"));
    }

    #[test]
    fn descend_into_subtable_then_string_leaf_by_id() {
        let bfbs = build_schema();
        let buf = build_order(Some(("alice@example.com", "Alice")), 1, None);
        // customer is field id 0 on Order; email is field id 0 on
        // Customer.
        let v = run("Order:#0.#0", &buf, &bfbs).unwrap();
        assert_eq!(v.as_deref(), Some("alice@example.com"));
    }

    // -- nullability --

    #[test]
    fn absent_string_returns_none() {
        let bfbs = build_schema();
        let buf = build_order(None, 1, None);
        let v = run("Order:note", &buf, &bfbs).unwrap();
        assert!(v.is_none());
    }

    #[test]
    fn present_empty_string_returns_some_empty() {
        let bfbs = build_schema();
        let buf = build_order(None, 1, Some(""));
        let v = run("Order:note", &buf, &bfbs).unwrap();
        // Distinct from absent: vtable slot is non-zero, points to a
        // zero-length string.
        assert_eq!(v.as_deref(), Some(""));
    }

    #[test]
    fn absent_subtable_short_circuits_to_none() {
        let bfbs = build_schema();
        let buf = build_order(None, 1, None);
        let v = run("Order:customer.name", &buf, &bfbs).unwrap();
        assert!(v.is_none());
    }

    // -- error variants --

    #[test]
    fn unknown_field_name_errors() {
        let bfbs = build_schema();
        let buf = build_order(None, 1, None);
        let err = run("Order:nope", &buf, &bfbs).unwrap_err();
        assert!(
            matches!(&err, ExecuteError::FieldNotFound { what, table }
                if what == "nope" && table == "Order"),
            "got {err:?}"
        );
    }

    #[test]
    fn unknown_field_id_errors() {
        let bfbs = build_schema();
        let buf = build_order(None, 1, None);
        let err = run("Order:#99", &buf, &bfbs).unwrap_err();
        assert!(
            matches!(&err, ExecuteError::FieldNotFound { what, .. } if what == "#99"),
            "got {err:?}"
        );
    }

    #[test]
    fn vector_step_errors_with_unsupported_step() {
        let bfbs = build_schema();
        let buf = build_order(None, 1, None);
        // Synthesize a Query with a `Step::All` even though the
        // schema has no vector — execute() should still reject
        // with UnsupportedStep before touching the schema.
        let q = Query {
            schema: None,
            root: "Order".to_owned(),
            steps: vec![Step::All],
        };
        let schema = root_as_schema(&bfbs).unwrap();
        let err = execute(&buf, &schema, &q, &Bounds::default()).unwrap_err();
        assert!(matches!(err, ExecuteError::UnsupportedStep { what: "[*]" }));
    }

    #[test]
    fn map_key_step_errors_with_unsupported_step() {
        let bfbs = build_schema();
        let buf = build_order(None, 1, None);
        let q = Query {
            schema: None,
            root: "Order".to_owned(),
            steps: vec![Step::MapKey(MapKey::Text("x".to_owned()))],
        };
        let schema = root_as_schema(&bfbs).unwrap();
        let err = execute(&buf, &schema, &q, &Bounds::default()).unwrap_err();
        assert!(matches!(
            err,
            ExecuteError::UnsupportedStep { what: "[map-key]" }
        ));
    }

    #[test]
    fn descending_into_scalar_errors_with_unsupported_type() {
        let bfbs = build_schema();
        let buf = build_order(Some(("a@b", "A")), 1, None);
        // `id` is `int`; trying to descend `id.foo` should fail
        // because `int` is not a sub-table.
        let err = run("Order:id.foo", &buf, &bfbs).unwrap_err();
        assert!(
            matches!(&err, ExecuteError::UnsupportedType { field, type_name }
                if field == "id" && *type_name == "int"),
            "got {err:?}"
        );
    }

    #[test]
    fn verifier_failure_propagates() {
        let bfbs = build_schema();
        // Garbage buffer (4 bytes, root offset out of range).
        let err = run("Order:id", &[0u8, 1, 2, 3], &bfbs).unwrap_err();
        assert!(matches!(err, ExecuteError::Verify(_)), "got {err:?}");
    }

    // ---------------------------------------------------------------
    // Vector fixtures + tests
    // ---------------------------------------------------------------

    /// Build a vector-bearing schema, kept separate from
    /// `build_schema()` so the two fixtures evolve independently:
    ///
    /// ```text
    /// table Item {
    ///   sku: string;          // id 0, vtable offset 4
    /// }
    /// table Bag {
    ///   flags: [bool];        // id 0, vtable offset 4
    ///   items: [Item];        // id 1, vtable offset 6
    ///   nums:  [int];         // id 2, vtable offset 8
    ///   tags:  [string];      // id 3, vtable offset 10
    /// }
    /// root_type Bag;
    /// ```
    ///
    /// Field vectors are sorted alphabetically (flags, items, nums,
    /// tags). Object vector is sorted (Bag < Item).
    fn build_vec_schema() -> Vec<u8> {
        let mut fbb = FlatBufferBuilder::new();

        let str_t = Type::create(
            &mut fbb,
            &TypeArgs {
                base_type: BaseType::String,
                ..Default::default()
            },
        );

        // -- Item.sku: string (single-field table) --
        // Marked `(key)` so the `Step::MapKey` tests can do
        // `Bag:items[abc].sku` lookups against this fixture. The
        // existing `[i]` and `[*]` tests don't observe the `key`
        // flag, so this annotation is non-breaking for them.
        let sku_n = fbb.create_string("sku");
        let sku_f = RField::create(
            &mut fbb,
            &FieldArgs {
                name: Some(sku_n),
                type_: Some(str_t),
                id: 0,
                offset: 4,
                key: true,
                ..Default::default()
            },
        );
        let item_fields = fbb.create_vector(&[sku_f]);
        let item_n = fbb.create_string("Item");
        let item = RObject::create(
            &mut fbb,
            &ObjectArgs {
                name: Some(item_n),
                fields: Some(item_fields),
                ..Default::default()
            },
        );

        // -- Vector element types --
        // Object index 1 = Item (Bag is 0, Item is 1 once sorted).
        let vec_bool_t = Type::create(
            &mut fbb,
            &TypeArgs {
                base_type: BaseType::Vector,
                element: BaseType::Bool,
                ..Default::default()
            },
        );
        let vec_item_t = Type::create(
            &mut fbb,
            &TypeArgs {
                base_type: BaseType::Vector,
                element: BaseType::Obj,
                index: 1,
                ..Default::default()
            },
        );
        let vec_int_t = Type::create(
            &mut fbb,
            &TypeArgs {
                base_type: BaseType::Vector,
                element: BaseType::Int,
                ..Default::default()
            },
        );
        let vec_str_t = Type::create(
            &mut fbb,
            &TypeArgs {
                base_type: BaseType::Vector,
                element: BaseType::String,
                ..Default::default()
            },
        );

        // -- Bag fields (sorted: flags, items, nums, tags) --
        let flags_n = fbb.create_string("flags");
        let flags_f = RField::create(
            &mut fbb,
            &FieldArgs {
                name: Some(flags_n),
                type_: Some(vec_bool_t),
                id: 0,
                offset: 4,
                ..Default::default()
            },
        );
        let items_n = fbb.create_string("items");
        let items_f = RField::create(
            &mut fbb,
            &FieldArgs {
                name: Some(items_n),
                type_: Some(vec_item_t),
                id: 1,
                offset: 6,
                ..Default::default()
            },
        );
        let nums_n = fbb.create_string("nums");
        let nums_f = RField::create(
            &mut fbb,
            &FieldArgs {
                name: Some(nums_n),
                type_: Some(vec_int_t),
                id: 2,
                offset: 8,
                ..Default::default()
            },
        );
        let tags_n = fbb.create_string("tags");
        let tags_f = RField::create(
            &mut fbb,
            &FieldArgs {
                name: Some(tags_n),
                type_: Some(vec_str_t),
                id: 3,
                offset: 10,
                ..Default::default()
            },
        );
        let bag_fields = fbb.create_vector(&[flags_f, items_f, nums_f, tags_f]);
        let bag_n = fbb.create_string("Bag");
        let bag = RObject::create(
            &mut fbb,
            &ObjectArgs {
                name: Some(bag_n),
                fields: Some(bag_fields),
                ..Default::default()
            },
        );

        // Objects sorted by name: Bag (0), Item (1).
        let objects = fbb.create_vector(&[bag, item]);
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

    /// Build a `Bag` buffer. Each argument may be `None` to elide the
    /// vector slot entirely (covers the absent-vector path); an
    /// empty slice produces a present zero-length vector.
    fn build_bag(
        items: Option<&[&str]>,
        tags: Option<&[&str]>,
        nums: Option<&[i32]>,
        flags: Option<&[bool]>,
    ) -> Vec<u8> {
        let mut fbb = FlatBufferBuilder::new();

        // Build vectors first so their offsets are known before we
        // open the Bag table.
        let items_off = items.map(|skus| {
            // Each Item is its own table; build them, collect
            // offsets, then create_vector over them.
            let item_offs: Vec<_> = skus
                .iter()
                .map(|sku| {
                    let sku_off = fbb.create_string(sku);
                    let t = fbb.start_table();
                    fbb.push_slot_always(4, sku_off);
                    fbb.end_table(t)
                })
                .collect();
            fbb.create_vector(&item_offs)
        });
        let tags_off = tags.map(|ts| {
            let tag_offs: Vec<_> = ts.iter().map(|t| fbb.create_string(t)).collect();
            fbb.create_vector(&tag_offs)
        });
        let nums_off = nums.map(|ns| fbb.create_vector(ns));
        let flags_off = flags.map(|fs| fbb.create_vector(fs));

        let t = fbb.start_table();
        if let Some(off) = flags_off {
            fbb.push_slot_always(4, off);
        }
        if let Some(off) = items_off {
            fbb.push_slot_always(6, off);
        }
        if let Some(off) = nums_off {
            fbb.push_slot_always(8, off);
        }
        if let Some(off) = tags_off {
            fbb.push_slot_always(10, off);
        }
        let bag = fbb.end_table(t);
        fbb.finish_minimal(bag);
        fbb.finished_data().to_vec()
    }

    // -- struct fixtures (struct descent: design §7.2 / §4.3) --

    /// Build a schema with a struct field on a table:
    ///
    /// ```text
    /// struct Vec3 { x:float; y:float; z:float; }   // bytesize 12, align 4
    /// struct Inner { a:int; b:int; }               // bytesize 8,  align 4
    /// struct Outer { lo:Inner; hi:Inner; }         // bytesize 16, align 4
    /// table Point {
    ///   name:  string;   // id 0, vtable offset 4
    ///   pos:   Vec3;     // id 1, vtable offset 6 (struct, inline)
    ///   o:     Outer;    // id 2, vtable offset 8 (struct, inline)
    /// }
    /// root_type Point;
    /// ```
    ///
    /// Object vector is sorted alphabetically:
    /// `Inner` (0), `Outer` (1), `Point` (2), `Vec3` (3).
    fn build_struct_schema() -> Vec<u8> {
        let mut fbb = FlatBufferBuilder::new();

        // -- Scalar leaf types --
        let int_t = Type::create(
            &mut fbb,
            &TypeArgs {
                base_type: BaseType::Int,
                ..Default::default()
            },
        );
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

        // -- struct Inner { a:int @0; b:int @4; } --
        let inner_a_n = fbb.create_string("a");
        let inner_a = RField::create(
            &mut fbb,
            &FieldArgs {
                name: Some(inner_a_n),
                type_: Some(int_t),
                id: 0,
                offset: 0, // byte offset within the struct
                ..Default::default()
            },
        );
        let inner_b_n = fbb.create_string("b");
        let inner_b = RField::create(
            &mut fbb,
            &FieldArgs {
                name: Some(inner_b_n),
                type_: Some(int_t),
                id: 1,
                offset: 4,
                ..Default::default()
            },
        );
        let inner_fields = fbb.create_vector(&[inner_a, inner_b]);
        let inner_n = fbb.create_string("Inner");
        let inner = RObject::create(
            &mut fbb,
            &ObjectArgs {
                name: Some(inner_n),
                fields: Some(inner_fields),
                is_struct: true,
                bytesize: 8,
                minalign: 4,
                ..Default::default()
            },
        );

        // -- struct Outer { lo:Inner @0; hi:Inner @8; } --
        // Inner sorts as object index 0.
        let inner_obj_t = Type::create(
            &mut fbb,
            &TypeArgs {
                base_type: BaseType::Obj,
                index: 0,
                ..Default::default()
            },
        );
        let outer_hi_n = fbb.create_string("hi");
        let outer_hi = RField::create(
            &mut fbb,
            &FieldArgs {
                name: Some(outer_hi_n),
                type_: Some(inner_obj_t),
                id: 1,
                offset: 8,
                ..Default::default()
            },
        );
        let outer_lo_n = fbb.create_string("lo");
        let outer_lo = RField::create(
            &mut fbb,
            &FieldArgs {
                name: Some(outer_lo_n),
                type_: Some(inner_obj_t),
                id: 0,
                offset: 0,
                ..Default::default()
            },
        );
        // Fields sorted alphabetically: hi (h) < lo (l).
        let outer_fields = fbb.create_vector(&[outer_hi, outer_lo]);
        let outer_n = fbb.create_string("Outer");
        let outer = RObject::create(
            &mut fbb,
            &ObjectArgs {
                name: Some(outer_n),
                fields: Some(outer_fields),
                is_struct: true,
                bytesize: 16,
                minalign: 4,
                ..Default::default()
            },
        );

        // -- struct Vec3 { x:float @0; y:float @4; z:float @8; } --
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

        // -- Table Point { name:string, pos:Vec3, o:Outer } --
        // Outer sorts as object index 1, Vec3 as 3.
        let outer_obj_t = Type::create(
            &mut fbb,
            &TypeArgs {
                base_type: BaseType::Obj,
                index: 1,
                ..Default::default()
            },
        );
        let vec3_obj_t = Type::create(
            &mut fbb,
            &TypeArgs {
                base_type: BaseType::Obj,
                index: 3,
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
        let point_o_n = fbb.create_string("o");
        let point_o = RField::create(
            &mut fbb,
            &FieldArgs {
                name: Some(point_o_n),
                type_: Some(outer_obj_t),
                id: 2,
                offset: 8,
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
        // Field vector sorted alphabetically: name, o, pos.
        let point_fields = fbb.create_vector(&[point_name, point_o, point_pos]);
        let point_n = fbb.create_string("Point");
        let point = RObject::create(
            &mut fbb,
            &ObjectArgs {
                name: Some(point_n),
                fields: Some(point_fields),
                ..Default::default()
            },
        );

        // Object vector sorted alphabetically: Inner, Outer, Point, Vec3.
        let objects = fbb.create_vector(&[inner, outer, point, vec3]);
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

    /// `Vec3` mirror used by the `Push` impl below to write a struct
    /// inline into a `Point` table. `repr(C, packed)` matches the
    /// FlatBuffers wire layout for structs (no compiler padding).
    #[repr(C, packed)]
    #[derive(Clone, Copy)]
    struct TestVec3 {
        x: f32,
        y: f32,
        z: f32,
    }

    // SAFETY: `TestVec3` is `repr(C, packed)` so its in-memory bytes
    // match the on-wire little-endian layout on every supported
    // host (x86_64, aarch64). `flatbuffers::Push::size()` defaults
    // to `size_of::<Self::Output>()` (= 12) and `alignment()` to
    // `align_of::<Self::Output>()` (= 4), which matches the
    // `bytesize`/`minalign` we declared in the schema.
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

    /// `Outer { lo:Inner, hi:Inner }` mirror — same `Push`
    /// strategy as [`TestVec3`].
    #[repr(C, packed)]
    #[derive(Clone, Copy)]
    struct TestOuter {
        lo_a: i32,
        lo_b: i32,
        hi_a: i32,
        hi_b: i32,
    }

    // SAFETY: see [`TestVec3`].
    impl flatbuffers::Push for TestOuter {
        type Output = TestOuter;
        unsafe fn push(&self, dst: &mut [u8], _written_len: usize) {
            let src = std::slice::from_raw_parts(
                self as *const Self as *const u8,
                std::mem::size_of::<Self>(),
            );
            dst[..src.len()].copy_from_slice(src);
        }
    }

    /// Build a `Point` buffer matching `build_struct_schema()`.
    /// `name`, `pos`, and `o` may be independently absent.
    fn build_point_buf(name: Option<&str>, pos: Option<TestVec3>, o: Option<TestOuter>) -> Vec<u8> {
        let mut fbb = FlatBufferBuilder::new();
        let name_off = name.map(|s| fbb.create_string(s));

        let t = fbb.start_table();
        if let Some(off) = name_off {
            fbb.push_slot_always(4, off); // slot 4 = name
        }
        if let Some(v) = pos {
            // Inline struct push at slot 6 = pos.
            fbb.push_slot_always(6, v);
        }
        if let Some(v) = o {
            // Inline struct push at slot 8 = o.
            fbb.push_slot_always(8, v);
        }
        let point = fbb.end_table(t);
        fbb.finish_minimal(point);
        fbb.finished_data().to_vec()
    }

    /// Test helper: execute against the `Point` schema and return
    /// the first leaf.
    fn run_struct(
        query_str: &str,
        buf: &[u8],
        bfbs: &[u8],
    ) -> Result<Option<String>, ExecuteError> {
        let schema = root_as_schema(bfbs).expect("test schema verifies");
        let query = parse(query_str).expect("test query parses");
        execute(buf, &schema, &query, &Bounds::default()).map(|v| v.into_iter().next().flatten())
    }

    /// Test helper: execute against the `Point` schema and return
    /// the raw `Vec<Option<String>>` (used by the unsupported-step
    /// tests to confirm the error variant).
    fn run_struct_err(query_str: &str, buf: &[u8], bfbs: &[u8]) -> ExecuteError {
        let schema = root_as_schema(bfbs).expect("test schema verifies");
        let query = parse(query_str).expect("test query parses");
        execute(buf, &schema, &query, &Bounds::default()).unwrap_err()
    }

    /// Test helper: execute against the `Point` schema and unwrap to
    /// the first leaf. The fanout (`Step::All`) tests use
    /// [`run_vec_all`] instead to inspect the whole vec.
    fn run_vec(query_str: &str, buf: &[u8], bfbs: &[u8]) -> Result<Option<String>, ExecuteError> {
        run_vec_all(query_str, buf, bfbs).map(|v| v.into_iter().next().flatten())
    }

    fn run_vec_all(
        query_str: &str,
        buf: &[u8],
        bfbs: &[u8],
    ) -> Result<Vec<Option<String>>, ExecuteError> {
        let schema = root_as_schema(bfbs).expect("test schema verifies");
        let query = parse(query_str).expect("test query parses");
        execute(buf, &schema, &query, &Bounds::default())
    }

    // -- vector of strings --

    #[test]
    fn vector_string_index_in_range() {
        let bfbs = build_vec_schema();
        let buf = build_bag(None, Some(&["red", "green", "blue"]), None, None);
        let v = run_vec("Bag:tags[0]", &buf, &bfbs).unwrap();
        assert_eq!(v.as_deref(), Some("red"));
        let v = run_vec("Bag:tags[2]", &buf, &bfbs).unwrap();
        assert_eq!(v.as_deref(), Some("blue"));
    }

    #[test]
    fn vector_string_index_out_of_bounds_returns_none() {
        let bfbs = build_vec_schema();
        let buf = build_bag(None, Some(&["red"]), None, None);
        let v = run_vec("Bag:tags[99]", &buf, &bfbs).unwrap();
        // Per design §4.3, OOB indices short-circuit to NULL —
        // explicitly distinct from `ERROR`.
        assert!(v.is_none());
    }

    #[test]
    fn vector_string_empty_index_returns_none() {
        let bfbs = build_vec_schema();
        // Present-but-empty vector: vtable slot points to a Vector
        // with length 0. Index 0 is OOB.
        let buf = build_bag(None, Some(&[]), None, None);
        let v = run_vec("Bag:tags[0]", &buf, &bfbs).unwrap();
        assert!(v.is_none());
    }

    #[test]
    fn vector_absent_vtable_slot_returns_none() {
        let bfbs = build_vec_schema();
        let buf = build_bag(None, None, None, None); // no slots set
        let v = run_vec("Bag:tags[0]", &buf, &bfbs).unwrap();
        assert!(v.is_none());
    }

    // -- vector of scalars --

    #[test]
    fn vector_int_index_in_range() {
        let bfbs = build_vec_schema();
        let buf = build_bag(None, None, Some(&[10, 20, 30]), None);
        let v = run_vec("Bag:nums[1]", &buf, &bfbs).unwrap();
        assert_eq!(v.as_deref(), Some("20"));
    }

    #[test]
    fn vector_bool_renders_as_zero_or_one() {
        let bfbs = build_vec_schema();
        let buf = build_bag(None, None, None, Some(&[true, false]));
        // Bool elements stringify via `u8::Display` → "0" / "1",
        // matching the way scalar bool *fields* render through the
        // upstream `get_any_field_string` path.
        assert_eq!(
            run_vec("Bag:flags[0]", &buf, &bfbs).unwrap().as_deref(),
            Some("1")
        );
        assert_eq!(
            run_vec("Bag:flags[1]", &buf, &bfbs).unwrap().as_deref(),
            Some("0")
        );
    }

    // -- vector of tables --

    #[test]
    fn vector_of_tables_descend_then_string_leaf() {
        let bfbs = build_vec_schema();
        let buf = build_bag(Some(&["ABC", "DEF", "GHI"]), None, None, None);
        let v = run_vec("Bag:items[1].sku", &buf, &bfbs).unwrap();
        assert_eq!(v.as_deref(), Some("DEF"));
    }

    #[test]
    fn vector_of_tables_oob_then_field_returns_none() {
        let bfbs = build_vec_schema();
        let buf = build_bag(Some(&["ABC"]), None, None, None);
        let v = run_vec("Bag:items[5].sku", &buf, &bfbs).unwrap();
        assert!(v.is_none());
    }

    // -- error paths --

    #[test]
    fn bare_vector_at_leaf_errors_with_unsupported_type() {
        let bfbs = build_vec_schema();
        let buf = build_bag(None, Some(&["x"]), None, None);
        // `Bag:tags` (no `[i]`) has no v0.1 textual form.
        let err = run_vec("Bag:tags", &buf, &bfbs).unwrap_err();
        assert!(
            matches!(&err, ExecuteError::UnsupportedType { field, .. } if field == "tags"),
            "got {err:?}"
        );
    }

    #[test]
    fn vector_of_tables_element_at_leaf_errors_with_unsupported_type() {
        let bfbs = build_vec_schema();
        let buf = build_bag(Some(&["ABC"]), None, None, None);
        // `Bag:items[0]` would yield a sub-table value; can't
        // stringify.
        let err = run_vec("Bag:items[0]", &buf, &bfbs).unwrap_err();
        assert!(
            matches!(&err, ExecuteError::UnsupportedType { field, .. } if field == "items"),
            "got {err:?}"
        );
    }

    #[test]
    fn descend_through_scalar_vector_errors_with_unsupported_type() {
        let bfbs = build_vec_schema();
        let buf = build_bag(None, None, Some(&[1, 2, 3]), None);
        // `nums[0].foo` — can't descend into a scalar element.
        let err = run_vec("Bag:nums[0].foo", &buf, &bfbs).unwrap_err();
        assert!(
            matches!(&err, ExecuteError::UnsupportedType { field, .. } if field == "nums"),
            "got {err:?}"
        );
    }

    // -- Step::All fanout --

    #[test]
    fn vector_all_strings() {
        let bfbs = build_vec_schema();
        let buf = build_bag(None, Some(&["red", "green", "blue"]), None, None);
        let v = run_vec_all("Bag:tags[*]", &buf, &bfbs).expect("ok");
        assert_eq!(
            v,
            vec![
                Some("red".to_owned()),
                Some("green".to_owned()),
                Some("blue".to_owned()),
            ],
        );
    }

    #[test]
    fn vector_all_strings_empty_vector() {
        let bfbs = build_vec_schema();
        // tags is present but empty.
        let buf = build_bag(None, Some(&[]), None, None);
        let v = run_vec_all("Bag:tags[*]", &buf, &bfbs).expect("ok");
        assert!(v.is_empty(), "got {v:?}");
    }

    #[test]
    fn vector_all_strings_absent_vector() {
        let bfbs = build_vec_schema();
        // tags is absent (vtable slot 0).
        let buf = build_bag(None, None, None, None);
        let v = run_vec_all("Bag:tags[*]", &buf, &bfbs).expect("ok");
        assert!(v.is_empty(), "got {v:?}");
    }

    #[test]
    fn vector_all_scalar_ints() {
        let bfbs = build_vec_schema();
        let buf = build_bag(None, None, Some(&[10, 20, 30]), None);
        let v = run_vec_all("Bag:nums[*]", &buf, &bfbs).expect("ok");
        assert_eq!(
            v,
            vec![
                Some("10".to_owned()),
                Some("20".to_owned()),
                Some("30".to_owned()),
            ],
        );
    }

    #[test]
    fn vector_all_scalar_bools() {
        let bfbs = build_vec_schema();
        // bool elements stringify as "1" / "0" to match
        // `read_vector_element` (and the upstream
        // `get_any_field_string` form for scalar bool fields).
        let buf = build_bag(None, None, None, Some(&[true, false, true]));
        let v = run_vec_all("Bag:flags[*]", &buf, &bfbs).expect("ok");
        assert_eq!(
            v,
            vec![
                Some("1".to_owned()),
                Some("0".to_owned()),
                Some("1".to_owned()),
            ],
        );
    }

    #[test]
    fn vector_all_table_field_descent() {
        let bfbs = build_vec_schema();
        // Three items with skus "a", "b", "c".
        let buf = build_bag(Some(&["a", "b", "c"]), None, None, None);
        let v = run_vec_all("Bag:items[*].sku", &buf, &bfbs).expect("ok");
        assert_eq!(
            v,
            vec![
                Some("a".to_owned()),
                Some("b".to_owned()),
                Some("c".to_owned()),
            ],
        );
    }

    #[test]
    fn vector_all_table_field_absent_intermediate() {
        let bfbs = build_vec_schema();
        // build_bag's items always set sku, so use empty-string sku
        // to exercise the "present but empty" path; absent-sku
        // requires a custom builder. The fanout still emits one
        // entry per element regardless.
        let buf = build_bag(Some(&["x", "", "y"]), None, None, None);
        let v = run_vec_all("Bag:items[*].sku", &buf, &bfbs).expect("ok");
        assert_eq!(
            v,
            vec![
                Some("x".to_owned()),
                Some("".to_owned()),
                Some("y".to_owned()),
            ],
        );
    }

    #[test]
    fn vector_all_table_with_no_descent_errors() {
        let bfbs = build_vec_schema();
        let buf = build_bag(Some(&["a"]), None, None, None);
        // `Bag:items[*]` lands at a sub-table value with no textual
        // form — same rationale as `Bag:items[0]`.
        let err = run_vec_all("Bag:items[*]", &buf, &bfbs).unwrap_err();
        assert!(
            matches!(&err, ExecuteError::UnsupportedType { field, .. } if field == "items"),
            "got {err:?}"
        );
    }

    #[test]
    fn vector_all_scalar_with_descent_errors() {
        let bfbs = build_vec_schema();
        let buf = build_bag(None, None, Some(&[1, 2, 3]), None);
        // `Bag:nums[*].foo` — can't descend into a scalar element.
        let err = run_vec_all("Bag:nums[*].foo", &buf, &bfbs).unwrap_err();
        assert!(
            matches!(&err, ExecuteError::UnsupportedType { field, .. } if field == "nums"),
            "got {err:?}"
        );
    }

    // -- Step::MapKey --

    #[test]
    fn vector_map_key_hit() {
        let bfbs = build_vec_schema();
        let buf = build_bag(Some(&["a", "b", "c"]), None, None, None);
        // Linear scan finds the element whose `(key)`-annotated
        // `sku` field equals "b", then descends with `.sku` to
        // stringify it.
        let v = run_vec("Bag:items[b].sku", &buf, &bfbs).expect("ok");
        assert_eq!(v.as_deref(), Some("b"));
    }

    #[test]
    fn vector_map_key_first_hit_in_wire_order() {
        let bfbs = build_vec_schema();
        // Two entries with sku = "dup": linear scan returns the
        // first in wire order (matching the §10
        // `key_lookup_strict = off` fallback semantics that this
        // slice ships unconditionally).
        let buf = build_bag(Some(&["dup", "x", "dup"]), None, None, None);
        let v = run_vec("Bag:items[dup].sku", &buf, &bfbs).expect("ok");
        assert_eq!(v.as_deref(), Some("dup"));
    }

    #[test]
    fn vector_map_key_miss_returns_none() {
        let bfbs = build_vec_schema();
        let buf = build_bag(Some(&["a", "b"]), None, None, None);
        let v = run_vec("Bag:items[zzz].sku", &buf, &bfbs).expect("ok");
        assert!(v.is_none(), "got {v:?}");
    }

    #[test]
    fn vector_map_key_empty_vector_returns_none() {
        let bfbs = build_vec_schema();
        let buf = build_bag(Some(&[]), None, None, None);
        let v = run_vec("Bag:items[abc].sku", &buf, &bfbs).expect("ok");
        assert!(v.is_none(), "got {v:?}");
    }

    #[test]
    fn vector_map_key_absent_vector_returns_none() {
        let bfbs = build_vec_schema();
        // items field elided entirely.
        let buf = build_bag(None, None, None, None);
        let v = run_vec("Bag:items[abc].sku", &buf, &bfbs).expect("ok");
        assert!(v.is_none(), "got {v:?}");
    }

    #[test]
    fn vector_map_key_against_scalar_vector_errors() {
        let bfbs = build_vec_schema();
        // `tags` is a vector of strings, not a vector of tables;
        // map-key lookup isn't defined for it.
        let buf = build_bag(None, Some(&["a", "b"]), None, None);
        let err = run_vec("Bag:tags[a]", &buf, &bfbs).unwrap_err();
        assert!(
            matches!(&err, ExecuteError::UnsupportedType { field, .. } if field == "tags"),
            "got {err:?}"
        );
    }

    #[test]
    fn vector_map_key_no_descent_errors() {
        let bfbs = build_vec_schema();
        // `Bag:items[abc]` lands at a sub-table value with no v0.1
        // textual form — same rationale as `Bag:items[0]`.
        let buf = build_bag(Some(&["abc"]), None, None, None);
        let err = run_vec("Bag:items[abc]", &buf, &bfbs).unwrap_err();
        assert!(
            matches!(&err, ExecuteError::UnsupportedType { field, .. } if field == "items"),
            "got {err:?}"
        );
    }

    // -- Step::MapKeys --

    #[test]
    fn vector_map_keys_fans_out_keys_in_wire_order() {
        let bfbs = build_vec_schema();
        let buf = build_bag(Some(&["a", "b", "c"]), None, None, None);
        let v = run_vec_all("Bag:items|keys", &buf, &bfbs).expect("ok");
        assert_eq!(
            v,
            vec![
                Some("a".to_owned()),
                Some("b".to_owned()),
                Some("c".to_owned()),
            ],
        );
    }

    #[test]
    fn vector_map_keys_preserves_duplicates() {
        let bfbs = build_vec_schema();
        // Linear / wire-order fanout: duplicates are NOT collapsed
        // (the §10 `key_lookup_strict = off` fallback semantics that
        // this slice ships unconditionally).
        let buf = build_bag(Some(&["dup", "x", "dup"]), None, None, None);
        let v = run_vec_all("Bag:items|keys", &buf, &bfbs).expect("ok");
        assert_eq!(
            v,
            vec![
                Some("dup".to_owned()),
                Some("x".to_owned()),
                Some("dup".to_owned()),
            ],
        );
    }

    #[test]
    fn vector_map_keys_empty_vector_returns_empty_vec() {
        let bfbs = build_vec_schema();
        let buf = build_bag(Some(&[]), None, None, None);
        let v = run_vec_all("Bag:items|keys", &buf, &bfbs).expect("ok");
        assert!(v.is_empty(), "got {v:?}");
    }

    #[test]
    fn vector_map_keys_absent_vector_returns_empty_vec() {
        let bfbs = build_vec_schema();
        let buf = build_bag(None, None, None, None);
        let v = run_vec_all("Bag:items|keys", &buf, &bfbs).expect("ok");
        assert!(v.is_empty(), "got {v:?}");
    }

    #[test]
    fn vector_map_keys_against_scalar_vector_errors() {
        let bfbs = build_vec_schema();
        // `tags` is a vector of strings; `|keys` only works over
        // vectors of `(key)`-annotated tables.
        let buf = build_bag(None, Some(&["a", "b"]), None, None);
        let err = run_vec_all("Bag:tags|keys", &buf, &bfbs).unwrap_err();
        assert!(
            matches!(&err, ExecuteError::UnsupportedType { field, .. } if field == "tags"),
            "got {err:?}"
        );
    }

    #[test]
    fn vector_map_keys_with_trailing_step_errors() {
        let bfbs = build_vec_schema();
        // Parser allows `items|keys.foo`; executor rejects because
        // the keys are themselves the leaves.
        let buf = build_bag(Some(&["a"]), None, None, None);
        let err = run_vec_all("Bag:items|keys.foo", &buf, &bfbs).unwrap_err();
        assert!(
            matches!(&err, ExecuteError::UnsupportedType { field, .. } if field == "items"),
            "got {err:?}"
        );
    }

    // -- struct descent (design §7.2 / §4.3) --

    #[test]
    fn struct_field_scalar_leaves() {
        let bfbs = build_struct_schema();
        let buf = build_point_buf(
            Some("p"),
            Some(TestVec3 {
                x: 1.0,
                y: 2.0,
                z: 3.0,
            }),
            None,
        );
        // Each scalar field on the inline `Vec3` struct stringifies
        // through the same `f32::Display` path that table-level
        // scalars use, so values match what the same value would
        // render as in a non-struct field.
        assert_eq!(
            run_struct("Point:pos.x", &buf, &bfbs)
                .expect("ok")
                .as_deref(),
            Some("1")
        );
        assert_eq!(
            run_struct("Point:pos.y", &buf, &bfbs)
                .expect("ok")
                .as_deref(),
            Some("2")
        );
        assert_eq!(
            run_struct("Point:pos.z", &buf, &bfbs)
                .expect("ok")
                .as_deref(),
            Some("3")
        );
    }

    #[test]
    fn struct_field_scalar_leaf_by_id() {
        let bfbs = build_struct_schema();
        let buf = build_point_buf(
            None,
            Some(TestVec3 {
                x: 7.5,
                y: 0.0,
                z: 0.0,
            }),
            None,
        );
        // Field id lookup inside structs goes through the same
        // `find_field` linear-by-id scan as tables.
        let v = run_struct("Point:pos.#0", &buf, &bfbs).expect("ok");
        assert_eq!(v.as_deref(), Some("7.5"));
    }

    #[test]
    fn struct_field_absent_returns_none() {
        let bfbs = build_struct_schema();
        // `pos` elided entirely from the buffer (vtable slot 0).
        let buf = build_point_buf(Some("p"), None, None);
        let v = run_struct("Point:pos.x", &buf, &bfbs).expect("ok");
        assert!(v.is_none(), "got {v:?}");
    }

    #[test]
    fn struct_at_leaf_errors() {
        let bfbs = build_struct_schema();
        let buf = build_point_buf(
            None,
            Some(TestVec3 {
                x: 1.0,
                y: 2.0,
                z: 3.0,
            }),
            None,
        );
        // `Point:pos` lands at the struct itself with no v0.1
        // textual form (would need a JSON-shaped output, which is
        // the §8 round-trip slice).
        let err = run_struct_err("Point:pos", &buf, &bfbs);
        assert!(
            matches!(&err, ExecuteError::UnsupportedType { field, .. } if field == "pos"),
            "got {err:?}"
        );
    }

    #[test]
    fn struct_with_index_step_errors() {
        let bfbs = build_struct_schema();
        let buf = build_point_buf(
            None,
            Some(TestVec3 {
                x: 1.0,
                y: 2.0,
                z: 3.0,
            }),
            None,
        );
        // Structs hold no vectors, so `[i]`, `[*]`, `[k]`, and
        // `|keys` are all static type-system mismatches at this
        // position. The error is raised by `walk_struct` rather
        // than the parser (the parser doesn't know the type).
        let err = run_struct_err("Point:pos[0]", &buf, &bfbs);
        assert!(
            matches!(&err, ExecuteError::UnsupportedType { field, .. } if field == "Vec3"),
            "got {err:?}"
        );
    }

    #[test]
    fn struct_with_all_step_errors() {
        let bfbs = build_struct_schema();
        let buf = build_point_buf(
            None,
            Some(TestVec3 {
                x: 1.0,
                y: 2.0,
                z: 3.0,
            }),
            None,
        );
        let err = run_struct_err("Point:pos[*]", &buf, &bfbs);
        assert!(
            matches!(&err, ExecuteError::UnsupportedType { field, .. } if field == "Vec3"),
            "got {err:?}"
        );
    }

    #[test]
    fn struct_with_keys_step_errors() {
        let bfbs = build_struct_schema();
        let buf = build_point_buf(
            None,
            Some(TestVec3 {
                x: 1.0,
                y: 2.0,
                z: 3.0,
            }),
            None,
        );
        let err = run_struct_err("Point:pos|keys", &buf, &bfbs);
        assert!(
            matches!(&err, ExecuteError::UnsupportedType { field, .. } if field == "Vec3"),
            "got {err:?}"
        );
    }

    #[test]
    fn nested_struct_descent_lo() {
        let bfbs = build_struct_schema();
        let buf = build_point_buf(
            None,
            None,
            Some(TestOuter {
                lo_a: 11,
                lo_b: 12,
                hi_a: 21,
                hi_b: 22,
            }),
        );
        // `Point:o.lo.a` = recursive `walk_struct` (Outer → Inner).
        // The byte offsets accumulate: Outer.lo @ 0, Inner.a @ 0.
        let v = run_struct("Point:o.lo.a", &buf, &bfbs).expect("ok");
        assert_eq!(v.as_deref(), Some("11"));
        let v = run_struct("Point:o.lo.b", &buf, &bfbs).expect("ok");
        assert_eq!(v.as_deref(), Some("12"));
    }

    #[test]
    fn nested_struct_descent_hi() {
        let bfbs = build_struct_schema();
        let buf = build_point_buf(
            None,
            None,
            Some(TestOuter {
                lo_a: 11,
                lo_b: 12,
                hi_a: 21,
                hi_b: 22,
            }),
        );
        // Outer.hi @ 8, Inner.b @ 4 → struct_loc + 12.
        let v = run_struct("Point:o.hi.a", &buf, &bfbs).expect("ok");
        assert_eq!(v.as_deref(), Some("21"));
        let v = run_struct("Point:o.hi.b", &buf, &bfbs).expect("ok");
        assert_eq!(v.as_deref(), Some("22"));
    }

    #[test]
    fn unknown_struct_field_errors() {
        let bfbs = build_struct_schema();
        let buf = build_point_buf(
            None,
            Some(TestVec3 {
                x: 1.0,
                y: 2.0,
                z: 3.0,
            }),
            None,
        );
        let err = run_struct_err("Point:pos.w", &buf, &bfbs);
        assert!(
            matches!(&err, ExecuteError::FieldNotFound { what, table }
                if what == "w" && table == "Vec3"),
            "got {err:?}"
        );
    }

    #[test]
    fn descending_into_struct_scalar_errors() {
        let bfbs = build_struct_schema();
        let buf = build_point_buf(
            None,
            Some(TestVec3 {
                x: 1.0,
                y: 2.0,
                z: 3.0,
            }),
            None,
        );
        // `Point:pos.x.foo` — `pos.x` is a float scalar; can't
        // descend further. Same shape as `Order:id.foo`.
        let err = run_struct_err("Point:pos.x.foo", &buf, &bfbs);
        assert!(
            matches!(&err, ExecuteError::UnsupportedType { field, type_name }
                if field == "x" && *type_name == "float"),
            "got {err:?}"
        );
    }

    // -----------------------------------------------------------------
    // Union dispatch fixtures + tests (design §4.3, §7.2).
    // -----------------------------------------------------------------

    /// Build the reflected schema:
    ///
    /// ```fbs
    /// table A   { name:string;   }   // object index 0
    /// table B   { count:int;     }   // object index 1
    /// union U   { A, B }             // enum   index 0
    /// table Msg { body:U;        }   // object index 2
    ///                                 //   body_type:UType @ slot 4
    ///                                 //   body:Union     @ slot 6
    /// root_type Msg;
    /// ```
    ///
    /// Object vector is sorted alphabetically: `A` (0), `B` (1),
    /// `Msg` (2). Enum vector has a single entry: `U` (0). Within
    /// `U`, `EnumVal`s are sorted by `value`: `NONE` (0), `A` (1),
    /// `B` (2) — that ordering is what lets `walk_union` use
    /// [`flatbuffers::Vector::lookup_by_key`] for the discriminator.
    fn build_union_schema() -> Vec<u8> {
        let mut fbb = FlatBufferBuilder::new();

        // Scalar types.
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

        // table A { name:string; }  -> object index 0
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

        // table B { count:int; }  -> object index 1
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
        let none_n = fbb.create_string("NONE");
        // The NONE variant has a `union_type` of `BaseType::None` —
        // it never resolves to an object, so we don't need an
        // `index` here, but we *do* emit it to mirror flatc output.
        let none_t = Type::create(
            &mut fbb,
            &TypeArgs {
                base_type: BaseType::None,
                ..Default::default()
            },
        );
        let none_ev = EnumVal::create(
            &mut fbb,
            &EnumValArgs {
                name: Some(none_n),
                value: 0,
                union_type: Some(none_t),
                ..Default::default()
            },
        );

        let a_variant_n = fbb.create_string("A");
        let a_obj_t = Type::create(
            &mut fbb,
            &TypeArgs {
                base_type: BaseType::Obj,
                index: 0, // points at object A
                ..Default::default()
            },
        );
        let a_ev = EnumVal::create(
            &mut fbb,
            &EnumValArgs {
                name: Some(a_variant_n),
                value: 1,
                union_type: Some(a_obj_t),
                ..Default::default()
            },
        );

        let b_variant_n = fbb.create_string("B");
        let b_obj_t = Type::create(
            &mut fbb,
            &TypeArgs {
                base_type: BaseType::Obj,
                index: 1, // points at object B
                ..Default::default()
            },
        );
        let b_ev = EnumVal::create(
            &mut fbb,
            &EnumValArgs {
                name: Some(b_variant_n),
                value: 2,
                union_type: Some(b_obj_t),
                ..Default::default()
            },
        );

        // EnumVals stored sorted by `value` (already 0/1/2).
        let u_values = fbb.create_vector(&[none_ev, a_ev, b_ev]);
        let u_underlying = Type::create(
            &mut fbb,
            &TypeArgs {
                base_type: BaseType::UType,
                index: 0, // self-reference is harmless here
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

        // table Msg { body:U; }  -> object index 2
        // body_type:UType  @ slot 4 (id 0)
        // body:Union       @ slot 6 (id 1)
        let body_type_n = fbb.create_string("body_type");
        let body_utype_t = Type::create(
            &mut fbb,
            &TypeArgs {
                base_type: BaseType::UType,
                index: 0, // points at enum U
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
                index: 0, // points at enum U
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

        // Field vector sorted alphabetically: "body" < "body_type".
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

        // Object vector sorted: A, B, Msg.
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

    /// Pick which variant (and what payload) to write into a `Msg`
    /// buffer built by [`build_msg_buf`].
    enum UnionVariant<'a> {
        /// Discriminator 0; both `body_type` and `body` slots are
        /// omitted (matches the wire shape flatc emits for a NONE
        /// union).
        None,
        /// Discriminator 1; pushes a `TableA` into `body` with the
        /// given `name`.
        A(&'a str),
        /// Discriminator 2; pushes a `TableB` into `body` with the
        /// given `count`.
        B(i32),
    }

    /// Build a `Msg` buffer for the schema produced by
    /// [`build_union_schema`]. Slot 4 holds the `u8` discriminator
    /// and slot 6 holds the union value (a sub-table offset).
    fn build_msg_buf(variant: UnionVariant<'_>) -> Vec<u8> {
        let mut fbb = FlatBufferBuilder::new();

        // Build the value sub-table first so its offset is known.
        // `flatbuffers::WIPOffset` is generic over the table type;
        // erase to a `usize`-shaped offset before pushing into the
        // union slot to keep both arms type-compatible.
        let (disc, value_off) = match variant {
            UnionVariant::None => (0u8, None),
            UnionVariant::A(name) => {
                let name_off = fbb.create_string(name);
                let t = fbb.start_table();
                fbb.push_slot_always(4, name_off);
                let off = fbb.end_table(t);
                (1, Some(off))
            }
            UnionVariant::B(count) => {
                let t = fbb.start_table();
                fbb.push_slot::<i32>(4, count, 0);
                let off = fbb.end_table(t);
                (2, Some(off))
            }
        };

        let t = fbb.start_table();
        if disc != 0 {
            // Discriminator at slot 4 (default 0 = NONE).
            fbb.push_slot::<u8>(4, disc, 0);
        }
        if let Some(off) = value_off {
            // Union value pointer at slot 6.
            fbb.push_slot_always(6, off);
        }
        let msg = fbb.end_table(t);
        fbb.finish_minimal(msg);
        fbb.finished_data().to_vec()
    }

    /// Test helper: execute against the union schema and return the
    /// first leaf.
    fn run_union(query_str: &str, buf: &[u8], bfbs: &[u8]) -> Result<Option<String>, ExecuteError> {
        let schema = root_as_schema(bfbs).expect("test schema verifies");
        let query = parse(query_str).expect("test query parses");
        execute(buf, &schema, &query, &Bounds::default()).map(|v| v.into_iter().next().flatten())
    }

    /// Test helper: execute against the union schema and return the
    /// raw error (used by the unsupported-step / not-found tests).
    fn run_union_err(query_str: &str, buf: &[u8], bfbs: &[u8]) -> ExecuteError {
        let schema = root_as_schema(bfbs).expect("test schema verifies");
        let query = parse(query_str).expect("test query parses");
        execute(buf, &schema, &query, &Bounds::default()).unwrap_err()
    }

    #[test]
    fn union_descends_into_table_a_variant() {
        let bfbs = build_union_schema();
        let buf = build_msg_buf(UnionVariant::A("hello"));
        // Auto-dispatch through the discriminator: body resolves to
        // TableA, then `.name` reads the string scalar leaf.
        assert_eq!(
            run_union("Msg:body.name", &buf, &bfbs).unwrap(),
            Some("hello".to_string())
        );
    }

    #[test]
    fn union_descends_into_table_b_variant() {
        let bfbs = build_union_schema();
        let buf = build_msg_buf(UnionVariant::B(42));
        assert_eq!(
            run_union("Msg:body.count", &buf, &bfbs).unwrap(),
            Some("42".to_string())
        );
    }

    #[test]
    fn union_none_variant_yields_none() {
        let bfbs = build_union_schema();
        let buf = build_msg_buf(UnionVariant::None);
        // Discriminator 0 short-circuits to `vec![None]` regardless
        // of what's asked beneath the union; same shape as an
        // absent sub-table.
        assert_eq!(run_union("Msg:body.name", &buf, &bfbs).unwrap(), None);
        assert_eq!(run_union("Msg:body.count", &buf, &bfbs).unwrap(), None);
    }

    #[test]
    fn union_field_not_in_active_variant_errors() {
        let bfbs = build_union_schema();
        // Active variant is A (which has `name` only); ask for B's
        // `count`. Auto-dispatch lands in TableA, where `count`
        // doesn't exist → FieldNotFound.
        let buf = build_msg_buf(UnionVariant::A("hi"));
        let err = run_union_err("Msg:body.count", &buf, &bfbs);
        assert!(
            matches!(&err, ExecuteError::FieldNotFound { what, table }
                if what == "count" && table == "A"),
            "got {err:?}"
        );
    }

    #[test]
    fn union_at_leaf_errors() {
        let bfbs = build_union_schema();
        let buf = build_msg_buf(UnionVariant::A("x"));
        // `Msg:body` with no descent — the union value is not a
        // textual leaf. Hits the existing `read_leaf` rejection
        // with `type_name: "union"`. Adding the variant-specific
        // "descend with `.field`" hint would require duplicating
        // the dispatch here; not worth a slice on its own.
        let err = run_union_err("Msg:body", &buf, &bfbs);
        assert!(
            matches!(&err, ExecuteError::UnsupportedType { field, type_name }
                if field == "body" && *type_name == "union"),
            "got {err:?}"
        );
    }

    #[test]
    fn union_discriminator_field_returns_value() {
        let bfbs = build_union_schema();
        // `body_type` is a UType (u8) scalar; queryable directly.
        // Returns the discriminator number — callers can map it to
        // a name in SQL until the deferred `|type` syntax lands.
        assert_eq!(
            run_union("Msg:body_type", &build_msg_buf(UnionVariant::A("x")), &bfbs).unwrap(),
            Some("1".to_string())
        );
        assert_eq!(
            run_union("Msg:body_type", &build_msg_buf(UnionVariant::B(0)), &bfbs).unwrap(),
            Some("2".to_string())
        );
        // NONE: discriminator slot omitted → schema default = 0.
        assert_eq!(
            run_union("Msg:body_type", &build_msg_buf(UnionVariant::None), &bfbs).unwrap(),
            Some("0".to_string())
        );
    }

    #[test]
    fn union_with_index_step_errors() {
        let bfbs = build_union_schema();
        let buf = build_msg_buf(UnionVariant::A("x"));
        // `[0]` makes no sense on a union (it's not a vector).
        // Falls through walk_union → walk_table on TableA, which
        // rejects Step::Index at the `head` match.
        let err = run_union_err("Msg:body[0]", &buf, &bfbs);
        assert!(
            matches!(&err, ExecuteError::UnsupportedStep { what } if *what == "[index]"),
            "got {err:?}"
        );
    }

    #[test]
    fn union_with_keys_step_errors() {
        let bfbs = build_union_schema();
        let buf = build_msg_buf(UnionVariant::A("x"));
        // `|keys` on a union: same dispatch path as `[0]`.
        let err = run_union_err("Msg:body|keys", &buf, &bfbs);
        assert!(
            matches!(&err, ExecuteError::UnsupportedStep { what } if *what == "|keys"),
            "got {err:?}"
        );
    }

    // -- `|type` (Step::UnionType) tests --

    #[test]
    fn union_type_leaf_returns_variant_name_a() {
        let bfbs = build_union_schema();
        let buf = build_msg_buf(UnionVariant::A("hello"));
        // `body|type` reads the discriminator and yields the
        // EnumVal name (the symbolic variant name) — string leaf.
        assert_eq!(
            run_union("Msg:body|type", &buf, &bfbs).unwrap(),
            Some("A".to_string())
        );
    }

    #[test]
    fn union_type_leaf_returns_variant_name_b() {
        let bfbs = build_union_schema();
        let buf = build_msg_buf(UnionVariant::B(99));
        assert_eq!(
            run_union("Msg:body|type", &buf, &bfbs).unwrap(),
            Some("B".to_string())
        );
    }

    #[test]
    fn union_type_leaf_for_none_returns_none_name() {
        let bfbs = build_union_schema();
        let buf = build_msg_buf(UnionVariant::None);
        // Discriminator absent → 0 → NONE EnumVal. Returns its
        // *name* (not SQL NULL) so the row stays filterable in SQL.
        // Symmetric with `body_type` returning "0" for absent.
        assert_eq!(
            run_union("Msg:body|type", &buf, &bfbs).unwrap(),
            Some("NONE".to_string())
        );
    }

    #[test]
    fn union_type_on_non_union_field_errors() {
        let bfbs = build_union_schema();
        let buf = build_msg_buf(UnionVariant::A("x"));
        // `body_type` is a UType scalar, not a union. `|type` is
        // only meaningful on `BaseType::Union` fields.
        let err = run_union_err("Msg:body_type|type", &buf, &bfbs);
        assert!(
            matches!(&err, ExecuteError::UnsupportedStep { what }
                if what.starts_with("|type") && what.contains("only valid on union")),
            "got {err:?}"
        );
    }

    #[test]
    fn union_type_with_descent_after_errors() {
        let bfbs = build_union_schema();
        let buf = build_msg_buf(UnionVariant::A("x"));
        // `|type` is a terminal leaf — descending past it is a
        // type-shape error. Parser allows the syntactic shape;
        // executor rejects it (mirrors the `|keys.foo` policy).
        let err = run_union_err("Msg:body|type.x", &buf, &bfbs);
        assert!(
            matches!(&err, ExecuteError::UnsupportedStep { what }
                if what.starts_with("|type") && what.contains("terminal")),
            "got {err:?}"
        );
    }

    #[test]
    fn union_type_at_root_errors() {
        // `Msg:|type` would mean "type of the root" — but the root
        // isn't a union. The parser produces a Step::UnionType as
        // the first step; walk_table's head match rejects it.
        let bfbs = build_union_schema();
        let buf = build_msg_buf(UnionVariant::A("x"));
        // The parser rejects an empty identifier before `|`; we
        // simulate the executor-side path by constructing the AST
        // directly. (`Msg:|type` parses as `EmptyComponent` /
        // `ExpectedIdentifier`, never reaching the executor.)
        let schema = root_as_schema(&bfbs).expect("test schema verifies");
        let query = Query {
            schema: None,
            root: "Msg".to_string(),
            steps: vec![Step::UnionType],
        };
        let err = execute(&buf, &schema, &query, &Bounds::default()).unwrap_err();
        assert!(
            matches!(&err, ExecuteError::UnsupportedStep { what } if *what == "|type"),
            "got {err:?}"
        );
    }
}
