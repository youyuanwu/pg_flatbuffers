//! `flatbuffers_query_multi`: set-returning, multi-row queries.

use super::fixtures::*;
use pgrx::prelude::*;

#[pgrx::pg_schema]
mod tests {
    use super::*;

    /// `STRICT` short-circuits NULL inputs \u2014 zero rows, never the
    /// function body.
    #[pg_test]
    fn pg_query_multi_null_buf_returns_zero_rows() {
        let n = Spi::get_one::<i64>(
            "SELECT count(*) FROM flatbuffers_query_multi('B:tags[*]', NULL::bytea)",
        )
        .expect("SPI failure")
        .expect("count is non-null");
        assert_eq!(n, 0);
    }

    /// Empty `bytea` short-circuits to zero rows (mirrors the empty
    /// array `flatbuffers_query_array` returns; just a different
    /// shape).
    #[pg_test]
    fn pg_query_multi_empty_buf_returns_zero_rows() {
        let n = Spi::get_one::<i64>(
            "SELECT count(*) FROM flatbuffers_query_multi('B:tags[*]', ''::bytea)",
        )
        .expect("SPI failure")
        .expect("count is non-null");
        assert_eq!(n, 0);
    }

    /// Happy-path fanout: three tags \u2192 three rows in wire-format
    /// order. `array_agg` round-trips so we can pin the order
    /// assertion in one [`Spi::get_one`] call without needing a
    /// cursor.
    #[pg_test]
    fn pg_query_multi_happy_path_strings() {
        register("default", "B", build_b_schema_bfbs());
        let buf = build_b_buf(Some(&["red", "green", "blue"]));
        let v = Spi::get_one_with_args::<Vec<Option<String>>>(
            "SELECT array_agg(t ORDER BY ord) \
             FROM flatbuffers_query_multi('B:tags[*]', $1) \
                 WITH ORDINALITY AS s(t, ord)",
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

    /// Absent vector under `[*]` \u2192 zero rows.
    #[pg_test]
    fn pg_query_multi_absent_vector_is_zero_rows() {
        register("default", "B", build_b_schema_bfbs());
        let buf = build_b_buf(None);
        let n = Spi::get_one_with_args::<i64>(
            "SELECT count(*) FROM flatbuffers_query_multi('B:tags[*]', $1)",
            &[buf.into()],
        )
        .expect("SPI failure")
        .expect("count is non-null");
        assert_eq!(n, 0);
    }

    /// `LATERAL` against a single-row source: the SETOF wrapper is
    /// the whole reason this function exists, so pin a tiny
    /// representative shape.
    #[pg_test]
    fn pg_query_multi_lateral_join() {
        register("default", "B", build_b_schema_bfbs());
        let buf = build_b_buf(Some(&["a", "b"]));
        let v = Spi::get_one_with_args::<Vec<Option<String>>>(
            "SELECT array_agg(tag) \
             FROM (SELECT $1::bytea AS payload) src, \
                  LATERAL flatbuffers_query_multi('B:tags[*]', src.payload) AS tag",
            &[buf.into()],
        )
        .expect("SPI failure")
        .expect("NULL from happy path");
        assert_eq!(v, vec![Some("a".to_owned()), Some("b".to_owned())]);
    }
}
