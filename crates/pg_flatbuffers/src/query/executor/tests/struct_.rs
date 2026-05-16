use super::common::TestVec3;
use super::*;

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

/// `Outer { lo:Inner, hi:Inner }` mirror — same `Push`
/// strategy as [`TestVec3`](super::common::TestVec3).
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
        // SAFETY: see [`TestVec3`].
        let src = unsafe {
            std::slice::from_raw_parts(
                self as *const Self as *const u8,
                std::mem::size_of::<Self>(),
            )
        };
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
