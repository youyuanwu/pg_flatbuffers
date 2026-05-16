//! Pure-Rust unit tests for the executor (no `pgrx`; pgrx-backed SQL
//! smoke tests live in `crate::functions::tests`).
//!
//! Helpers (schema/buffer builders, `run_*` wrappers) are grouped by
//! the section they exercise — the layout mirrors the production-side
//! submodule split so a test fixture stays adjacent to the code it
//! exercises.

use super::map_key::{compare_actual_to_compiled, CompiledKey};
use super::*;
use crate::query::ast::{MapKey, Query, Step};
use crate::query::parse;
use crate::verify::Bounds;
use flatbuffers::FlatBufferBuilder;
use flatbuffers_reflection::reflection::{
    root_as_schema, BaseType, Enum, EnumArgs, EnumVal, EnumValArgs, Field as RField, FieldArgs,
    Object as RObject, ObjectArgs, Schema as RSchema, SchemaArgs, Type, TypeArgs,
};
use std::cmp::Ordering;

// -- comparator unit tests (pure; no fixture) --

#[test]
fn compare_text_keys_lexicographically() {
    let key = CompiledKey::Text("banana");
    assert_eq!(compare_actual_to_compiled("apple", &key), Ordering::Less);
    assert_eq!(compare_actual_to_compiled("banana", &key), Ordering::Equal);
    assert_eq!(
        compare_actual_to_compiled("cherry", &key),
        Ordering::Greater
    );
}

#[test]
fn compare_int_keys_numerically() {
    // Lexicographic comparison would put "10" < "2"; numeric
    // comparison puts 2 < 10. Pin the numeric semantics.
    let key = CompiledKey::Int(10);
    assert_eq!(compare_actual_to_compiled("2", &key), Ordering::Less);
    assert_eq!(compare_actual_to_compiled("10", &key), Ordering::Equal);
    assert_eq!(compare_actual_to_compiled("11", &key), Ordering::Greater);
}

#[test]
fn compare_int_keys_signed() {
    // Negative actuals (signed int fields) parse and compare
    // correctly without underflowing through "-" lexicography.
    let key = CompiledKey::Int(0);
    assert_eq!(compare_actual_to_compiled("-5", &key), Ordering::Less);
    assert_eq!(compare_actual_to_compiled("5", &key), Ordering::Greater);
}

#[test]
fn compare_int_keys_unparseable_actual_is_less() {
    // Documented in `compare_actual_to_compiled`: a
    // non-i64-parseable actual (e.g., a ULong above i64::MAX)
    // sorts as `Less` so the bisect remains deterministic.
    let key = CompiledKey::Int(42);
    assert_eq!(
        compare_actual_to_compiled("99999999999999999999", &key),
        Ordering::Less,
    );
}

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
fn run_all(query_str: &str, buf: &[u8], bfbs: &[u8]) -> Result<Vec<Option<String>>, ExecuteError> {
    let schema = root_as_schema(bfbs).expect("test schema verifies");
    let query = parse(query_str).expect("test query parses");
    execute_with_options(
        buf,
        &schema,
        &query,
        &Bounds::default(),
        &ExecuteOptions::default(),
    )
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
    // Per design §4.3 with `fill_scalar_defaults = on` (today's
    // default), the schema default is returned, NOT NULL.
    assert_eq!(v.as_deref(), Some("0"));
}

/// `fill_scalar_defaults = off` flips the absent-scalar branch
/// from "synthesise the schema default" to "surface SQL NULL"
/// (design §4.3 / §10). Same fixture as
/// `scalar_absent_returns_default` so the difference is purely
/// the [`ExecuteOptions`] flag.
#[test]
fn scalar_absent_returns_null_when_fill_scalar_defaults_off() {
    let bfbs = build_schema();
    let buf = build_order(None, 0, None);
    let schema = root_as_schema(&bfbs).expect("test schema verifies");
    let query = parse("Order:id").expect("test query parses");
    let options = ExecuteOptions {
        fill_scalar_defaults: false,
        ..ExecuteOptions::default()
    };
    let leaves = execute_with_options(&buf, &schema, &query, &Bounds::default(), &options)
        .expect("execute succeeds");
    assert_eq!(leaves, vec![None]);
}

