//! Bounded reflection-driven FlatBuffers verifier.
//!
//! See `docs/design.md` §10 ("Safety / untrusted input"). Every read
//! path that touches caller-supplied bytes — `flatbuffers_query*`,
//! `flatbuffers_to_json*`, the `CHECK` constraint behind
//! `flatbuffers_verify` — funnels through [`verify`] first so a
//! malicious payload cannot trigger out-of-bounds reads in the
//! unchecked accessors used by the executor (see
//! `schema_cache::CachedSchema::schema`).
//!
//! This module is intentionally pure Rust (no `pgrx` dependency) so
//! that the verifier and its bounds can be exercised by `cargo test`
//! without spinning up a Postgres backend. The Postgres-side
//! integration (the SQL `flatbuffers_verify` function and GUC
//! plumbing) lives in `functions.rs` and will be added in the next
//! slice.
//!
//! ## What is checked
//!
//! [`verify`] delegates to
//! [`flatbuffers_reflection::reflection_verifier::verify_with_options`],
//! which walks the buffer guided by the reflection schema and:
//!
//! - validates every offset is in-bounds and properly aligned,
//! - bounds total table count (`max_tables`),
//! - bounds nested table depth (`max_depth`),
//! - bounds the apparent (DAG-expanded) size (`max_apparent_size`),
//! - checks that strings are well-formed and required fields are
//!   present.
//!
//! What it does *not* yet check (deferred to the executor slice that
//! consumes [`verify`]):
//!
//! - `(key)`-annotated vector sortedness / no-duplicates
//!   (`pg_flatbuffers.key_lookup_strict`, design §7.2 step 4).
//! - Union-discriminator/value matching for vectors of unions
//!   (design §4.3 "Vector of unions"). Single-value unions are
//!   covered by the underlying verifier.
//!
//! ## What is *deferred*
//!
//! - **Content-hashed result cache** (design §10 "Verifier result
//!   caching"). The cache is per-statement-scoped and dropped on
//!   memory-context reset, which only makes sense once the executor
//!   exists to call us many times per statement. Adding `blake3`
//!   speculatively would be cargo-cult dependency growth.
//! - **GUC wiring** for [`Bounds`]. For now [`Bounds::default`]
//!   matches the design defaults (`max_depth=64`,
//!   `max_tables=1_000_000`, `max_apparent_size=64 MiB`). When the
//!   GUC slice lands, callers will read each GUC and pass an explicit
//!   [`Bounds`] instead of `default()`.

use flatbuffers::VerifierOptions;
use flatbuffers_reflection::reflection::Schema;
use flatbuffers_reflection::{FlatbufferError, SafeBuffer};
use thiserror::Error;

/// Verifier resource bounds. Map 1:1 to the `pg_flatbuffers.max_*`
/// GUCs in `docs/design.md` §10. Every field is `usize` to match
/// [`flatbuffers::VerifierOptions`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bounds {
    /// Maximum nested-table depth. Defaults to 64. Bypass would
    /// permit a stack-exhaustion DoS on the verifier itself.
    pub max_depth: usize,
    /// Maximum number of tables visited during verification.
    /// Defaults to 1_000_000. Bypass would permit a quadratic-time
    /// DoS on payloads that share the same sub-table from many
    /// parents.
    pub max_tables: usize,
    /// Maximum apparent (DAG-expanded) byte size. Defaults to 64
    /// MiB. Bypass would permit unbounded memory amplification on
    /// `to_json` paths that materialise the expansion.
    ///
    /// Stored as bytes (not MiB) because that is the unit the
    /// upstream verifier uses; the `pg_flatbuffers.max_apparent_size_mb`
    /// GUC will be multiplied by `1024 * 1024` at the call site.
    pub max_apparent_size: usize,
}

impl Default for Bounds {
    fn default() -> Self {
        Self {
            max_depth: 64,
            max_tables: 1_000_000,
            max_apparent_size: 64 * 1024 * 1024,
        }
    }
}

impl Bounds {
    /// Project onto `flatbuffers::VerifierOptions`. Preserves the
    /// upstream default for `ignore_missing_null_terminator` (`false`)
    /// — we do not want to silently accept C++-old-style strings.
    fn to_options(self) -> VerifierOptions {
        VerifierOptions {
            max_depth: self.max_depth,
            max_tables: self.max_tables,
            max_apparent_size: self.max_apparent_size,
            ignore_missing_null_terminator: false,
        }
    }
}

