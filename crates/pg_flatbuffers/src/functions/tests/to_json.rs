//! `flatbuffers_to_json` / `flatbuffers_to_json_text`: SQL surface
//! (NULL / empty / happy path / verifier-failure-always-errors / root-table
//! mismatch).

use super::fixtures::*;
use pgrx::prelude::*;

#[pgrx::pg_schema]
mod tests {
    use super::*;

    /// `STRICT` short-circuits NULL inputs without touching the
    /// function body.
    #[pg_test]
    fn pg_to_json_null_buf_returns_null() {
        let v = Spi::get_one::<pgrx::JsonB>("SELECT flatbuffers_to_json('T', NULL::bytea)")
            .expect("SPI failure");
        assert!(v.is_none(), "expected NULL");
    }

    /// Empty `bytea` is the "absent payload" sentinel (§10): NULL,
    /// not an ERROR. JSON conversion does NOT raise on empty bufs.
    #[pg_test]
    fn pg_to_json_empty_buf_returns_null() {
        let v = Spi::get_one::<pgrx::JsonB>("SELECT flatbuffers_to_json('T', ''::bytea)")
            .expect("SPI failure");
        assert!(v.is_none(), "expected NULL");
    }

    /// Happy path: registered schema + valid buffer → JSON object.
    /// Uses the simple `T { n:int }` fixture; expects the present
    /// scalar (42) rendered as a JSON number.
    #[pg_test]
    fn pg_to_json_happy_path_table() {
        register("default", "T", build_t_schema_bfbs());
        let buf = build_t_buf(42);
        let v = Spi::get_one_with_args::<pgrx::JsonB>(
            "SELECT flatbuffers_to_json('T', $1)",
            &[buf.into()],
        )
        .expect("SPI failure")
        .expect("non-null JSON");
        assert_eq!(v.0, serde_json::json!({ "n": 42 }));
    }

    /// Absent scalar (value == default == 0) emits the schema
    /// default in JSON (flatc parity — JSON encoding doesn't honor
    /// the read-side `fill_scalar_defaults = off` knob, since the
    /// JSON consumer has no other way to learn the default).
    #[pg_test]
    fn pg_to_json_absent_scalar_emits_default() {
        register("default", "T", build_t_schema_bfbs());
        let buf = build_t_buf(0); // elided slot
        let v = Spi::get_one_with_args::<pgrx::JsonB>(
            "SELECT flatbuffers_to_json('T', $1)",
            &[buf.into()],
        )
        .expect("SPI failure")
        .expect("non-null JSON");
        assert_eq!(v.0, serde_json::json!({ "n": 0 }));
    }

    /// `_text` variant emits the same JSON serialized as `text`.
    #[pg_test]
    fn pg_to_json_text_returns_compact_string() {
        register("default", "T", build_t_schema_bfbs());
        let buf = build_t_buf(42);
        let v = Spi::get_one_with_args::<String>(
            "SELECT flatbuffers_to_json_text('T', $1)",
            &[buf.into()],
        )
        .expect("SPI failure")
        .expect("non-null text");
        assert_eq!(v, r#"{"n":42}"#);
    }

    /// Root-table mismatch (schema registered for `T` but caller
    /// asks for `OtherTable`) is a *config* error → ERROR, not
    /// NULL. Mirrors the `flatbuffers_verify` policy.
    #[pg_test]
    #[should_panic(expected = "is registered with root table")]
    fn pg_to_json_root_table_mismatch_errors() {
        register("default", "T", build_t_schema_bfbs());
        let buf = build_t_buf(42);
        let _ = Spi::get_one_with_args::<pgrx::JsonB>(
            "SELECT flatbuffers_to_json('OtherTable', $1)",
            &[buf.into()],
        );
    }

    /// Garbage bytes → verifier ERROR, NEVER NULL. Per design §8,
    /// JSON conversion always errors on a malformed buffer; the
    /// `strict` GUC is not consulted.
    #[pg_test]
    #[should_panic(expected = "flatbuffers_to_json")]
    fn pg_to_json_garbage_buf_errors() {
        register("default", "T", build_t_schema_bfbs());
        let _ =
            Spi::get_one::<pgrx::JsonB>("SELECT flatbuffers_to_json('T', '\\xdeadbeef'::bytea)");
    }

    /// Even with `strict = off`, JSON conversion still errors on a
    /// malformed buffer (design §8). This is the asymmetry with
    /// `flatbuffers_query`, which would silently return NULL.
    #[pg_test]
    #[should_panic(expected = "flatbuffers_to_json")]
    fn pg_to_json_garbage_errors_even_with_strict_off() {
        register("default", "T", build_t_schema_bfbs());
        Spi::run("SET pg_flatbuffers.strict = off").expect("SPI: SET strict");
        let _ =
            Spi::get_one::<pgrx::JsonB>("SELECT flatbuffers_to_json('T', '\\xdeadbeef'::bytea)");
    }
}
