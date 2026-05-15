//! Catalog functions for `flatbuffers_schemas`.
//!
//! See `docs/design.md` §4.1, §4.2, and §10. The catalog table itself
//! and its grants are emitted by `sql/catalog.sql`; this module hosts
//! the Rust-side functions referenced by that SQL (notably the CHECK
//! constraint on the `bfbs` column).

use pgrx::prelude::*;

/// Verify that a candidate `.bfbs` blob is a well-formed FlatBuffers
/// `reflection::Schema`. Raises `ERROR` on failure; returns silently
/// on success.
///
/// Used by:
///
/// - The `CHECK (flatbuffers_validate_schema(bfbs))`-style guard on
///   `flatbuffers_schemas.bfbs` (so a bad blob never reaches the
///   cache; see `docs/design.md` §4.1).
/// - Operators wishing to validate before INSERT.
///
/// # Soundness
///
/// Uses `flatbuffers_reflection::root_as_schema`, the safe verifying
/// entry point. It runs the standard FlatBuffers verifier (offset,
/// vector-length, alignment, and string termination checks) before
/// returning a `Schema` view. We discard the view; only the verifier
/// outcome matters here.
#[pg_extern(immutable, parallel_safe, strict)]
fn flatbuffers_validate_schema(bfbs: &[u8]) -> bool {
    match flatbuffers_reflection::reflection::root_as_schema(bfbs) {
        Ok(_schema) => true,
        Err(e) => {
            // ereport(ERROR, ...) — pgrx's `error!` macro mirrors PG's
            // ereport(ERROR), longjmp'ing back to the SPI/executor.
            error!("invalid FlatBuffers reflection schema: {e}");
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(any(test, feature = "pg_test"))]
#[pg_schema]
mod tests {
    use pgrx::prelude::*;

    // The verifier's "Range [a, b) is out of bounds" message is fully
    // deterministic given fixed input bytes:
    //   * `\x00\x01\x02\x03` → root offset = 0x03020100 = 50462976
    //   * `\x01`             → not enough bytes to read the 4-byte root offset
    //
    // pgrx-tests does *exact* string equality on `error = "..."` and
    // pgrx wraps Rust-side ereports with a trailing `\n\n` (backtrace
    // separator), so we include it in the expected literals.

    /// Random bytes are never a valid FlatBuffers Schema; the CHECK
    /// constraint must reject them with ERROR.
    #[pg_test(
        error = "invalid FlatBuffers reflection schema: Range [50462976, 50462980) is out of bounds.\n\n"
    )]
    fn rejects_garbage_bfbs() {
        Spi::run(
            "INSERT INTO flatbuffers_schemas (name, bfbs, root_table) \
             VALUES ('garbage', '\\x00010203'::bytea, 'NoSuchTable')",
        )
        .expect("SPI failure");
    }

    /// A short blob (too few bytes for a root offset) must also be
    /// rejected.
    #[pg_test(error = "invalid FlatBuffers reflection schema: Range [0, 4) is out of bounds.\n\n")]
    fn rejects_truncated_bfbs() {
        Spi::run(
            "INSERT INTO flatbuffers_schemas (name, bfbs, root_table) \
             VALUES ('truncated', '\\x01'::bytea, 'NoSuchTable')",
        )
        .expect("SPI failure");
    }

    /// A role that is not a member of `flatbuffers_admin` and is not a
    /// superuser cannot INSERT into the catalog. PUBLIC has SELECT
    /// only (see sql/catalog.sql).
    #[pg_test(error = "permission denied for table flatbuffers_schemas")]
    fn unprivileged_insert_denied() {
        // pgrx-tests wraps each test in a transaction — the role is
        // discarded on rollback, so this is reproducible across runs.
        Spi::run(
            "CREATE ROLE flatbuffers_unprivileged_test NOLOGIN; \
             SET LOCAL ROLE flatbuffers_unprivileged_test",
        )
        .expect("setup SPI failure");
        Spi::run(
            "INSERT INTO flatbuffers_schemas (name, bfbs, root_table) \
             VALUES ('x', ''::bytea, 'X')",
        )
        .expect("SPI failure");
    }

    /// PUBLIC may SELECT from the catalog. Empty result is fine; we
    /// just need the SELECT itself to succeed under an unprivileged
    /// role.
    #[pg_test]
    fn unprivileged_select_allowed() {
        Spi::run(
            "CREATE ROLE flatbuffers_reader_test NOLOGIN; \
             SET LOCAL ROLE flatbuffers_reader_test",
        )
        .expect("setup SPI failure");

        let count: i64 = Spi::get_one("SELECT count(*) FROM flatbuffers_schemas")
            .expect("SPI failure")
            .expect("NULL from count(*)");
        assert_eq!(count, 0);
    }
}
