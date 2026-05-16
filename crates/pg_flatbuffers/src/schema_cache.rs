//! Per-backend schema cache for `flatbuffers_schemas`.
//!
//! See `docs/design/schema-cache.md`. The cache is the integration point between
//! the catalog (registered, verified `.bfbs` blobs) and the executor
//! (zero-copy `reflection::Schema` views needed per query).
//!
//! ## Coherence model (summary)
//!
//! - **Per-backend.** Each Postgres backend is single-threaded with
//!   respect to its own `static` state, so a `Mutex<LruCache<...>>` is
//!   sufficient. We never share entries across backends.
//! - **Cross-backend invalidation** is delivered by Postgres' shared
//!   invalidation queue. We register a `RelcacheCallback` against
//!   `flatbuffers_schemas`; when *any* backend commits a write to the
//!   catalog, every other backend's next message-processing point
//!   (typically the start of the next statement) drains the queue and
//!   our callback fires. The callback drops the cache entirely.
//!   Per-name invalidation is deferred (see §6, "Lazy field-index
//!   construction") because a typical deployment has very few
//!   registered schemas and writes are rare.
//! - **Verification.** Bytes are re-verified on cache miss before the
//!   `Vec<u8>` is wrapped in an `Arc<CachedSchema>`. This keeps the
//!   safety boundary at cache-insertion time: callers may treat the
//!   bytes as a verified FlatBuffer for the lifetime of the `Arc`.
//!
//! ## Out of scope for v0.1 (TODOs)
//!
//! - Statement-scoped memoization (§6 "current view"). Not needed
//!   until the executor lands.
//! - Lazy per-`Object` `name -> Field` hashes (§6 "Lazy field-index").
//!   Belongs with the executor slice.
//! - GUC-driven cache size (`pg_flatbuffers.schema_cache_mb`, §10).
//!   For now the LRU has a fixed capacity below.
//! - Per-name invalidation. The relcache callback drops everything.

use lru::LruCache;
use parking_lot::Mutex;
use pgrx::pg_sys;
use pgrx::prelude::*;
use std::num::NonZeroUsize;
use std::sync::{Arc, OnceLock};

/// LRU capacity until the cache-size GUC lands (see TODO above).
const DEFAULT_CACHE_CAPACITY: usize = 128;

/// A verified, immutable FlatBuffers reflection schema with the
/// metadata the executor needs to start a query.
///
/// The bytes are owned (`Vec<u8>`); callers reconstruct the
/// zero-copy `reflection::Schema<'_>` view from `&self.bfbs` on demand
/// — re-derivation is just a root-table offset read.
pub struct CachedSchema {
    /// Verified `.bfbs` bytes. Treat as immutable for the entry's
    /// lifetime; the `Arc<CachedSchema>` aliasing makes this enforced.
    pub bfbs: Vec<u8>,
    /// Fully-qualified name of the schema's root table, as registered
    /// in `flatbuffers_schemas.root_table`.
    pub root_table: String,
    /// Optional 4-byte file_identifier registered alongside the schema;
    /// used by JSON conversion sanity checks (§8).
    //
    // TODO(json-slice): consumed by `to_json` / `from_json`. Marked
    // `allow(dead_code)` until that slice lands.
    #[allow(dead_code)]
    pub file_identifier: Option<String>,
}

impl CachedSchema {
    /// Re-derive the zero-copy reflection view from the owned bytes.
    /// Cheap (single offset read); no allocation.
    pub fn schema(&self) -> flatbuffers_reflection::reflection::Schema<'_> {
        // SAFETY: `self.bfbs` was verified before insertion into the
        // cache (see `lookup_schema` / `insert_verified`), so the
        // unchecked `root` is sound. We use the unchecked variant to
        // avoid re-running the verifier on the hot path.
        unsafe { flatbuffers_reflection::reflection::root_as_schema_unchecked(&self.bfbs) }
    }
}

// ---------------------------------------------------------------------------
// Backend-global state
// ---------------------------------------------------------------------------

/// Per-backend cache. Lazily initialized on first use OR on
/// `_PG_init` (both are valid: the `OnceLock` ensures the initializer
/// runs exactly once per backend).
static CACHE: OnceLock<Mutex<LruCache<String, Arc<CachedSchema>>>> = OnceLock::new();

/// OID of `flatbuffers_schemas` resolved on first cache miss. Until
/// resolved, the relcache callback conservatively flushes on every
/// invalidation it sees.
static CATALOG_OID: OnceLock<pg_sys::Oid> = OnceLock::new();

fn cache() -> &'static Mutex<LruCache<String, Arc<CachedSchema>>> {
    CACHE.get_or_init(|| {
        Mutex::new(LruCache::new(
            NonZeroUsize::new(DEFAULT_CACHE_CAPACITY).expect("nonzero capacity"),
        ))
    })
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Resolve a registered schema by name. On a hit, returns the existing
/// `Arc` (cheap clone). On a miss, runs SPI against
/// `flatbuffers_schemas`, re-verifies the bytes, inserts, and returns.
///
/// Raises `ERROR` if the name is unknown or if the stored bytes fail
/// verification (which would indicate catalog corruption — the CHECK
/// constraint should make this impossible at INSERT time).
pub fn lookup_schema(name: &str) -> Arc<CachedSchema> {
    if let Some(hit) = cache().lock().get(name).cloned() {
        return hit;
    }
    load_and_insert(name)
}

