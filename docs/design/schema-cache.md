# Schema cache

Schemas change rarely and are read by every query, so caching the parsed
`reflection::Schema` is essential. Implementation:
[`src/schema_cache.rs`](../../crates/pg_flatbuffers/src/schema_cache.rs).

## Design constraints

1. **Cluster-wide invalidation must be reliable.** A long-lived connection
   pool with N backends would otherwise serve stale schemas for an unbounded
   window after a schema rotation.
2. **Per-statement re-lookup must be cheap** so single-statement transactions
   (PostgREST, ORMs, pgBouncer in `transaction` mode) do not re-parse the
   schema per query.

## Architecture

- **Per-backend LRU keyed by `schema_name`** holding the parsed `CachedSchema`
  (see [`schema_cache.rs::CachedSchema`](../../crates/pg_flatbuffers/src/schema_cache.rs)).
  Cache size is bounded by `pg_flatbuffers.schema_cache_mb` (default 16 MiB;
  `SIGHUP`); eviction is LRU on insert. Reducing the GUC takes effect on the
  next backend start.

- **Cluster-wide invalidation via `CacheRegisterRelcacheCallback`** on
  `flatbuffers_schemas`. When any backend commits a write to the catalog, the
  resulting relcache invalidation message is delivered to every other backend,
  which drops the entries it holds. The callback registration happens in
  [`schema_cache.rs::init`](../../crates/pg_flatbuffers/src/schema_cache.rs),
  invoked from `_PG_init` in [`src/lib.rs`](../../crates/pg_flatbuffers/src/lib.rs).

  Plain DML on user tables does not auto-emit relcache invalidation, so
  [`sql/catalog.sql`](../../crates/pg_flatbuffers/sql/catalog.sql) installs a
  STATEMENT-level AFTER trigger
  ([`flatbuffers_schemas_invalidate_trigger`](../../crates/pg_flatbuffers/src/catalog.rs))
  that explicitly fires the invalidation.

- **Lazy field-index construction.** A cached entry's `name → Field` hash for
  a given `Object` is built on first access rather than at cache load, so
  registering a 500-table schema does not cost the first query that touches
  only one of those tables.

The LRU is a Rust-side `parking_lot::Mutex<lru::LruCache<String,
Arc<CachedSchema>>>` initialised in `_PG_init`. It is per-backend (each
Postgres backend is single-threaded with respect to its own caches; we never
share across backends).

## MVCC contract for `flatbuffers_schemas`

A statement reads the schema row visible to its snapshot at first lookup;
that view is pinned for the rest of the statement. The next statement
re-checks the relcache invalidation queue and, if the row has been replaced,
re-parses. The catalog is therefore safe to `UPDATE` from another session
while a long scan is in progress (the scan keeps using the schema it started
with), and the next statement in any backend sees the new schema once the
writing transaction commits.

## Statement-scoped memoization (deferred)

The original design called for a per-statement `HashMap<String, Arc<CachedSchema>>`
in the per-statement memory context so a query that resolves the same schema
name many times (e.g., `LATERAL flatbuffers_query_multi(...)`) hits an
`Arc::clone` rather than the LRU. v0.1 ships without it — the per-backend
LRU lookup is already cheap enough in practice. Tracked in
[roadmap.md](roadmap.md) if profiling shows a hot spot.
