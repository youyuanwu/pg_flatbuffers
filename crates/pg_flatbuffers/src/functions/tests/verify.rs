// -- flatbuffers_verify --

#[pg_test]
fn pg_verify_empty_buf_is_false() {
    register("default", "T", build_t_schema_bfbs());
    let v = Spi::get_one::<bool>("SELECT flatbuffers_verify('T', ''::bytea)")
        .expect("SPI failure")
        .expect("NULL from non-strict-on-empty path");
    assert!(!v);
}

#[pg_test]
fn pg_verify_happy_path_is_true() {
    register("default", "T", build_t_schema_bfbs());
    let buf = build_t_buf(7);
    let v = Spi::get_one_with_args::<bool>("SELECT flatbuffers_verify('T', $1)", &[buf.into()])
        .expect("SPI failure")
        .expect("NULL from happy path");
    assert!(v);
}

/// A schema-prefixed table name must agree with the registered
/// root table.
#[pg_test]
fn pg_verify_root_table_mismatch_is_false() {
    register("default", "T", build_t_schema_bfbs());
    let buf = build_t_buf(7);
    let v = Spi::get_one_with_args::<bool>("SELECT flatbuffers_verify('NotT', $1)", &[buf.into()])
        .expect("SPI failure")
        .expect("NULL");
    assert!(!v);
}

#[pg_test]
fn pg_verify_garbage_buf_is_false() {
    register("default", "T", build_t_schema_bfbs());
    let v = Spi::get_one::<bool>("SELECT flatbuffers_verify('T', '\\x00010203'::bytea)")
        .expect("SPI failure")
        .expect("NULL");
    assert!(!v);
}

/// Malformed `table_name` (three components — common copy/paste
/// of a full `flatbuffers_query` argument string) is a *user*
/// error and must `ERROR`. A single-colon input like `"T:n"` is
/// *not* malformed: it parses as `(schema=T, table=n)`, which
/// then surfaces as an unknown-schema error from the cache —
/// covered by [`pg_query_unknown_schema_errors`].
#[pg_test(
    error = "flatbuffers_verify: invalid table_name \"default:T:n\": expected `[schema:]table`, found extra `:`"
)]
fn pg_verify_malformed_table_name_errors() {
    register("default", "T", build_t_schema_bfbs());
    Spi::get_one_with_args::<bool>(
        "SELECT flatbuffers_verify('default:T:n', $1)",
        &[build_t_buf(0).into()],
    )
    .expect("SPI failure");
}