/// Drop one entry by name. No-op if absent. Currently only used by
/// tests; the relcache callback uses [`invalidate_all`] instead because
/// it does not know the schema *name* from the catalog `Oid`.
#[cfg_attr(
    not(test),
    allow(
        dead_code,
        reason = "only consumed from the in-module test suite; kept on the public API for future direct-invalidation call sites"
    )
)]
pub fn invalidate(name: &str) {
    cache().lock().pop(name);
}

/// Drop every cached schema. Invoked by the relcache callback whenever
/// `flatbuffers_schemas` (or the world) is invalidated.
pub fn invalidate_all() {
    // Guard against being called before `_PG_init` ran (e.g., during
    // very early backend startup): only clear if already initialized.
    if let Some(c) = CACHE.get() {
        c.lock().clear();
    }
}

// ---------------------------------------------------------------------------
// Internal: load + verify
// ---------------------------------------------------------------------------

fn load_and_insert(name: &str) -> Arc<CachedSchema> {
    // Resolve catalog OID lazily on first use so the cache works even
    // before any explicit OID-resolution call. We do this *before* the
    // SPI lookup so that a callback racing with a write sees the
    // resolved OID and can target its invalidation precisely.
    let _ = catalog_oid();

    // The three columns we lift out of `flatbuffers_schemas` per row:
    // `(bfbs, root_table, file_identifier)`. Aliased to keep the
    // closure return type readable (and silence `clippy::type_complexity`).
    type SchemaRow = (Vec<u8>, String, Option<String>);

    // SPI: parameterized lookup. `name` is bound as text — SQL
    // injection-safe. Use `connect_mut` directly so we can distinguish
    // "no rows" (schema not registered) from a real SPI error;
    // `Spi::get_three_with_args` collapses the two and we want a clean
    // user-facing message for the common case.
    let row = Spi::connect_mut(|client| -> spi::Result<Option<SchemaRow>> {
        let table = client.update(
            "SELECT bfbs, root_table, file_identifier \
             FROM flatbuffers_schemas WHERE name = $1",
            Some(1),
            &[name.to_string().into()],
        )?;
        if table.is_empty() {
            return Ok(None);
        }
        let row = table.first();
        let bfbs = row
            .get::<&[u8]>(1)?
            .expect("flatbuffers_schemas.bfbs is NOT NULL")
            .to_vec();
        let root_table = row
            .get::<String>(2)?
            .expect("flatbuffers_schemas.root_table is NOT NULL");
        let file_identifier = row.get::<String>(3)?;
        Ok(Some((bfbs, root_table, file_identifier)))
    })
    .unwrap_or_else(|e| error!("SPI failure looking up flatbuffers schema {name:?}: {e}"));

    let (bfbs, root_table, file_identifier) =
        row.unwrap_or_else(|| error!("flatbuffers schema {name:?} is not registered"));

    // Re-verify on cache load. The catalog CHECK constraint means a
    // properly-inserted row already passed verification, but a corrupt
    // table or a manual catalog edit would otherwise let unverified
    // bytes through to the executor.
    if let Err(e) = flatbuffers_reflection::reflection::root_as_schema(&bfbs) {
        error!("stored schema {name:?} failed FlatBuffers verification: {e}");
    }

    let entry = Arc::new(CachedSchema {
        bfbs,
        root_table,
        file_identifier,
    });

    cache().lock().put(name.to_string(), Arc::clone(&entry));
    entry
}

/// Resolve the OID of `flatbuffers_schemas`, caching the result for
/// the lifetime of the backend. Uses `regclass::oid` so the search
/// path determines which schema's catalog wins — fine for v0.1, where
/// only one extension install per database is supported.
fn catalog_oid() -> pg_sys::Oid {
    *CATALOG_OID.get_or_init(|| {
        Spi::get_one::<pg_sys::Oid>("SELECT 'flatbuffers_schemas'::regclass::oid")
            .unwrap_or_else(|e| error!("could not resolve flatbuffers_schemas OID via SPI: {e}"))
            .unwrap_or_else(|| error!("could not resolve flatbuffers_schemas OID: NULL result"))
    })
}

// ---------------------------------------------------------------------------
// Initialization / sinval registration
// ---------------------------------------------------------------------------

/// Register the relcache invalidation callback. Called from
/// `_PG_init`.
pub(crate) fn init() {
    // Touch the LRU so it's allocated by the time the callback can fire.
    let _ = cache();

    // SAFETY: `CacheRegisterRelcacheCallback` is a stable Postgres C
    // API; the callback function pointer has the matching
    // `extern "C-unwind" fn(Datum, Oid)` signature.
    unsafe {
        pg_sys::CacheRegisterRelcacheCallback(
            Some(relcache_callback),
            // Datum::null() is fine — we don't carry user data; all
            // relevant state lives in the static OnceLocks.
            pg_sys::Datum::null(),
        );
    }
}

