use super::common::TestVec3;
use super::*;

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
/// padding; nested struct elements use [`TestVec3`](super::common::TestVec3)).
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
