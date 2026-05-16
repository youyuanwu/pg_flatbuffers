// -- flatbuffers_root_type --

#[pg_test]
fn pg_root_type_returns_registered_value() {
    register("default", "T", build_t_schema_bfbs());
    let v = Spi::get_one::<String>("SELECT flatbuffers_root_type('default')")
        .expect("SPI failure")
        .expect("NULL from registered schema");
    assert_eq!(v, "T");
}

#[pg_test(error = "flatbuffers schema \"missing\" is not registered")]
fn pg_root_type_unknown_schema_errors() {
    Spi::get_one::<String>("SELECT flatbuffers_root_type('missing')").expect("SPI failure");
}

