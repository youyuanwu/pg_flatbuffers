// -- flatbuffers_query: basic SQL surface (NULL/empty/happy/garbage/strict-off, map_key, struct/union/vector_of_struct/array shape-dispatch smokes).
// -- flatbuffers_query --

/// `STRICT` short-circuits NULL inputs without touching the
/// function body. Verifies pgrx wired the attribute correctly.
#[pg_test]
fn pg_query_null_buf_returns_null() {
    let v = Spi::get_one::<String>("SELECT flatbuffers_query('T:n', NULL::bytea)")
        .expect("SPI failure");
    assert!(v.is_none(), "expected NULL, got {v:?}");
}

/// Empty `bytea` is the "absent payload" sentinel from §10 — must
/// be NULL, not an `ERROR` from the verifier (which would otherwise
/// reject zero-length input).
#[pg_test]
fn pg_query_empty_buf_returns_null() {
    let v =
        Spi::get_one::<String>("SELECT flatbuffers_query('T:n', ''::bytea)").expect("SPI failure");
    assert!(v.is_none(), "expected NULL, got {v:?}");
}

#[pg_test]
fn pg_query_happy_path_scalar() {
    register("default", "T", build_t_schema_bfbs());
    let buf = build_t_buf(42);
    let v = Spi::get_one_with_args::<String>("SELECT flatbuffers_query('T:n', $1)", &[buf.into()])
        .expect("SPI failure");
    assert_eq!(v.as_deref(), Some("42"));
}

/// Absent scalar (value == default == 0) round-trips through the
/// executor's `scalar_default_string` path and back to SQL as the
/// schema default (today's `fill_scalar_defaults = on` behaviour).
#[pg_test]
fn pg_query_absent_scalar_returns_default() {
    register("default", "T", build_t_schema_bfbs());
    let buf = build_t_buf(0); // elided slot
    let v = Spi::get_one_with_args::<String>("SELECT flatbuffers_query('T:n', $1)", &[buf.into()])
        .expect("SPI failure");
    assert_eq!(v.as_deref(), Some("0"));
}

/// `SET pg_flatbuffers.fill_scalar_defaults = off` flips the
/// absent-scalar branch from "schema default" to SQL NULL
/// (design §4.3 / §10). Same fixture as
/// `pg_query_absent_scalar_returns_default`; only the GUC
/// changes.
#[pg_test]
fn pg_query_absent_scalar_returns_null_when_fill_off() {
    register("default", "T", build_t_schema_bfbs());
    let buf = build_t_buf(0); // elided slot
    Spi::run("SET pg_flatbuffers.fill_scalar_defaults = off")
        .expect("SPI: SET fill_scalar_defaults");
    let v = Spi::get_one_with_args::<String>("SELECT flatbuffers_query('T:n', $1)", &[buf.into()])
        .expect("SPI failure");
    assert!(v.is_none(), "expected NULL under fill_off, got {v:?}");
}

/// `fill_scalar_defaults = off` must not affect *present*
/// scalars — including the `n = 42` value from
/// `pg_query_happy_path_scalar`. Pins that the GUC gates only
/// the absent branch.
#[pg_test]
fn pg_query_present_scalar_unaffected_by_fill_off() {
    register("default", "T", build_t_schema_bfbs());
    let buf = build_t_buf(42); // slot emitted (42 != default 0)
    Spi::run("SET pg_flatbuffers.fill_scalar_defaults = off")
        .expect("SPI: SET fill_scalar_defaults");
    let v = Spi::get_one_with_args::<String>("SELECT flatbuffers_query('T:n', $1)", &[buf.into()])
        .expect("SPI failure");
    assert_eq!(v.as_deref(), Some("42"));
}

/// Garbage bytes must reach the verifier and ERROR (default
/// `strict = on` semantics). The doubled "buffer rejected by
/// verifier" prefix comes from `ExecuteError::Verify`'s
/// Display layered on `VerifyError::Invalid`'s Display; the
/// `Range [50462976, 50462980)` substring is the verifier's
/// deterministic decoding of `\x00010203` as a root offset of
/// `0x03020100 = 50462976`. pgrx-tests does exact equality and
/// appends `\n\n` for the backtrace separator.
#[pg_test(
    error = "flatbuffers_query: buffer rejected by verifier: buffer rejected by FlatBuffers verifier: Range [50462976, 50462980) is out of bounds.\n\n"
)]
fn pg_query_garbage_buf_errors() {
    register("default", "T", build_t_schema_bfbs());
    Spi::get_one::<String>("SELECT flatbuffers_query('T:n', '\\x00010203'::bytea)")
        .expect("SPI failure");
}

/// `strict = off` turns a *structural* verifier failure into a
/// NULL result for the scalar entry point (design §10).
#[pg_test]
fn pg_query_strict_off_garbage_returns_null() {
    register("default", "T", build_t_schema_bfbs());
    Spi::run("SET pg_flatbuffers.strict = off").expect("SET");
    let v = Spi::get_one::<String>("SELECT flatbuffers_query('T:n', '\\x00010203'::bytea)")
        .expect("SPI failure");
    assert!(v.is_none(), "expected NULL under strict=off, got {v:?}");
}

