//! Reflection-driven query executor (see `docs/design.md` §7.2).
//!
//! Consumes a parsed [`Query`] plus a registered [`Schema`] and a
//! caller-supplied buffer, and produces the leaf value as a `String`
//! (or `None` for SQL-NULL semantics).
//!
//! This module is the smallest viable executor: a starting point that
//! the next slices grow into the full design. It supports paths that
//! walk through nested tables to a scalar/string leaf:
//!
//! - `Step::Field` (by name and by `#id`).
//! - Descent into sub-tables (`BaseType::Obj` where the referenced
//!   `Object` is **not** a struct).
//! - Stringification of scalar (int/uint/float/bool) and string leaves
//!   via [`flatbuffers_reflection::get_any_field_string`].
//!
//! Deliberately deferred to dedicated micro-slices, each returning a
//! clear [`ExecuteError::Unsupported*`] variant today:
//!
//! - `Step::Index`, `Step::All` — vector access (§7.2 step 2/3).
//! - `Step::MapKey`, `Step::MapKeys` — `(key)`-vector lookups
//!   (§7.2 step 4/5).
//! - Struct descent — needs a separate `walk_struct` cursor since
//!   structs are inline fixed-size and use different accessors than
//!   tables.
//! - Union dispatch — needs the discriminator-slot pairing logic from
//!   §4.3 ("Union types").
//! - The `pg_flatbuffers.proto3_defaults` GUC (§10) — today scalar
//!   "absent" returns the schema default (postgres-protobuf compat).
//!
//! Pure Rust; no `pgrx` dependency. The Postgres SQL wrappers live in
//! `functions.rs` (next slice).

use super::ast::{FieldRef, Query, Step};
use crate::verify::VerifyError;
use crate::verify::{verify, Bounds};
use flatbuffers::Table;
use flatbuffers_reflection::reflection::{BaseType, Field, Object, Schema};
use flatbuffers_reflection::{
    get_any_field_string, get_any_root, get_field_table, FlatbufferError,
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

/// Run `query` against `buf`. Returns the leaf value as `Some(text)`,
/// or `None` for SQL-NULL semantics (field absent in the buffer or
/// any intermediate sub-table absent).
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
) -> Result<Option<String>, ExecuteError> {
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
/// `object`). Returns the leaf string, or `None` for SQL-NULL.
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
) -> Result<Option<String>, ExecuteError> {
    let (head, tail) = steps
        .split_first()
        .expect("parser guarantees at least one step");

    let field = match head {
        Step::Field(field_ref) => find_field(object, field_ref)?,
        Step::Index(_) => return Err(ExecuteError::UnsupportedStep { what: "[index]" }),
        Step::All => return Err(ExecuteError::UnsupportedStep { what: "[*]" }),
        Step::MapKey(_) => return Err(ExecuteError::UnsupportedStep { what: "[map-key]" }),
        Step::MapKeys => return Err(ExecuteError::UnsupportedStep { what: "|keys" }),
    };

    let field_name = field.name();
    let base_type = field.type_().base_type();

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

    if !is_present && is_nullable_type {
        return Ok(None);
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
            return Ok(Some(scalar_default_string(&field, base_type)));
        }
        return read_leaf(table, &field, schema, base_type);
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
                return Err(ExecuteError::UnsupportedType {
                    field: field_name.to_string(),
                    type_name: "struct (inline fixed-size record)",
                });
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
                None => Ok(None),
            }
        }
        other => Err(ExecuteError::UnsupportedType {
            field: field_name.to_string(),
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
        | BaseType::UType
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
        root_as_schema, Enum, Field as RField, FieldArgs, Object as RObject, ObjectArgs,
        Schema as RSchema, SchemaArgs, Type, TypeArgs,
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

    fn run(query_str: &str, buf: &[u8], bfbs: &[u8]) -> Result<Option<String>, ExecuteError> {
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
}