/// Errors returned by [`verify`]. The string-shaped `Invalid` variant
/// preserves the verifier's offset/range diagnostic; we deliberately
/// do not parse it, since its shape is part of the upstream crate's
/// public contract.
#[derive(Error, Debug)]
pub enum VerifyError {
    /// The buffer was zero bytes. Per design §10 this is
    /// reported separately from [`VerifyError::TooSmall`] so callers
    /// like `flatbuffers_query` can map empty `bytea` to SQL `NULL`
    /// (the "absent payload" contract) without touching the verifier.
    #[error("buffer is empty")]
    Empty,

    /// Smaller than 4 bytes — there is no room for the FlatBuffers
    /// root offset. Reported separately because the upstream verifier
    /// returns the somewhat opaque `Range [0, 4) is out of bounds`
    /// for this case.
    #[error("buffer is {len} byte(s); minimum is 4 (FlatBuffers root offset)")]
    TooSmall { len: usize },

    /// The schema does not declare a `root_table`. Reported
    /// separately because `verify_with_options` returns the same
    /// error code (`InvalidSchema`) for this *and* a malformed root
    /// offset, and operators need to distinguish the two when
    /// debugging registrations.
    #[error("schema has no root_table; cannot verify a buffer against it")]
    NoRootTable,

    /// The buffer was rejected by the upstream verifier (offset out
    /// of range, depth/tables/size bound exceeded, missing required
    /// field, etc.). The string is the upstream diagnostic.
    #[error("buffer rejected by FlatBuffers verifier: {0}")]
    Invalid(String),
}

impl VerifyError {
    /// True if the failure is a *bound exceedance* (depth, tables,
    /// apparent size) rather than a structural invalidity. Used by
    /// future `flatbuffers_query` call sites where
    /// `pg_flatbuffers.strict = off` substitutes `NULL` for
    /// structural failures but still ERRORs on bound exceedance —
    /// see design §10 ("strict does not relax bounds").
    ///
    /// The upstream verifier exposes bound failures only through the
    /// error message (no machine-readable code), so this is a string
    /// match on stable substrings of `flatbuffers::InvalidFlatbuffer`'s
    /// `Display` output. Conservative: callers can treat `false` as
    /// "not provably a bound failure" without misclassifying anything
    /// as a structural error.
    #[cfg_attr(
        not(test),
        allow(
            dead_code,
            reason = "only consumed from the in-module test suite; production call site lands with the deferred `pg_flatbuffers.strict` GUC plumbing (§10)"
        )
    )]
    pub fn is_bound_exceedance(&self) -> bool {
        match self {
            VerifyError::Invalid(msg) => {
                msg.contains("depth") && msg.contains("limit")
                    || msg.contains("tables") && msg.contains("limit")
                    || msg.contains("apparent size")
            }
            _ => false,
        }
    }
}

/// Verify `buf` against `schema` under `bounds`. On success the buffer
/// is safe to read with the unchecked accessors used by the executor
/// hot path (see `schema_cache::CachedSchema::schema`).
///
/// This function is **idempotent and side-effect free**: it allocates
/// only an internal `HashMap<usize, i32>` (used by the upstream
/// verifier to deduplicate buffer locations) and a `Vec<bool>`
/// proportional to `buf.len()`. It does not touch any global state,
/// so it is safe to call from any thread or callback.
pub fn verify(buf: &[u8], schema: &Schema<'_>, bounds: &Bounds) -> Result<(), VerifyError> {
    if buf.is_empty() {
        return Err(VerifyError::Empty);
    }
    if buf.len() < 4 {
        return Err(VerifyError::TooSmall { len: buf.len() });
    }
    if schema.root_table().is_none() {
        return Err(VerifyError::NoRootTable);
    }

    let opts = bounds.to_options();
    // `SafeBuffer::new_with_options` is the upstream public entry
    // point that wraps `verify_with_options` (which itself is
    // crate-private). We discard the returned `SafeBuffer` because
    // the caller already holds an `&[u8]` plus a `&Schema`, and the
    // executor uses the unchecked accessors for hot-path reads.
    SafeBuffer::new_with_options(buf, schema, &opts)
        .map(|_| ())
        .map_err(|e| match e {
            FlatbufferError::VerificationError(inner) => VerifyError::Invalid(inner.to_string()),
            // We pre-checked `root_table.is_some()` above, so the only
            // remaining cause of `InvalidSchema` from the upstream
            // verifier is a malformed root offset in the user buffer.
            FlatbufferError::InvalidSchema => {
                VerifyError::Invalid("buffer's root offset is malformed".to_owned())
            }
            // Other variants of `FlatbufferError` (e.g.,
            // `TryFromIntError`) shouldn't surface from a vanilla
            // verify call, but if they do we forward the message
            // rather than panicking — the executor will already have
            // its own panic-catching boundary at the `#[pg_extern]`
            // layer.
            other => VerifyError::Invalid(other.to_string()),
        })
}