/// Postgres invokes this whenever a relcache entry is invalidated
/// anywhere in the cluster (after the responsible transaction commits
/// and the receiving backend processes its sinval queue).
///
/// `relid == InvalidOid` is Postgres' "the entire relcache was
/// invalidated" signal; we treat that as flush-all.
unsafe extern "C-unwind" fn relcache_callback(_arg: pg_sys::Datum, relid: pg_sys::Oid) {
    let our_oid = CATALOG_OID.get().copied();
    let invalid = pg_sys::Oid::INVALID;
    let matches = relid == invalid || matches!(our_oid, Some(o) if o == relid);
    if matches {
        invalidate_all();
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(any(test, feature = "pg_test"))]
#[pg_schema]
mod tests {
    use super::*;

    /// Build a minimal but verifier-clean `reflection::Schema`
    /// flatbuffer (no objects, no enums, no root table). Used as a
    /// fixture so tests can exercise the catalog/cache plumbing
    /// without depending on `flatc`.
    fn empty_bfbs() -> Vec<u8> {
        use flatbuffers::FlatBufferBuilder;
        use flatbuffers_reflection::reflection::{Schema, SchemaArgs};

        let mut fbb = FlatBufferBuilder::new();
        let objects = fbb.create_vector::<flatbuffers::ForwardsUOffset<
            flatbuffers_reflection::reflection::Object,
        >>(&[]);
        let enums = fbb.create_vector::<flatbuffers::ForwardsUOffset<
            flatbuffers_reflection::reflection::Enum,
        >>(&[]);
        let schema = Schema::create(
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

    /// Insert the empty fixture under `name`, bypassing the role check
    /// (test runs as superuser).
    fn register_empty(name: &str, root_table: &str) {
        let bfbs = empty_bfbs();
        Spi::run_with_args(
            "INSERT INTO flatbuffers_schemas (name, bfbs, root_table) \
             VALUES ($1, $2, $3)",
            &[
                name.to_string().into(),
                bfbs.into(),
                root_table.to_string().into(),
            ],
        )
        .expect("SPI insert");
    }

    #[pg_test]
    fn empty_bfbs_roundtrip_through_check_constraint() {
        // The fixture must satisfy the CHECK constraint (else the rest
        // of the cache tests are vacuous).
        register_empty("rt_empty", "Empty");
    }

    #[pg_test]
    fn lookup_misses_then_hits() {
        register_empty("c_hit", "Empty");

        // Force a known-empty cache for this backend's view of `c_hit`.
        // (Other test runs in this backend may have populated it.)
        super::invalidate("c_hit");

        let a = super::lookup_schema("c_hit");
        let b = super::lookup_schema("c_hit");

        assert_eq!(a.root_table, "Empty");
        assert_eq!(b.root_table, "Empty");
        // Second lookup must hit the cache and return the same Arc.
        assert!(Arc::ptr_eq(&a, &b), "expected cache hit on second lookup");
    }

    #[pg_test(error = "flatbuffers schema \"nope\" is not registered")]
    fn lookup_missing_schema_errors() {
        let _ = super::lookup_schema("nope");
    }

    #[pg_test]
    fn delete_invalidates_via_relcache_callback() {
        register_empty("c_inv", "Empty");
        let _ = super::lookup_schema("c_inv"); // populate

        // Delete the row in this backend. The relcache callback fires
        // on commit, but pgrx-tests runs each test in a single
        // transaction and rolls back at end — so to *observe* the
        // callback within one test we use a savepoint-free
        // self-trigger: AcceptInvalidationMessages is called by
        // executor between statements. After the DELETE statement
        // completes, the next SPI call will see the invalidated cache.
        Spi::run("DELETE FROM flatbuffers_schemas WHERE name = 'c_inv'").expect("SPI delete");

        // After invalidation, the entry should be gone; another
        // lookup must miss and ERROR (row no longer present).
        match Spi::run("SELECT * FROM flatbuffers_schemas WHERE name = 'c_inv'") {
            Ok(()) => (),
            Err(e) => panic!("unexpected SPI error: {e}"),
        }
        // We can't directly call lookup_schema here without ERRORing
        // out of the test; instead assert the cache no longer holds it.
        assert!(
            super::CACHE
                .get()
                .expect("cache initialized")
                .lock()
                .peek("c_inv")
                .is_none(),
            "cache entry should have been invalidated by relcache callback"
        );
    }

    #[pg_test]
    fn schema_view_is_re_derivable() {
        register_empty("c_view", "Empty");
        super::invalidate("c_view");
        let entry = super::lookup_schema("c_view");
        let schema = entry.schema();
        // No objects, no enums — the empty fixture round-trip.
        assert_eq!(schema.objects().len(), 0);
        assert_eq!(schema.enums().len(), 0);
    }
}
