// -- flatbuffers_query_array --

/// `STRICT` short-circuits NULL inputs.
#[pg_test]
fn pg_query_array_null_buf_returns_null() {
    // Result is the SQL NULL array, *not* an empty array — STRICT
    // bypasses the function body entirely. (`text[]` arrives back
    // as `Vec<Option<String>>` so the outer Option is the
    // array-level NULL.)
    let v = Spi::get_one::<Vec<Option<String>>>(
        "SELECT flatbuffers_query_array('B:tags[*]', NULL::bytea)",
    )
    .expect("SPI failure");
    assert!(v.is_none(), "expected NULL, got {v:?}");
}

/// Empty `bytea` short-circuits to the empty array (not NULL,
/// not ERROR). Mirrors §10's "absent payload" sentinel; downstream
/// SQL using `array_length(... , 1)` will see `NULL` for the
/// "no matches" cardinality.
#[pg_test]
fn pg_query_array_empty_buf_returns_empty_array() {
    let v = Spi::get_one::<Vec<Option<String>>>(
        "SELECT flatbuffers_query_array('B:tags[*]', ''::bytea)",
    )
    .expect("SPI failure")
    .expect("empty bytea must produce empty array, not NULL");
    assert!(v.is_empty(), "got {v:?}");
}

/// Happy-path fanout: three tags → a three-element `text[]`,
/// preserving wire order.
#[pg_test]
fn pg_query_array_happy_path_strings() {
    register("default", "B", build_b_schema_bfbs());
    let buf = build_b_buf(Some(&["red", "green", "blue"]));
    let v = Spi::get_one_with_args::<Vec<Option<String>>>(
        "SELECT flatbuffers_query_array('B:tags[*]', $1)",
        &[buf.into()],
    )
    .expect("SPI failure")
    .expect("NULL from happy path");
    assert_eq!(
        v,
        vec![
            Some("red".to_owned()),
            Some("green".to_owned()),
            Some("blue".to_owned()),
        ],
    );
}

/// Absent vector under `[*]` must be the empty array (no items
/// to fan out over) — distinct from `[i]` which would yield a
/// one-element NULL array.
#[pg_test]
fn pg_query_array_absent_vector_is_empty_array() {
    register("default", "B", build_b_schema_bfbs());
    let buf = build_b_buf(None);
    let v = Spi::get_one_with_args::<Vec<Option<String>>>(
        "SELECT flatbuffers_query_array('B:tags[*]', $1)",
        &[buf.into()],
    )
    .expect("SPI failure")
    .expect("absent vector must be empty array, not NULL");
    assert!(v.is_empty(), "got {v:?}");
}

/// `flatbuffers_query` returning the *first* leaf — we now
/// share the executor result with `flatbuffers_query_array`, so
/// pin the contract that `_query` against `[*]` collapses to the
/// first element.
#[pg_test]
fn pg_query_first_leaf_under_all() {
    register("default", "B", build_b_schema_bfbs());
    let buf = build_b_buf(Some(&["alpha", "beta"]));
    let v = Spi::get_one_with_args::<String>(
        "SELECT flatbuffers_query('B:tags[*]', $1)",
        &[buf.into()],
    )
    .expect("SPI failure");
    assert_eq!(v.as_deref(), Some("alpha"));
}

/// `Step::MapKey` end-to-end: lookup `Catalog:entries[beta].name`
/// against a `Catalog` whose `Entry.name` is the `(key)` field.
/// This is the SQL-side smoke for the executor's
/// `walk_vector_at_map_key` (full unit coverage of hits, misses,
/// empty / absent vectors, etc. lives in the executor module).
#[pg_test]
fn pg_query_map_key_hit() {
    register("default", "Catalog", build_catalog_schema_bfbs());
    let buf = build_catalog_buf(&["alpha", "beta", "gamma"]);
    let v = Spi::get_one_with_args::<String>(
        "SELECT flatbuffers_query('Catalog:entries[beta].name', $1)",
        &[buf.into()],
    )
    .expect("SPI failure");
    assert_eq!(v.as_deref(), Some("beta"));
}

