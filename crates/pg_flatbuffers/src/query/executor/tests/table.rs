use super::*;

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
