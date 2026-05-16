# Roadmap and open questions

## v0.1 — current

- Schema catalog ([catalog-and-schemas.md](catalog-and-schemas.md))
- `flatbuffers_query{,_array,_multi}` ([sql-surface.md](sql-surface.md),
  [query-execution.md](query-execution.md))
- JSON round-trip ([json-conversion.md](json-conversion.md))
- Verifier integration ([safety.md](safety.md))
- PG 18 source builds ([compatibility-build-test.md](compatibility-build-test.md))

## v0.2 — planned

- **PG 17** added to the CI matrix and binary releases. Should be a feature
  flag flip (the design carries no PG-18-only assumptions).
- **`IMMUTABLE` field-id-only accessors** (`flatbuffers_get_*`) enabling
  functional indexes (see [safety.md → indexability](safety.md#indexability)).
- **Binary release packaging** for at least Debian bookworm and Ubuntu 24.04
  via a pinned Dockerfile used by CI.
- **Vector-of-union schema support.** Requires either vendoring the upstream
  `flatbuffers-reflection` verifier or upstreaming a fix and waiting; for now
  rejected at registration time
  ([`verify.rs`](../../crates/pg_flatbuffers/src/verify.rs)).
- **`Vector64` support.** Same upstream gap as vector-of-union.
- **Statement-scoped schema memoization.** Per-statement
  `HashMap<String, Arc<CachedSchema>>` in the per-statement memory context
  (see [schema-cache.md](schema-cache.md) — currently every lookup hits the
  per-backend LRU).
- **Verifier result caching.** Cache key per
  `(buffer content, root_type)` so a buffer accessed by many path
  expressions within a single SQL function call only pays the verifier cost
  once (see [safety.md](safety.md)).
- **Additional GUCs** documented but not yet registered in
  [`src/guc.rs`](../../crates/pg_flatbuffers/src/guc.rs):
  `max_build_depth`, `max_query_length`, `max_path_depth`,
  `schema_cache_mb` (`SIGHUP`), `identifier_mismatch`. Parser bounds are
  currently hard-coded inside
  [`query/parser.rs::parse_with_bounds`](../../crates/pg_flatbuffers/src/query/parser.rs).
- **`(key)`-sorted vector verification.** Have the verifier reject unsorted
  `(key)` vectors so `key_lookup_strict = on` cannot silently miss against
  contract-violating writers (see [safety.md](safety.md)).

## v0.3 — exploratory

- **Typed accessor families** (`flatbuffers_query_int`, `_double`, `_bool`)
  — also unlocks indexable extraction in combination with the IMMUTABLE
  field-id-only accessors. Defer until usage signal exists.
- **Richer error messages with path location** — annotate every executor
  error with the AST step that surfaced it for friendlier SQL diagnostics.

## Later / aspirational

- Exploring partial deserialization for very large vectors.
- Optional in-place mutation API.
- Native `composite type` returns from `flatbuffers_query` so callers can
  destructure without going through `text`.
- `SUPPORT` functions (`ROWS`, `COST` annotations) once we have
  measurement. v0.1 ships static `ROWS=10` and a payload-size-proportional
  `COST` annotation.

## Open questions

- Whether to ship a `flatbuffers_compose(table_name, jsonb) -> bytea`
  helper distinct from `from_json` for the common case of building from a
  Postgres-native `jsonb` accumulated by other operators.
- Generative testing harness shape — `proptest` strategy that builds
  arbitrary buffers from a simple schema for `from_json(to_json(b)) == b`
  invariants. Whether to also add a fuzz target.
- `(hex)` attribute semantics on input — current implementation accepts both
  upper and lower case (be liberal); should we restrict to lowercase for
  strict flatc parity?