/// `fill_scalar_defaults = off` must NOT swallow *present*
/// scalars whose value happens to be zero. Pins the
/// presence-vs-default distinction.
#[test]
fn scalar_present_zero_is_not_treated_as_absent_when_fill_off() {
    // Force the writer to actually emit the slot even though
    // `id == 0` by passing a non-default default — `flatc`
    // would normally elide a slot that matches the schema
    // default, but here we go through `push_slot_always`
    // indirectly by setting the default sentinel to a value
    // that won't match.
    let bfbs = build_schema();
    use flatbuffers::FlatBufferBuilder;
    let mut fbb = FlatBufferBuilder::new();
    let t = fbb.start_table();
    // slot 6 = id; write 0 with default = 1 so the slot is
    // actually emitted in the vtable.
    fbb.push_slot::<i32>(6, 0, 1);
    let order = fbb.end_table(t);
    fbb.finish_minimal(order);
    let buf = fbb.finished_data().to_vec();

    let schema = root_as_schema(&bfbs).expect("test schema verifies");
    let query = parse("Order:id").expect("test query parses");
    let options = ExecuteOptions {
        fill_scalar_defaults: false,
        ..ExecuteOptions::default()
    };
    let leaves = execute_with_options(&buf, &schema, &query, &Bounds::default(), &options)
        .expect("execute succeeds");
    // Present-with-value-0 → "0", not None. (The schema's own
    // default is still 0; the `1` above is only a builder-side
    // device to force emission.)
    assert_eq!(leaves, vec![Some("0".to_owned())]);
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
    // schema has no vector — execute_with_options() should still reject
    // with UnsupportedStep before touching the schema.
    let q = Query {
        schema: None,
        root: "Order".to_owned(),
        steps: vec![Step::All],
    };
    let schema = root_as_schema(&bfbs).unwrap();
    let err = execute_with_options(
        &buf,
        &schema,
        &q,
        &Bounds::default(),
        &ExecuteOptions::default(),
    )
    .unwrap_err();
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
    let err = execute_with_options(
        &buf,
        &schema,
        &q,
        &Bounds::default(),
        &ExecuteOptions::default(),
    )
    .unwrap_err();
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
fn run_struct(query_str: &str, buf: &[u8], bfbs: &[u8]) -> Result<Option<String>, ExecuteError> {
    let schema = root_as_schema(bfbs).expect("test schema verifies");
    let query = parse(query_str).expect("test query parses");
    execute_with_options(
        buf,
        &schema,
        &query,
        &Bounds::default(),
        &ExecuteOptions::default(),
    )
    .map(|v| v.into_iter().next().flatten())
}

/// Test helper: execute against the `Point` schema and return
/// the raw `Vec<Option<String>>` (used by the unsupported-step
/// tests to confirm the error variant).
fn run_struct_err(query_str: &str, buf: &[u8], bfbs: &[u8]) -> ExecuteError {
    let schema = root_as_schema(bfbs).expect("test schema verifies");
    let query = parse(query_str).expect("test query parses");
    execute_with_options(
        buf,
        &schema,
        &query,
        &Bounds::default(),
        &ExecuteOptions::default(),
    )
    .unwrap_err()
}

/// Test helper: execute against the `Point` schema and unwrap to
/// the first leaf. The fanout (`Step::All`) tests use
/// [`run_vec_all`] instead to inspect the whole vec.
fn run_vec(query_str: &str, buf: &[u8], bfbs: &[u8]) -> Result<Option<String>, ExecuteError> {
    run_vec_all(query_str, buf, bfbs).map(|v| v.into_iter().next().flatten())
}

/// Like [`run_vec`] but takes a custom [`ExecuteOptions`] so
/// tests can pin the `key_lookup_strict` mode (binary search
/// vs. linear scan) explicitly.
fn run_vec_with(
    query_str: &str,
    buf: &[u8],
    bfbs: &[u8],
    options: &ExecuteOptions,
) -> Result<Option<String>, ExecuteError> {
    let schema = root_as_schema(bfbs).expect("test schema verifies");
    let query = parse(query_str).expect("test query parses");
    execute_with_options(buf, &schema, &query, &Bounds::default(), options)
        .map(|v| v.into_iter().next().flatten())
}

fn run_vec_all(
    query_str: &str,
    buf: &[u8],
    bfbs: &[u8],
) -> Result<Vec<Option<String>>, ExecuteError> {
    let schema = root_as_schema(bfbs).expect("test schema verifies");
    let query = parse(query_str).expect("test query parses");
    execute_with_options(
        buf,
        &schema,
        &query,
        &Bounds::default(),
        &ExecuteOptions::default(),
    )
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
    // Two entries with sku = "dup": the linear-scan path
    // (`key_lookup_strict = off`) returns the first in wire
    // order. Binary search under the default
    // `key_lookup_strict = on` mode can return *either* match
    // depending on the bisect path, so we pin the wire-order
    // tiebreaker against the off-path explicitly.
    let buf = build_bag(Some(&["dup", "x", "dup"]), None, None, None);
    let v = run_vec_with(
        "Bag:items[dup].sku",
        &buf,
        &bfbs,
        &ExecuteOptions {
            key_lookup_strict: false,
            ..ExecuteOptions::default()
        },
    )
    .expect("ok");
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

// -- Step::MapKey: binary search vs. linear scan --

/// Binary search (the `key_lookup_strict = on` default) finds an
/// interior key on a properly sorted vector. With 5 elements
/// the bisect visits ~3 of them, vs. ~3 on average for linear
/// scan; the perf-win story shows up at larger n, but this
/// fixture is enough to pin correctness.
#[test]
fn vector_map_key_binary_search_interior_hit() {
    let bfbs = build_vec_schema();
    let buf = build_bag(Some(&["a", "b", "c", "d", "e"]), None, None, None);
    let v = run_vec("Bag:items[c].sku", &buf, &bfbs).expect("ok");
    assert_eq!(v.as_deref(), Some("c"));
}

/// Binary search must hit both endpoints (lo-mid and hi-mid
/// boundaries). The interior-hit test alone doesn't exercise
/// either edge.
#[test]
fn vector_map_key_binary_search_first_and_last_hit() {
    let bfbs = build_vec_schema();
    let buf = build_bag(Some(&["a", "b", "c", "d", "e"]), None, None, None);
    let first = run_vec("Bag:items[a].sku", &buf, &bfbs).expect("ok");
    assert_eq!(first.as_deref(), Some("a"));
    let last = run_vec("Bag:items[e].sku", &buf, &bfbs).expect("ok");
    assert_eq!(last.as_deref(), Some("e"));
}

/// Binary search reports miss on a key sandwiched between two
/// present keys (the classic "split goes both ways" case where
/// a naive bisect would loop forever if it didn't tighten the
/// half-open range correctly).
#[test]
fn vector_map_key_binary_search_interior_miss() {
    let bfbs = build_vec_schema();
    // "bb" sorts between "b" and "c"; not present.
    let buf = build_bag(Some(&["a", "b", "c", "d"]), None, None, None);
    let v = run_vec("Bag:items[bb].sku", &buf, &bfbs).expect("ok");
    assert!(v.is_none(), "got {v:?}");
}

/// Binary search misses below the first element (lo advances
/// past hi at lo=0,hi=0 immediately).
#[test]
fn vector_map_key_binary_search_below_first_miss() {
    let bfbs = build_vec_schema();
    let buf = build_bag(Some(&["m", "n", "o"]), None, None, None);
    let v = run_vec("Bag:items[a].sku", &buf, &bfbs).expect("ok");
    assert!(v.is_none(), "got {v:?}");
}

/// Binary search misses above the last element.
#[test]
fn vector_map_key_binary_search_above_last_miss() {
    let bfbs = build_vec_schema();
    let buf = build_bag(Some(&["a", "b", "c"]), None, None, None);
    let v = run_vec("Bag:items[z].sku", &buf, &bfbs).expect("ok");
    assert!(v.is_none(), "got {v:?}");
}

/// `key_lookup_strict = on` against an unsorted vector silently
/// misses keys the bisect can't reach. This is the documented
/// worst case from the GUC's doc text — fail-fast for the
/// operator is escalate to `key_lookup_strict = off`. Pin it so
/// a regression that accidentally fell back to linear scan
/// under strict-on would surface.
#[test]
fn vector_map_key_binary_search_unsorted_silent_miss() {
    let bfbs = build_vec_schema();
    // "a" sorts before "b" / "c", but the vector is in reverse
    // order. Bisect on len=3 visits mid=1 first; sees "b" vs.
    // target "a" → Less → moves to hi=1; then mid=0, sees "c"
    // vs. "a" → Greater → hi=0; loop ends; no match.
    let buf = build_bag(Some(&["c", "b", "a"]), None, None, None);
    let v = run_vec("Bag:items[a].sku", &buf, &bfbs).expect("ok");
    assert!(
        v.is_none(),
        "binary search on unsorted vec must silently miss; got {v:?}",
    );
}

/// Same fixture as the strict-on silent-miss test, but with
/// `key_lookup_strict = off`: linear scan finds the element
/// regardless of order. Pins the escape-hatch contract.
#[test]
fn vector_map_key_linear_scan_finds_unsorted_match() {
    let bfbs = build_vec_schema();
    let buf = build_bag(Some(&["c", "b", "a"]), None, None, None);
    let v = run_vec_with(
        "Bag:items[a].sku",
        &buf,
        &bfbs,
        &ExecuteOptions {
            key_lookup_strict: false,
            ..ExecuteOptions::default()
        },
    )
    .expect("ok");
    assert_eq!(v.as_deref(), Some("a"));
}

/// Both modes agree on miss when the key genuinely isn't
/// present, regardless of sortedness. Belt-and-suspenders for
/// the "off mode doesn't silently invent matches" property.
#[test]
fn vector_map_key_both_modes_agree_on_absent_key() {
    let bfbs = build_vec_schema();
    let buf = build_bag(Some(&["a", "b", "c"]), None, None, None);
    let strict = run_vec("Bag:items[zzz].sku", &buf, &bfbs).expect("ok");
    let loose = run_vec_with(
        "Bag:items[zzz].sku",
        &buf,
        &bfbs,
        &ExecuteOptions {
            key_lookup_strict: false,
            ..ExecuteOptions::default()
        },
    )
    .expect("ok");
    assert!(
        strict.is_none() && loose.is_none(),
        "{strict:?} vs {loose:?}"
    );
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
    execute_with_options(
        buf,
        &schema,
        &query,
        &Bounds::default(),
        &ExecuteOptions::default(),
    )
    .map(|v| v.into_iter().next().flatten())
}

/// Test helper: execute against the union schema and return the
/// raw error (used by the unsupported-step / not-found tests).
fn run_union_err(query_str: &str, buf: &[u8], bfbs: &[u8]) -> ExecuteError {
    let schema = root_as_schema(bfbs).expect("test schema verifies");
    let query = parse(query_str).expect("test query parses");
    execute_with_options(
        buf,
        &schema,
        &query,
        &Bounds::default(),
        &ExecuteOptions::default(),
    )
    .unwrap_err()
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
    let err = execute_with_options(
        &buf,
        &schema,
        &query,
        &Bounds::default(),
        &ExecuteOptions::default(),
    )
    .unwrap_err();
    assert!(
        matches!(&err, ExecuteError::UnsupportedStep { what } if *what == "|type"),
        "got {err:?}"
    );
}

// -- vector of struct (design §7.2 / §4.3) --

/// Build a schema with a vector of inline structs:
///
/// ```text
/// struct Vec3 { x:float; y:float; z:float; }   // bytesize 12, align 4
/// table Bag {
///   points: [Vec3];   // id 0, vtable offset 4
/// }
/// root_type Bag;
/// ```
///
/// Object vector is sorted alphabetically: `Bag` (0), `Vec3` (1).
fn build_bag_schema() -> Vec<u8> {
    let mut fbb = FlatBufferBuilder::new();

    let float_t = Type::create(
        &mut fbb,
        &TypeArgs {
            base_type: BaseType::Float,
            ..Default::default()
        },
    );

    // -- struct Vec3 { x:float @0; y:float @4; z:float @8; } --
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

/// Build a `Bag` buffer matching `build_bag_schema()`. `None`
/// omits the `points` field entirely (vtable slot 0); `Some(&[])`
/// writes an empty vector.
fn build_bag_buf(points: Option<&[TestVec3]>) -> Vec<u8> {
    let mut fbb = FlatBufferBuilder::new();
    let points_off = points.map(|p| fbb.create_vector(p));

    let t = fbb.start_table();
    if let Some(off) = points_off {
        fbb.push_slot_always(4, off); // slot 4 = points
    }
    let bag = fbb.end_table(t);
    fbb.finish_minimal(bag);
    fbb.finished_data().to_vec()
}

/// Test helper: execute against the `Bag` schema and return the
/// first leaf.
fn run_bag(query_str: &str, buf: &[u8], bfbs: &[u8]) -> Option<String> {
    let schema = root_as_schema(bfbs).expect("test schema verifies");
    let query = parse(query_str).expect("test query parses");
    execute_with_options(
        buf,
        &schema,
        &query,
        &Bounds::default(),
        &ExecuteOptions::default(),
    )
    .expect("test execute succeeds")
    .into_iter()
    .next()
    .flatten()
}

/// Test helper: execute against the `Bag` schema and return the
/// raw `Vec<Option<String>>` (used by `[*]` fanout tests).
fn run_bag_all(query_str: &str, buf: &[u8], bfbs: &[u8]) -> Vec<Option<String>> {
    let schema = root_as_schema(bfbs).expect("test schema verifies");
    let query = parse(query_str).expect("test query parses");
    execute_with_options(
        buf,
        &schema,
        &query,
        &Bounds::default(),
        &ExecuteOptions::default(),
    )
    .expect("test execute succeeds")
}

/// Test helper: assert that executing `query_str` against the
/// `Bag` schema returns an error.
fn run_bag_err(query_str: &str, buf: &[u8], bfbs: &[u8]) -> ExecuteError {
    let schema = root_as_schema(bfbs).expect("test schema verifies");
    let query = parse(query_str).expect("test query parses");
    execute_with_options(
        buf,
        &schema,
        &query,
        &Bounds::default(),
        &ExecuteOptions::default(),
    )
    .unwrap_err()
}

fn three_points() -> [TestVec3; 3] {
    [
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
    ]
}

#[test]
fn vector_of_struct_index_then_field_scalar() {
    let bfbs = build_bag_schema();
    let pts = three_points();
    let buf = build_bag_buf(Some(&pts));
    // Element 1 is {4, 5, 6} — read its `y` (= 5).
    let v = run_bag("Bag:points[1].y", &buf, &bfbs);
    assert_eq!(v.as_deref(), Some("5"));
}

#[test]
fn vector_of_struct_index_zero_field_scalar() {
    let bfbs = build_bag_schema();
    let pts = three_points();
    let buf = build_bag_buf(Some(&pts));
    let v = run_bag("Bag:points[0].x", &buf, &bfbs);
    assert_eq!(v.as_deref(), Some("1"));
    let v = run_bag("Bag:points[2].z", &buf, &bfbs);
    assert_eq!(v.as_deref(), Some("9"));
}

#[test]
fn vector_of_struct_all_field_fanout() {
    let bfbs = build_bag_schema();
    let pts = three_points();
    let buf = build_bag_buf(Some(&pts));
    let v = run_bag_all("Bag:points[*].x", &buf, &bfbs);
    // One leaf per element, in wire-format order.
    let strs: Vec<Option<&str>> = v.iter().map(|s| s.as_deref()).collect();
    assert_eq!(strs, vec![Some("1"), Some("4"), Some("7")]);
}

#[test]
fn vector_of_struct_oob_index_returns_none() {
    let bfbs = build_bag_schema();
    let pts = three_points();
    let buf = build_bag_buf(Some(&pts));
    // Per design §4.3, OOB indices short-circuit to NULL.
    let v = run_bag_all("Bag:points[99].x", &buf, &bfbs);
    assert_eq!(v, vec![None]);
}

#[test]
fn vector_of_struct_index_on_absent_returns_none() {
    let bfbs = build_bag_schema();
    let buf = build_bag_buf(None);
    let v = run_bag_all("Bag:points[0].x", &buf, &bfbs);
    assert_eq!(v, vec![None]);
}

#[test]
fn vector_of_struct_all_on_absent_returns_empty() {
    let bfbs = build_bag_schema();
    let buf = build_bag_buf(None);
    // Absent vector + `[*]` fanout → empty result, not `[None]`.
    let v = run_bag_all("Bag:points[*].x", &buf, &bfbs);
    assert!(v.is_empty(), "expected empty vec, got {v:?}");
}

#[test]
fn vector_of_struct_at_leaf_index_errors() {
    let bfbs = build_bag_schema();
    let pts = three_points();
    let buf = build_bag_buf(Some(&pts));
    // No textual leaf form for a struct element in v0.1 — must
    // descend with `.field`.
    let err = run_bag_err("Bag:points[0]", &buf, &bfbs);
    assert!(
        matches!(
            &err,
            ExecuteError::UnsupportedType { type_name, .. }
                if type_name.contains("vector-of-struct element")
        ),
        "got {err:?}"
    );
}

#[test]
fn vector_of_struct_at_leaf_all_errors() {
    let bfbs = build_bag_schema();
    let pts = three_points();
    let buf = build_bag_buf(Some(&pts));
    let err = run_bag_err("Bag:points[*]", &buf, &bfbs);
    assert!(
        matches!(
            &err,
            ExecuteError::UnsupportedType { type_name, .. }
                if type_name.contains("vector-of-struct element")
        ),
        "got {err:?}"
    );
}

#[test]
fn vector_of_struct_with_map_key_errors() {
    let bfbs = build_bag_schema();
    let pts = three_points();
    let buf = build_bag_buf(Some(&pts));
    // Struct elements have no `(key)` annotation, so `[abc]`
    // (a textual map-key) is a schema-level error here.
    let err = run_bag_err("Bag:points[abc].x", &buf, &bfbs);
    assert!(
        matches!(
            &err,
            ExecuteError::UnsupportedType { type_name, .. }
                if type_name.contains("[key] not supported")
        ),
        "got {err:?}"
    );
}

#[test]
fn vector_of_struct_with_keys_errors() {
    let bfbs = build_bag_schema();
    let pts = three_points();
    let buf = build_bag_buf(Some(&pts));
    let err = run_bag_err("Bag:points|keys", &buf, &bfbs);
    assert!(
        matches!(
            &err,
            ExecuteError::UnsupportedType { type_name, .. }
                if type_name.contains("|keys not supported")
        ),
        "got {err:?}"
    );
}

// -- fixed-size arrays inside structs (BaseType::Array) --

/// Build a schema exercising both array element shapes:
///
/// ```text
/// struct Vec3   { x:float; y:float; z:float; }   // bytesize 12, align 4
/// struct Bundle {
///   xs:  [float:3];   // offset 0,  size 12 (scalar elements)
///   pts: [Vec3:2];    // offset 12, size 24 (struct elements)
/// }                                              // bytesize 36, align 4
/// table Holder {
///   b: Bundle;        // id 0, vtable offset 4 (inline struct)
/// }
/// root_type Holder;
/// ```
///
/// Object vector is sorted alphabetically:
/// `Bundle` (0), `Holder` (1), `Vec3` (2).
fn build_holder_schema() -> Vec<u8> {
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
    // Vec3 sorts as object index 2 (Bundle (0), Holder (1), Vec3 (2)).
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

/// Mirror of the `Bundle` struct above. `repr(C, packed)`
/// matches the FlatBuffers wire layout for structs (no compiler
/// padding; nested struct elements use [`TestVec3`]).
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
/// `b` is `Option<>` so absent-struct cases reuse the same
/// fixture; `None` omits the field entirely.
fn build_holder_buf(b: Option<TestBundle>) -> Vec<u8> {
    let mut fbb = FlatBufferBuilder::new();
    let t = fbb.start_table();
    if let Some(v) = b {
        fbb.push_slot_always(4, v); // slot 4 = b (inline struct)
    }
    let h = fbb.end_table(t);
    fbb.finish_minimal(h);
    fbb.finished_data().to_vec()
}

fn run_holder(query_str: &str, buf: &[u8], bfbs: &[u8]) -> Option<String> {
    let schema = root_as_schema(bfbs).expect("test schema verifies");
    let query = parse(query_str).expect("test query parses");
    execute_with_options(
        buf,
        &schema,
        &query,
        &Bounds::default(),
        &ExecuteOptions::default(),
    )
    .expect("test execute succeeds")
    .into_iter()
    .next()
    .flatten()
}

fn run_holder_all(query_str: &str, buf: &[u8], bfbs: &[u8]) -> Vec<Option<String>> {
    let schema = root_as_schema(bfbs).expect("test schema verifies");
    let query = parse(query_str).expect("test query parses");
    execute_with_options(
        buf,
        &schema,
        &query,
        &Bounds::default(),
        &ExecuteOptions::default(),
    )
    .expect("test execute succeeds")
}

fn run_holder_err(query_str: &str, buf: &[u8], bfbs: &[u8]) -> ExecuteError {
    let schema = root_as_schema(bfbs).expect("test schema verifies");
    let query = parse(query_str).expect("test query parses");
    execute_with_options(
        buf,
        &schema,
        &query,
        &Bounds::default(),
        &ExecuteOptions::default(),
    )
    .unwrap_err()
}

fn sample_bundle() -> TestBundle {
    TestBundle {
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
    }
}

#[test]
fn array_of_scalars_index_returns_element() {
    let bfbs = build_holder_schema();
    let buf = build_holder_buf(Some(sample_bundle()));
    let v = run_holder("Holder:b.xs[1]", &buf, &bfbs);
    assert_eq!(v.as_deref(), Some("2"));
    let v = run_holder("Holder:b.xs[0]", &buf, &bfbs);
    assert_eq!(v.as_deref(), Some("1"));
    let v = run_holder("Holder:b.xs[2]", &buf, &bfbs);
    assert_eq!(v.as_deref(), Some("3"));
}

#[test]
fn array_of_scalars_all_fans_out() {
    let bfbs = build_holder_schema();
    let buf = build_holder_buf(Some(sample_bundle()));
    let v = run_holder_all("Holder:b.xs[*]", &buf, &bfbs);
    let strs: Vec<Option<&str>> = v.iter().map(|s| s.as_deref()).collect();
    assert_eq!(strs, vec![Some("1"), Some("2"), Some("3")]);
}

#[test]
fn array_of_scalars_oob_index_returns_none() {
    let bfbs = build_holder_schema();
    let buf = build_holder_buf(Some(sample_bundle()));
    // Per design §4.3, OOB indices short-circuit to NULL.
    let v = run_holder_all("Holder:b.xs[99]", &buf, &bfbs);
    assert_eq!(v, vec![None]);
}

#[test]
fn array_of_scalars_at_leaf_errors() {
    let bfbs = build_holder_schema();
    let buf = build_holder_buf(Some(sample_bundle()));
    // No textual leaf form for an array — must descend.
    let err = run_holder_err("Holder:b.xs", &buf, &bfbs);
    assert!(
        matches!(
            &err,
            ExecuteError::UnsupportedType { type_name, .. }
                if type_name.contains("fixed-size array")
        ),
        "got {err:?}"
    );
}

#[test]
fn array_of_struct_index_then_field() {
    let bfbs = build_holder_schema();
    let buf = build_holder_buf(Some(sample_bundle()));
    // Element 0 is {10,20,30}; element 1 is {100,200,300}.
    let v = run_holder("Holder:b.pts[0].y", &buf, &bfbs);
    assert_eq!(v.as_deref(), Some("20"));
    let v = run_holder("Holder:b.pts[1].z", &buf, &bfbs);
    assert_eq!(v.as_deref(), Some("300"));
}

#[test]
fn array_of_struct_all_field_fanout() {
    let bfbs = build_holder_schema();
    let buf = build_holder_buf(Some(sample_bundle()));
    let v = run_holder_all("Holder:b.pts[*].x", &buf, &bfbs);
    let strs: Vec<Option<&str>> = v.iter().map(|s| s.as_deref()).collect();
    assert_eq!(strs, vec![Some("10"), Some("100")]);
}

#[test]
fn array_of_struct_oob_index_returns_none() {
    let bfbs = build_holder_schema();
    let buf = build_holder_buf(Some(sample_bundle()));
    let v = run_holder_all("Holder:b.pts[5].x", &buf, &bfbs);
    assert_eq!(v, vec![None]);
}

#[test]
fn array_of_struct_at_leaf_index_errors() {
    let bfbs = build_holder_schema();
    let buf = build_holder_buf(Some(sample_bundle()));
    // Struct elements have no textual leaf form — must descend
    // with `.field`.
    let err = run_holder_err("Holder:b.pts[0]", &buf, &bfbs);
    assert!(
        matches!(
            &err,
            ExecuteError::UnsupportedType { type_name, .. }
                if type_name.contains("fixed-size-array-of-struct element")
        ),
        "got {err:?}"
    );
}

#[test]
fn array_with_map_key_errors() {
    let bfbs = build_holder_schema();
    let buf = build_holder_buf(Some(sample_bundle()));
    // Arrays carry no `(key)` annotation, so `[abc]` is a
    // schema-level error.
    let err = run_holder_err("Holder:b.xs[abc]", &buf, &bfbs);
    assert!(
        matches!(
            &err,
            ExecuteError::UnsupportedType { type_name, .. }
                if type_name.contains("[map-key] on fixed-size array")
        ),
        "got {err:?}"
    );
}

#[test]
fn array_with_keys_errors() {
    let bfbs = build_holder_schema();
    let buf = build_holder_buf(Some(sample_bundle()));
    let err = run_holder_err("Holder:b.xs|keys", &buf, &bfbs);
    assert!(
        matches!(
            &err,
            ExecuteError::UnsupportedType { type_name, .. }
                if type_name.contains("|keys on fixed-size array")
        ),
        "got {err:?}"
    );
}

#[test]
fn array_absent_struct_returns_none() {
    let bfbs = build_holder_schema();
    // `b` is absent from the table → walk_table short-circuits
    // before walk_struct/walk_array gets a chance to descend.
    let buf = build_holder_buf(None);
    let v = run_holder_all("Holder:b.xs[1]", &buf, &bfbs);
    assert_eq!(v, vec![None]);
}