/// `strict = off` turns a *structural* verifier failure into an
/// empty `text[]` for the array entry point.
#[pg_test]
fn pg_query_array_strict_off_garbage_returns_empty_array() {
    register("default", "T", build_t_schema_bfbs());
    Spi::run("SET pg_flatbuffers.strict = off").expect("SET");
    let v = Spi::get_one::<Vec<Option<String>>>(
        "SELECT flatbuffers_query_array('T:n', '\\x00010203'::bytea)",
    )
    .expect("SPI failure");
    assert_eq!(
        v.as_deref(),
        Some(&[][..]),
        "expected empty text[] under strict=off, got {v:?}",
    );
}

/// `strict = off` turns a *structural* verifier failure into a
/// zero-row setof for the multi entry point.
#[pg_test]
fn pg_query_multi_strict_off_garbage_returns_zero_rows() {
    register("default", "T", build_t_schema_bfbs());
    Spi::run("SET pg_flatbuffers.strict = off").expect("SET");
    let n = Spi::get_one::<i64>(
        "SELECT count(*) FROM flatbuffers_query_multi('T:n', '\\x00010203'::bytea)",
    )
    .expect("SPI failure");
    assert_eq!(n, Some(0));
}

/// Unknown schema name surfaces the `lookup_schema` ERROR
/// verbatim (so operators can grep for typos).
#[pg_test(error = "flatbuffers schema \"nope\" is not registered")]
fn pg_query_unknown_schema_errors() {
    Spi::get_one_with_args::<String>(
        "SELECT flatbuffers_query('nope:T:n', $1)",
        &[build_t_buf(1).into()],
    )
    .expect("SPI failure");
}

// -- struct descent (design §7.2 / §4.3) --

/// SQL-side smoke for `walk_struct`: descend into an inline
/// struct field and stringify a scalar leaf. Full unit coverage
/// of nested-struct descent, absence handling, and step-shape
/// rejection lives in the executor module.
#[pg_test]
fn pg_query_struct_field_scalar_leaf() {
    register("default", "Point", build_point_schema_bfbs());
    let buf = build_point_buf(
        "p",
        TestVec3 {
            x: 1.5,
            y: 2.5,
            z: 3.5,
        },
    );
    let v = Spi::get_one_with_args::<String>(
        "SELECT flatbuffers_query('Point:pos.y', $1)",
        &[buf.into()],
    )
    .expect("SPI failure");
    assert_eq!(v.as_deref(), Some("2.5"));
}

// -- union dispatch (design §7.2 / §4.3) --

/// SQL-side smoke for `walk_union`: auto-dispatch through the
/// `body_type` discriminator and read a string scalar from the
/// active variant table. Full coverage of NONE handling,
/// not-in-variant FieldNotFound, the discriminator field
/// scalar, and step-shape rejection lives in the executor
/// module.
#[pg_test]
fn pg_query_union_dispatch_into_variant() {
    register("default", "Msg", build_msg_schema_bfbs());
    let buf = build_msg_buf_a("hello");
    let v = Spi::get_one_with_args::<String>(
        "SELECT flatbuffers_query('Msg:body.name', $1)",
        &[buf.into()],
    )
    .expect("SPI failure");
    assert_eq!(v.as_deref(), Some("hello"));
}

/// SQL-side smoke for the `|type` terminal: returns the active
/// variant's symbolic name as a string. Full coverage of NONE
/// handling, non-union rejection, and descent-past-leaf
/// rejection lives in the executor module.
#[pg_test]
fn pg_query_union_type_returns_variant_name() {
    register("default", "Msg", build_msg_schema_bfbs());
    let buf = build_msg_buf_a("hello");
    let v = Spi::get_one_with_args::<String>(
        "SELECT flatbuffers_query('Msg:body|type', $1)",
        &[buf.into()],
    )
    .expect("SPI failure");
    assert_eq!(v.as_deref(), Some("A"));
}

// -- vector of struct (design §7.2 / §4.3) --

/// SQL-side smoke for vector-of-struct dispatch: descend into
/// the i'th inline struct element and stringify a scalar leaf.
/// Full unit coverage of `[*]` fanout, OOB / absent handling,
/// and step-shape rejection (struct elements have no `(key)`,
/// and there's no v0.1 textual leaf form for a struct element)
/// lives in the executor module.
#[pg_test]
fn pg_query_vector_of_struct_index_field() {
    register("default", "Bag", build_bag_schema_bfbs());
    let pts = [
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
    ];
    let buf = build_bag_buf(&pts);
    // Element 1 is {4, 5, 6} — read its `y` (= 5).
    let v = Spi::get_one_with_args::<String>(
        "SELECT flatbuffers_query('Bag:points[1].y', $1)",
        &[buf.into()],
    )
    .expect("SPI failure");
    assert_eq!(v.as_deref(), Some("5"));
}

// -- fixed-size arrays inside structs (design §7.2 / §4.3) --

/// SQL-side smoke for `walk_array`: descend into a fixed-size
/// array of inline struct elements and stringify a scalar leaf.
/// Full unit coverage of scalar-element arrays, `[*]` fanout,
/// OOB / absent handling, and step-shape rejection lives in the
/// executor module.
#[pg_test]
fn pg_query_array_of_struct_index_field() {
    register("default", "Holder", build_holder_schema_bfbs());
    let buf = build_holder_buf(TestBundle {
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
    });
    // Element 1 of the struct array is {100, 200, 300} — read y.
    let v = Spi::get_one_with_args::<String>(
        "SELECT flatbuffers_query('Holder:b.pts[1].y', $1)",
        &[buf.into()],
    )
    .expect("SPI failure");
    assert_eq!(v.as_deref(), Some("200"));
}

