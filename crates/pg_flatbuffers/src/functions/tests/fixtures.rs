//! Shared fixtures for `functions::tests` submodules: schema/buffer
//! builders, helper structs, and the `register` SPI helper. All items
//! are `pub(super)` so each per-area submodule can `use super::fixtures::*`.

use pgrx::prelude::*;

/// Build a trivial schema with one table `T { n: int = 0; }` and
/// `T` as the root. Returns the `.bfbs` bytes.
pub(super) fn build_t_schema_bfbs() -> Vec<u8> {
    use flatbuffers::FlatBufferBuilder;
    use flatbuffers_reflection::reflection::{
        BaseType, Enum, Field, FieldArgs, Object, ObjectArgs, Schema, SchemaArgs, Type, TypeArgs,
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
pub(super) fn build_t_buf(value: i32) -> Vec<u8> {
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
pub(super) fn build_b_schema_bfbs() -> Vec<u8> {
    use flatbuffers::FlatBufferBuilder;
    use flatbuffers_reflection::reflection::{
        BaseType, Enum, Field, FieldArgs, Object, ObjectArgs, Schema, SchemaArgs, Type, TypeArgs,
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
pub(super) fn build_b_buf(tags: Option<&[&str]>) -> Vec<u8> {
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
pub(super) fn build_catalog_schema_bfbs() -> Vec<u8> {
    use flatbuffers::FlatBufferBuilder;
    use flatbuffers_reflection::reflection::{
        BaseType, Enum, Field, FieldArgs, Object, ObjectArgs, Schema, SchemaArgs, Type, TypeArgs,
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
pub(super) fn build_catalog_buf(names: &[&str]) -> Vec<u8> {
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
pub(super) fn build_point_schema_bfbs() -> Vec<u8> {
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
pub(super) struct TestVec3 {
    pub(super) x: f32,
    pub(super) y: f32,
    pub(super) z: f32,
}

// SAFETY: `repr(C, packed)` makes the in-memory bytes match the
// on-wire little-endian layout on every supported host.
impl flatbuffers::Push for TestVec3 {
    type Output = TestVec3;
    unsafe fn push(&self, dst: &mut [u8], _written_len: usize) {
        // SAFETY: see the impl-level comment above.
        let src = unsafe {
            std::slice::from_raw_parts(
                self as *const Self as *const u8,
                std::mem::size_of::<Self>(),
            )
        };
        dst[..src.len()].copy_from_slice(src);
    }
}

/// Build a `Point` buffer with the supplied name and pos.
pub(super) fn build_point_buf(name: &str, pos: TestVec3) -> Vec<u8> {
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
pub(super) fn build_bag_schema_bfbs() -> Vec<u8> {
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
pub(super) fn build_bag_buf(points: &[TestVec3]) -> Vec<u8> {
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
pub(super) fn build_holder_schema_bfbs() -> Vec<u8> {
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
pub(super) struct TestBundle {
    pub(super) xs: [f32; 3],
    pub(super) pts: [TestVec3; 2],
}

// SAFETY: see [`TestVec3`].
impl flatbuffers::Push for TestBundle {
    type Output = TestBundle;
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

/// Build a `Holder` buffer containing the supplied `Bundle`.
pub(super) fn build_holder_buf(b: TestBundle) -> Vec<u8> {
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
pub(super) fn build_msg_schema_bfbs() -> Vec<u8> {
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
pub(super) fn build_msg_buf_a(name: &str) -> Vec<u8> {
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
pub(super) fn register(name: &str, root_table: &str, bfbs: Vec<u8>) {
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
