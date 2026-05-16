// -- Vector64 / unsupported-schema-feature rejection (design §4.3) --

/// Build a `V64 { items: [int64-offset] }` schema rooted at `V64`.
/// Vector64 is the 64-bit-offset vector type; v0.1 doesn't
/// support it (the executor's 32-bit accessors would silently
/// truncate addresses), so `verify()` rejects the whole schema
/// up-front regardless of which field a query touches.
fn build_v64_schema_bfbs() -> Vec<u8> {
    use flatbuffers::FlatBufferBuilder;
    use flatbuffers_reflection::reflection::{
        BaseType, Enum, Field, FieldArgs, Object, ObjectArgs, Schema, SchemaArgs, Type, TypeArgs,
    };

    let mut fbb = FlatBufferBuilder::new();
    let items_t = Type::create(
        &mut fbb,
        &TypeArgs {
            base_type: BaseType::Vector64,
            element: BaseType::Int,
            ..Default::default()
        },
    );
    let items_name = fbb.create_string("items");
    let items_field = Field::create(
        &mut fbb,
        &FieldArgs {
            name: Some(items_name),
            type_: Some(items_t),
            id: 0,
            offset: 4,
            ..Default::default()
        },
    );
    let fields = fbb.create_vector(&[items_field]);
    let v_name = fbb.create_string("V64");
    let v_obj = Object::create(
        &mut fbb,
        &ObjectArgs {
            name: Some(v_name),
            fields: Some(fields),
            ..Default::default()
        },
    );
    let objects = fbb.create_vector(&[v_obj]);
    let enums = fbb.create_vector::<flatbuffers::ForwardsUOffset<Enum>>(&[]);
    let schema = Schema::create(
        &mut fbb,
        &SchemaArgs {
            objects: Some(objects),
            enums: Some(enums),
            root_table: Some(v_obj),
            ..Default::default()
        },
    );
    fbb.finish(schema, None);
    fbb.finished_data().to_vec()
}

/// `flatbuffers_query` against a Vector64 schema ERRORs with a
/// clear message naming the feature, the table, and the field.
/// The buffer bytes are irrelevant: schema-feature scanning fires
/// before any buffer inspection.
#[pg_test(
    error = "flatbuffers_query: buffer rejected by verifier: schema uses unsupported FlatBuffers feature Vector64: table \"V64\" field \"items\" (v0.1 does not support this)"
)]
fn pg_query_vector64_schema_errors() {
    register("default", "V64", build_v64_schema_bfbs());
    Spi::get_one::<String>("SELECT flatbuffers_query('V64:items', '\\x00010203'::bytea)")
        .expect("SPI failure");
}

/// `strict = off` MUST NOT silence a Vector64 schema rejection.
/// A schema-feature mismatch is a permanent config bug, not a
/// buffer-content failure; silencing would manifest as every
/// row returning NULL with no operator-visible signal.
#[pg_test(
    error = "flatbuffers_query: buffer rejected by verifier: schema uses unsupported FlatBuffers feature Vector64: table \"V64\" field \"items\" (v0.1 does not support this)"
)]
fn pg_query_vector64_schema_errors_even_when_strict_off() {
    register("default", "V64", build_v64_schema_bfbs());
    Spi::run("SET pg_flatbuffers.strict = off").expect("SET");
    Spi::get_one::<String>("SELECT flatbuffers_query('V64:items', '\\x00010203'::bytea)")
        .expect("SPI failure");
}

/// Same policy for the array entry point.
#[pg_test(
    error = "flatbuffers_query_array: buffer rejected by verifier: schema uses unsupported FlatBuffers feature Vector64: table \"V64\" field \"items\" (v0.1 does not support this)"
)]
fn pg_query_array_vector64_schema_errors_even_when_strict_off() {
    register("default", "V64", build_v64_schema_bfbs());
    Spi::run("SET pg_flatbuffers.strict = off").expect("SET");
    Spi::get_one::<Vec<Option<String>>>(
        "SELECT flatbuffers_query_array('V64:items', '\\x00010203'::bytea)",
    )
    .expect("SPI failure");
}

/// `flatbuffers_verify` ERRORs on a Vector64 schema even though
/// its normal contract is to swallow verifier failures into
/// `false`. The escape hatch keeps a broken schema from
/// silently passing a `CHECK` constraint that would then
/// silently reject every row.
#[pg_test(
    error = "flatbuffers_verify: schema uses unsupported FlatBuffers feature Vector64: table \"V64\" field \"items\" (v0.1 does not support this)"
)]
fn pg_verify_vector64_schema_errors_instead_of_false() {
    register("default", "V64", build_v64_schema_bfbs());
    Spi::get_one::<bool>("SELECT flatbuffers_verify('V64', '\\x00010203'::bytea)")
        .expect("SPI failure");
}

/// `flatbuffers_verify` retains its boolean contract for the
/// pre-schema short-circuit: an empty buffer still returns
/// `false` (never reaches schema scanning) even when the
/// registered schema would otherwise be rejected.
#[pg_test]
fn pg_verify_vector64_schema_empty_buf_still_false() {
    register("default", "V64", build_v64_schema_bfbs());
    let v = Spi::get_one::<bool>("SELECT flatbuffers_verify('V64', ''::bytea)")
        .expect("SPI failure")
        .expect("NULL");
    assert!(!v);
}
