use super::common::TestVec3;
use super::*;

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
