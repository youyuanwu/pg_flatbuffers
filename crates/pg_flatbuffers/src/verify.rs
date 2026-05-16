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
//! ## Schema-feature rejection
//!
//! Before any buffer bytes are inspected, [`verify`] scans the
//! reflection schema and rejects v0.1-unsupported features via
//! [`reject_unsupported_schema_features`]. Today the only such
//! feature is `BaseType::Vector64` (64-bit-offset vectors), which
//! would otherwise reach the executor's 32-bit `flatbuffers::Vector`
//! accessors and silently truncate addresses. Rejection is
//! per-schema, not per-touched-field: even a query that targets a
//! Vector64-sibling field fails, because the buffer is under-validated
//! as a whole.
//!
//! ## What is *deferred*
//!
//! - **Content-hashed result cache** (design §10 "Verifier result
//!   caching"). The cache is per-statement-scoped and dropped on
//!   memory-context reset, which only makes sense once the executor
//!   exists to call us many times per statement. Adding `blake3`
//!   speculatively would be cargo-cult dependency growth.

use flatbuffers::VerifierOptions;
use flatbuffers_reflection::reflection::{BaseType, Schema};
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

    /// The schema uses a FlatBuffers feature that v0.1 does not
    /// support. Detected *before* the upstream verifier runs so that
    /// (a) operators get a clear message naming the feature rather
    /// than a deferred query-time `UnsupportedType`, and (b) the
    /// soundness boundary stays tight: the executor's unchecked
    /// accessors only get to see schemas whose features we know we
    /// can read safely. See [`reject_unsupported_schema_features`]
    /// for the per-feature list.
    #[error(
        "schema uses unsupported FlatBuffers feature {feature}: \
         table {table:?} field {field:?} (v0.1 does not support this)"
    )]
    UnsupportedSchemaFeature {
        feature: &'static str,
        table: String,
        field: String,
    },
}

impl VerifyError {
    /// True if the failure is a *bound exceedance* (depth, tables,
    /// apparent size) rather than a structural invalidity. Consumed
    /// by [`crate::functions::should_error_on`] so that the
    /// `pg_flatbuffers.strict = off` lenient path still raises
    /// ERROR on bound failures — see design §10 ("strict does not
    /// relax bounds").
    ///
    /// The upstream verifier exposes bound failures only through the
    /// error message (no machine-readable code), so this is a string
    /// match on stable substrings of `flatbuffers::InvalidFlatbuffer`'s
    /// `Display` output. Conservative: callers can treat `false` as
    /// "not provably a bound failure" without misclassifying anything
    /// as a structural error.
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

