// -- GUC plumbing (design §10) --

/// End-to-end smoke that `SET pg_flatbuffers.max_tables` actually
/// reaches the verifier on a subsequent SQL call. A `Catalog`
/// with one `Entry` exposes two tables to the verifier, so a
/// `max_tables = 1` cap rejects the payload — surfaced through
/// `flatbuffers_verify`'s boolean contract as `false` (the
/// function never raises on a buffer-level failure, per
/// `flatbuffers_verify`'s docstring). The GUC's own
/// SET / SHOW / range-guard coverage lives in `guc.rs`.
#[pg_test]
fn pg_guc_max_tables_plumbs_to_verifier() {
    register("default", "Catalog", build_catalog_schema_bfbs());
    let buf = build_catalog_buf(&["alpha"]);

    // Baseline: default `max_tables = 1_000_000` accepts the
    // 2-table buffer.
    let baseline = Spi::get_one_with_args::<bool>(
        "SELECT flatbuffers_verify('Catalog', $1)",
        &[buf.clone().into()],
    )
    .expect("SPI failure")
    .expect("NULL from happy path");
    assert!(baseline, "baseline verify must accept");

    // Constrained: `max_tables = 1` is below the buffer's
    // 2-table requirement, so the verifier rejects.
    Spi::run("SET pg_flatbuffers.max_tables = 1").expect("SPI: SET max_tables");
    let constrained =
        Spi::get_one_with_args::<bool>("SELECT flatbuffers_verify('Catalog', $1)", &[buf.into()])
            .expect("SPI failure")
            .expect("NULL from constrained path");
    assert!(
        !constrained,
        "max_tables = 1 must reject a 2-table Catalog buffer",
    );
}

