//! `flatbuffers_from_json` / `flatbuffers_from_json_text`: SQL surface
//! (NULL inputs / happy path / round-trip via to_json / unknown field /
//! invalid JSON text / root-table mismatch).

use super::fixtures::*;
use pgrx::prelude::*;

#[pgrx::pg_schema]
mod tests {
    use super::*;

    /// `STRICT` short-circuits NULL inputs without touching the
    /// function body. Verifies pgrx wired the attribute correctly.
    #[pg_test]
    fn pg_from_json_null_input_returns_null() {
        let v = Spi::get_one::<Vec<u8>>("SELECT flatbuffers_from_json('T', NULL::jsonb)")
            .expect("SPI failure");
        assert!(v.is_none(), "expected NULL");
    }

    #[pg_test]
    fn pg_from_json_text_null_input_returns_null() {
        let v = Spi::get_one::<Vec<u8>>("SELECT flatbuffers_from_json_text('T', NULL::text)")
            .expect("SPI failure");
        assert!(v.is_none(), "expected NULL");
    }

    /// Happy path: build an empty `T` from `{}` (no required
    /// fields), then verify the resulting bytea is non-empty and
    /// verifies against the schema.
    #[pg_test]
    fn pg_from_json_happy_path_empty_object() {
        register("default", "T", build_t_schema_bfbs());
        let buf = Spi::get_one::<Vec<u8>>("SELECT flatbuffers_from_json('T', '{}'::jsonb)")
            .expect("SPI failure")
            .expect("non-null bytea");
        assert!(!buf.is_empty());
        // Round trip: read `n` back — should be the schema default 0.
        let n =
            Spi::get_one_with_args::<String>("SELECT flatbuffers_query('T:n', $1)", &[buf.into()])
                .expect("SPI failure");
        assert_eq!(n.as_deref(), Some("0"));
    }

    /// Round-trip via to_json: build a buffer from JSON, then ask
    /// to_json to re-emit it. Result should equal the input.
    #[pg_test]
    fn pg_from_json_round_trips_through_to_json() {
        register("default", "T", build_t_schema_bfbs());
        let buf =
            Spi::get_one::<Vec<u8>>("SELECT flatbuffers_from_json('T', '{\"n\": 99}'::jsonb)")
                .expect("SPI failure")
                .expect("non-null bytea");
        let back = Spi::get_one_with_args::<pgrx::JsonB>(
            "SELECT flatbuffers_to_json('T', $1)",
            &[buf.into()],
        )
        .expect("SPI failure")
        .expect("non-null JSON");
        assert_eq!(back.0, serde_json::json!({ "n": 99 }));
    }

    /// `_text` variant accepts a JSON text string and behaves
    /// identically to the `jsonb` form.
    #[pg_test]
    fn pg_from_json_text_happy_path() {
        register("default", "T", build_t_schema_bfbs());
        let buf = Spi::get_one::<Vec<u8>>("SELECT flatbuffers_from_json_text('T', '{\"n\": 7}')")
            .expect("SPI failure")
            .expect("non-null bytea");
        let back = Spi::get_one_with_args::<pgrx::JsonB>(
            "SELECT flatbuffers_to_json('T', $1)",
            &[buf.into()],
        )
        .expect("SPI failure")
        .expect("non-null JSON");
        assert_eq!(back.0, serde_json::json!({ "n": 7 }));
    }

    /// Unknown JSON key → ERROR (no `from_json_unknown = ignore`
    /// GUC in v0.1).
    #[pg_test]
    #[should_panic(expected = "no field named")]
    fn pg_from_json_unknown_key_errors() {
        register("default", "T", build_t_schema_bfbs());
        let _ =
            Spi::get_one::<Vec<u8>>("SELECT flatbuffers_from_json('T', '{\"bogus\": 1}'::jsonb)");
    }

    /// `_text` variant: invalid JSON syntax → ERROR with a parse
    /// message.
    #[pg_test]
    #[should_panic(expected = "invalid JSON")]
    fn pg_from_json_text_invalid_syntax_errors() {
        register("default", "T", build_t_schema_bfbs());
        let _ =
            Spi::get_one::<Vec<u8>>("SELECT flatbuffers_from_json_text('T', 'not json at all')");
    }

    /// Root-table mismatch (schema registered for `T` but caller
    /// asks for `OtherTable`) → ERROR. Mirrors the to_json policy.
    #[pg_test]
    #[should_panic(expected = "is registered with root table")]
    fn pg_from_json_root_table_mismatch_errors() {
        register("default", "T", build_t_schema_bfbs());
        let _ = Spi::get_one::<Vec<u8>>("SELECT flatbuffers_from_json('OtherTable', '{}'::jsonb)");
    }
}