    /// True if the failure is the up-front schema-level rejection
    /// added by [`reject_unsupported_schema_features`]. Consumed by
    /// [`crate::functions::should_error_on`] (which always ERRORs on
    /// these, regardless of `pg_flatbuffers.strict`, because a
    /// schema-feature mismatch is a *config* problem, not a
    /// buffer-content problem) and by [`crate::functions::flatbuffers_verify`]
    /// (which surfaces it as ERROR even though its normal contract
    /// is to swallow verifier failures into `false`).
    pub fn is_schema_feature_rejection(&self) -> bool {
        matches!(self, VerifyError::UnsupportedSchemaFeature { .. })
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

    // Reject schemas containing v0.1-unsupported features (Vector64
    // today) *before* handing the buffer to the upstream verifier.
    // The upstream verifier doesn't yet validate Vector64 payloads
    // (and even if it did, our executor's `flatbuffers::Vector<T>`
    // accessors only handle 32-bit offsets), so accepting a Vector64
    // schema here would risk reading from an under-validated region
    // when a query touches a *sibling* field. Fail loud and early.
    reject_unsupported_schema_features(schema)?;

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

/// Walk every object's fields in `schema` and reject any field whose
/// type uses a FlatBuffers feature that v0.1 doesn't support.
///
/// Today the list is exactly one entry: `BaseType::Vector64`, a
/// vector type that addresses elements via 64-bit offsets.
/// `flatbuffers::Vector<T>` (used by our executor) only handles
/// 32-bit offsets, so any access would silently truncate addresses.
/// Rejection is at *schema* granularity, not field-touched
/// granularity — see the call site in [`verify`] for rationale.
///
/// Pure scan; no allocation beyond the `String` payloads of the
/// error variant on rejection. O(total fields in schema), but every
/// well-formed schema has a bounded number of objects per design
/// §10 so this is a single linear pass over reflection metadata.
fn reject_unsupported_schema_features(schema: &Schema<'_>) -> Result<(), VerifyError> {
    for object in schema.objects() {
        for field in object.fields() {
            let t = field.type_();
            if t.base_type() == BaseType::Vector64 {
                return Err(VerifyError::UnsupportedSchemaFeature {
                    feature: "Vector64",
                    table: object.name().to_owned(),
                    field: field.name().to_owned(),
                });
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests (pure Rust — no `cargo pgrx test` needed)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use flatbuffers::FlatBufferBuilder;
    use flatbuffers_reflection::reflection::{
        root_as_schema, BaseType, Field, FieldArgs, Object, ObjectArgs, Schema as RSchema,
        SchemaArgs, Type, TypeArgs,
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

    // -- Vector64 / unsupported-schema-feature rejection --

    /// Build a reflection `Schema` whose root table `V64` has one
    /// field `items: [int64-offset]`. Mirrors the Vector64 case the
    /// upstream verifier doesn't fully cover. We're not building a
    /// matching *buffer* — the schema scan happens before any buffer
    /// inspection, so any non-empty ≥4-byte buffer triggers the same
    /// rejection.
    fn vector64_schema_bfbs() -> Vec<u8> {
        let mut fbb = FlatBufferBuilder::new();

        // Element type: int (BaseType::Int). The element_base_type
        // of the Vector64 lives in `element`, not in a nested Type.
        let items_type = Type::create(
            &mut fbb,
            &TypeArgs {
                base_type: BaseType::Vector64,
                element: BaseType::Int,
                ..Default::default()
            },
        );
        let items_name = fbb.create_string("items");
        let items_field = Field::create(
            &mut fbb,
            &FieldArgs {
                name: Some(items_name),
                type_: Some(items_type),
                id: 0,
                offset: 4,
                ..Default::default()
            },
        );
        let fields = fbb.create_vector(&[items_field]);
        let table_name = fbb.create_string("V64");
        let object = Object::create(
            &mut fbb,
            &ObjectArgs {
                name: Some(table_name),
                fields: Some(fields),
                is_struct: false,
                ..Default::default()
            },
        );
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

    #[test]
    fn rejects_vector64_schema_before_touching_buffer() {
        let bfbs = vector64_schema_bfbs();
        let schema = root_as_schema(&bfbs).unwrap();
        // Pass garbage bytes: the schema-feature scan fires first,
        // so we never reach the upstream verifier and never observe
        // an `Invalid` from the garbage.
        let err = verify(&[0xDE, 0xAD, 0xBE, 0xEF], &schema, &Bounds::default()).unwrap_err();
        match err {
            VerifyError::UnsupportedSchemaFeature {
                feature,
                table,
                field,
            } => {
                assert_eq!(feature, "Vector64");
                assert_eq!(table, "V64");
                assert_eq!(field, "items");
            }
            other => panic!("expected UnsupportedSchemaFeature, got {other:?}"),
        }
    }

    #[test]
    fn vector64_rejection_message_names_the_feature() {
        let bfbs = vector64_schema_bfbs();
        let schema = root_as_schema(&bfbs).unwrap();
        let err = verify(&[0u8; 8], &schema, &Bounds::default()).unwrap_err();
        let msg = err.to_string();
        // Operators should see all three: the feature name, the
        // table, and the field. Substring checks so we're robust to
        // any future cosmetic tweaks to the `#[error(...)]` template.
        assert!(msg.contains("Vector64"), "missing feature name: {msg}");
        assert!(msg.contains("V64"), "missing table name: {msg}");
        assert!(msg.contains("items"), "missing field name: {msg}");
    }

    #[test]
    fn is_schema_feature_rejection_classifies_variants() {
        let v64 = VerifyError::UnsupportedSchemaFeature {
            feature: "Vector64",
            table: "T".to_owned(),
            field: "f".to_owned(),
        };
        assert!(v64.is_schema_feature_rejection());
        // Crucially: it is *not* a bound exceedance, so the policy
        // in `functions::should_error_on` has to consult both
        // classifiers (a regression here would let `strict = off`
        // silently swallow Vector64 schemas).
        assert!(!v64.is_bound_exceedance());

        // Negative cases.
        assert!(!VerifyError::Empty.is_schema_feature_rejection());
        assert!(!VerifyError::TooSmall { len: 1 }.is_schema_feature_rejection());
        assert!(!VerifyError::NoRootTable.is_schema_feature_rejection());
        assert!(!VerifyError::Invalid("anything".to_owned()).is_schema_feature_rejection());
        assert!(
            !VerifyError::Invalid("depth 65 exceeds limit 64".to_owned())
                .is_schema_feature_rejection()
        );
    }
}