// ---------------------------------------------------------------------------
// Tests (pure Rust — no `cargo pgrx test` needed)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use flatbuffers::FlatBufferBuilder;
    use flatbuffers_reflection::reflection::{
        root_as_schema, Object, ObjectArgs, Schema as RSchema, SchemaArgs,
    };

    /// Build a minimal but verifier-clean reflection `Schema` with no
    /// `root_table`. Mirrors the fixture in `schema_cache::tests`.
    fn empty_bfbs() -> Vec<u8> {
        let mut fbb = FlatBufferBuilder::new();
        let objects = fbb.create_vector::<flatbuffers::ForwardsUOffset<Object>>(&[]);
        let enums = fbb.create_vector::<flatbuffers::ForwardsUOffset<
            flatbuffers_reflection::reflection::Enum,
        >>(&[]);
        let schema = RSchema::create(
            &mut fbb,
            &SchemaArgs {
                objects: Some(objects),
                enums: Some(enums),
                ..Default::default()
            },
        );
        fbb.finish(schema, None);
        fbb.finished_data().to_vec()
    }

    /// Build a reflection `Schema` whose `root_table` is a single empty
    /// table named `"Empty"`. Lets us exercise the happy path of
    /// [`verify`] without depending on `flatc`.
    fn empty_table_bfbs() -> Vec<u8> {
        let mut fbb = FlatBufferBuilder::new();

        let name = fbb.create_string("Empty");
        let fields = fbb.create_vector::<flatbuffers::ForwardsUOffset<
            flatbuffers_reflection::reflection::Field,
        >>(&[]);
        let object = Object::create(
            &mut fbb,
            &ObjectArgs {
                name: Some(name),
                fields: Some(fields),
                is_struct: false,
                ..Default::default()
            },
        );

        // Schema requires a non-null `objects` vector. Build it
        // *after* the Object so the offsets resolve cleanly.
        let objects = fbb.create_vector(&[object]);
        let enums = fbb.create_vector::<flatbuffers::ForwardsUOffset<
            flatbuffers_reflection::reflection::Enum,
        >>(&[]);

        let schema = RSchema::create(
            &mut fbb,
            &SchemaArgs {
                objects: Some(objects),
                enums: Some(enums),
                root_table: Some(object),
                ..Default::default()
            },
        );
        fbb.finish(schema, None);
        fbb.finished_data().to_vec()
    }

    /// Build a minimal in-the-wild FlatBuffer whose root is an empty
    /// table (no fields set). Equivalent to a default-constructed
    /// `Empty` instance.
    fn empty_table_buffer() -> Vec<u8> {
        let mut fbb = FlatBufferBuilder::new();
        let off = fbb.start_table();
        let off = fbb.end_table(off);
        fbb.finish_minimal(off);
        fbb.finished_data().to_vec()
    }

    // -- error variants for non-buffer-shaped inputs --

    #[test]
    fn rejects_empty_buffer() {
        let bfbs = empty_table_bfbs();
        let schema = root_as_schema(&bfbs).unwrap();
        let err = verify(&[], &schema, &Bounds::default()).unwrap_err();
        assert!(matches!(err, VerifyError::Empty));
    }

    #[test]
    fn rejects_buffer_smaller_than_root_offset() {
        let bfbs = empty_table_bfbs();
        let schema = root_as_schema(&bfbs).unwrap();
        let err = verify(&[0u8; 3], &schema, &Bounds::default()).unwrap_err();
        assert!(matches!(err, VerifyError::TooSmall { len: 3 }));
    }

    #[test]
    fn rejects_schema_without_root_table() {
        let bfbs = empty_bfbs();
        let schema = root_as_schema(&bfbs).unwrap();
        let buf = empty_table_buffer();
        let err = verify(&buf, &schema, &Bounds::default()).unwrap_err();
        assert!(matches!(err, VerifyError::NoRootTable));
    }

    // -- structural failures inside the buffer --

    #[test]
    fn rejects_garbage_buffer_with_valid_schema() {
        let bfbs = empty_table_bfbs();
        let schema = root_as_schema(&bfbs).unwrap();
        // 4 bytes interpreted as a root offset of 0x03020100 — way
        // off the end of the buffer.
        let err = verify(&[0u8, 1, 2, 3], &schema, &Bounds::default()).unwrap_err();
        match err {
            VerifyError::Invalid(_) => {}
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    // -- happy path --

    #[test]
    fn accepts_well_formed_empty_table() {
        let bfbs = empty_table_bfbs();
        let schema = root_as_schema(&bfbs).unwrap();
        let buf = empty_table_buffer();
        verify(&buf, &schema, &Bounds::default())
            .expect("an empty table must verify against a single-empty-table schema");
    }

    // -- bounds enforcement --

    #[test]
    fn rejects_when_max_tables_is_zero() {
        // With `max_tables = 0` even a single root table can't be
        // visited. This proves the bounds are actually plumbed
        // through to the upstream verifier.
        let bfbs = empty_table_bfbs();
        let schema = root_as_schema(&bfbs).unwrap();
        let buf = empty_table_buffer();
        let bounds = Bounds {
            max_tables: 0,
            ..Bounds::default()
        };
        let err = verify(&buf, &schema, &bounds).unwrap_err();
        assert!(
            matches!(&err, VerifyError::Invalid(msg) if msg.to_lowercase().contains("table")),
            "unexpected error variant or message: {err:?}",
        );
    }

    #[test]
    fn rejects_when_max_depth_is_zero() {
        // Even a single nested table is one level of depth. With
        // `max_depth = 0` the verifier must reject the root.
        let bfbs = empty_table_bfbs();
        let schema = root_as_schema(&bfbs).unwrap();
        let buf = empty_table_buffer();
        let bounds = Bounds {
            max_depth: 0,
            ..Bounds::default()
        };
        let err = verify(&buf, &schema, &bounds).unwrap_err();
        match err {
            VerifyError::Invalid(_) => {}
            other => panic!("expected Invalid (depth bound), got {other:?}"),
        }
    }

    #[test]
    fn defaults_match_design_section_10() {
        // Pin the defaults; if someone changes them in code without
        // updating §10 of the design, this test fires.
        let b = Bounds::default();
        assert_eq!(b.max_depth, 64);
        assert_eq!(b.max_tables, 1_000_000);
        assert_eq!(b.max_apparent_size, 64 * 1024 * 1024);
    }

    // -- helper / classifier --

    #[test]
    fn is_bound_exceedance_classifies_messages() {
        // Construct error variants directly — robust to upstream
        // diagnostic phrasing changes (the "stable substrings" we
        // match are documented in `is_bound_exceedance`).
        assert!(VerifyError::Invalid("depth 65 exceeds limit 64".to_owned()).is_bound_exceedance());
        assert!(
            VerifyError::Invalid("too many tables: limit 100 exceeded".to_owned())
                .is_bound_exceedance()
        );
        assert!(
            VerifyError::Invalid("apparent size 99 over budget".to_owned()).is_bound_exceedance()
        );

        // Negative cases.
        assert!(!VerifyError::Empty.is_bound_exceedance());
        assert!(!VerifyError::TooSmall { len: 1 }.is_bound_exceedance());
        assert!(!VerifyError::NoRootTable.is_bound_exceedance());
        assert!(
            !VerifyError::Invalid("Range [0, 4) is out of bounds".to_owned()).is_bound_exceedance()
        );
    }
}
