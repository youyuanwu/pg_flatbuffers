//! Catalog functions for `flatbuffers_schemas`.
//!
//! See `docs/design/catalog-and-schemas.md`, `docs/design/sql-surface.md`,
//! and `docs/design/safety.md`. The catalog table itself and its grants are
//! emitted by `sql/catalog.sql`; this module hosts the Rust-side functions
//! referenced by that SQL (notably the CHECK constraint on the `bfbs`
//! column).

use pgrx::prelude::*;

/// Verify that a candidate `.bfbs` blob is a well-formed FlatBuffers
/// `reflection::Schema`. Raises `ERROR` on failure; returns silently
/// on success.
///
/// Used by:
///
/// - The `CHECK (flatbuffers_validate_schema(bfbs))`-style guard on
///   `flatbuffers_schemas.bfbs` (so a bad blob never reaches the
///   cache; see `docs/design/catalog-and-schemas.md`).
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

/// Trigger fired AFTER any INSERT/UPDATE/DELETE/TRUNCATE on
/// `flatbuffers_schemas`. Emits a relcache invalidation message for
/// the catalog table, which:
///
/// 1. Drops this backend's cached schemas immediately (the local
///    invalidation queue is processed at the next
///    `CommandCounterIncrement` / `AcceptInvalidationMessages`).
/// 2. Is propagated to every other backend via shared sinval on
///    transaction commit, so they drop their caches at the next
///    statement boundary.
///
/// The trigger is needed because Postgres does **not** automatically
/// emit `CacheInvalidateRelcache` on plain DML against user tables
/// (only on DDL, and on row changes to system catalogs).
#[pg_trigger]
fn flatbuffers_schemas_invalidate_trigger<'a>(
    trigger: &'a PgTrigger<'a>,
) -> Result<Option<PgHeapTuple<'a, impl WhoAllocated>>, PgTriggerError> {
    let relid = trigger.relid()?;
    // SAFETY: stable Postgres C API; safe to call from any backend
    // context. Adds a message to the local invalidation queue, which
    // PG processes at the next safe boundary.
    unsafe { ::pgrx::pg_sys::CacheInvalidateRelcacheByRelid(relid) };

    // Pass-through: returning the row that was being modified is the
    // standard behaviour for STATEMENT-level AFTER triggers (the value
    // is ignored by the executor but the function signature requires
    // a tuple). For TRUNCATE there is no OLD/NEW, so return None.
    Ok(trigger.old().or_else(|| trigger.new()))
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