/// Map-key miss surfaces as SQL `NULL` rather than `ERROR` —
/// matches the OOB-index short-circuit so a typo on the SQL
/// side doesn't abort the statement.
#[pg_test]
fn pg_query_map_key_miss_returns_null() {
    register("default", "Catalog", build_catalog_schema_bfbs());
    let buf = build_catalog_buf(&["alpha", "gamma"]);
    let v = Spi::get_one_with_args::<String>(
        "SELECT flatbuffers_query('Catalog:entries[zzz].name', $1)",
        &[buf.into()],
    )
    .expect("SPI failure");
    assert!(v.is_none(), "got {v:?}");
}

/// Default `key_lookup_strict = on` bisects the (key)-vector
/// under the FlatBuffers sorted contract. A writer that emitted
/// an unsorted vector will see *silent misses* on keys whose
/// position the bisect can't reach — pin that behaviour so the
/// operator-visible signal (`escalate to key_lookup_strict =
/// off`) is reachable, and so a regression that fell back to
/// linear scan under strict-on would surface.
#[pg_test]
fn pg_query_map_key_unsorted_silent_miss_under_default() {
    register("default", "Catalog", build_catalog_schema_bfbs());
    // "alpha" precedes "beta" / "gamma" lexicographically but
    // sits at the tail — same trace as the executor's
    // `vector_map_key_binary_search_unsorted_silent_miss`.
    let buf = build_catalog_buf(&["gamma", "beta", "alpha"]);
    let v = Spi::get_one_with_args::<String>(
        "SELECT flatbuffers_query('Catalog:entries[alpha].name', $1)",
        &[buf.into()],
    )
    .expect("SPI failure");
    assert!(
        v.is_none(),
        "binary search on unsorted vec must silently miss; got {v:?}",
    );
}

/// `SET pg_flatbuffers.key_lookup_strict = off` flips the
/// executor to linear scan, which finds the key regardless of
/// vector order. End-to-end smoke that the GUC reaches the
/// executor through `current_execute_options` on the very next
/// SQL call.
#[pg_test]
fn pg_query_map_key_unsorted_found_under_key_lookup_strict_off() {
    register("default", "Catalog", build_catalog_schema_bfbs());
    let buf = build_catalog_buf(&["gamma", "beta", "alpha"]);
    Spi::run("SET pg_flatbuffers.key_lookup_strict = off").expect("SPI: SET key_lookup_strict");
    let v = Spi::get_one_with_args::<String>(
        "SELECT flatbuffers_query('Catalog:entries[alpha].name', $1)",
        &[buf.into()],
    )
    .expect("SPI failure");
    assert_eq!(v.as_deref(), Some("alpha"));
}

/// `field|keys` enumerates the `(key)`-annotated field of every
/// entry in wire order. Surfaces as a `text[]` via
/// `flatbuffers_query_array`; full unit coverage (duplicates,
/// empty / absent vectors, scalar-vector rejection) lives in
/// the executor module.
#[pg_test]
fn pg_query_map_keys_array_returns_keys_in_wire_order() {
    register("default", "Catalog", build_catalog_schema_bfbs());
    let buf = build_catalog_buf(&["alpha", "beta", "gamma"]);
    let v = Spi::get_one_with_args::<Vec<Option<String>>>(
        "SELECT flatbuffers_query_array('Catalog:entries|keys', $1)",
        &[buf.into()],
    )
    .expect("SPI failure")
    .expect("non-NULL result");
    assert_eq!(
        v,
        vec![
            Some("alpha".to_owned()),
            Some("beta".to_owned()),
            Some("gamma".to_owned()),
        ],
    );
}

/// Same query through `flatbuffers_query_multi` (SETOF text)
/// yields one row per key in the same wire-format order.
#[pg_test]
fn pg_query_map_keys_multi_yields_one_row_per_key() {
    register("default", "Catalog", build_catalog_schema_bfbs());
    let buf = build_catalog_buf(&["alpha", "beta", "gamma"]);
    let v = Spi::get_one_with_args::<Vec<Option<String>>>(
        "SELECT array_agg(t ORDER BY ord) \
         FROM flatbuffers_query_multi('Catalog:entries|keys', $1) \
             WITH ORDINALITY AS s(t, ord)",
        &[buf.into()],
    )
    .expect("SPI failure")
    .expect("non-NULL result");
    assert_eq!(
        v,
        vec![
            Some("alpha".to_owned()),
            Some("beta".to_owned()),
            Some("gamma".to_owned()),
        ],
    );
}

