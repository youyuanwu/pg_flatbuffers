# Safety, verifier bounds, GUCs, indexability

## Threat model

FlatBuffers' minimal validation is famously a footgun: a buffer crafted by an
attacker can produce out-of-bounds reads if read with the unchecked APIs.
The extension always goes through the verifier.

Implementation: [`src/verify.rs`](../../crates/pg_flatbuffers/src/verify.rs).
Verifier bounds are exposed as GUCs registered in
[`src/guc.rs`](../../crates/pg_flatbuffers/src/guc.rs); see the
[GUCs](#gucs) table below.

## Verifier

[`verify::verify`](../../crates/pg_flatbuffers/src/verify.rs) calls
`flatbuffers::root_with_opts` (and the reflection-based equivalent) with
`VerifierOptions` materialised from the current bounds. The same verifier with
the same bounds is applied at INSERT/UPDATE time on `flatbuffers_schemas.bfbs`
via [`flatbuffers_validate_schema`](../../crates/pg_flatbuffers/src/catalog.rs),
so a malformed reflection blob cannot reach the cache.

[`verify::reject_unsupported_schema_features`](../../crates/pg_flatbuffers/src/verify.rs)
additionally rejects, at *schema* registration time:

- **Vector-of-union fields** — the upstream `flatbuffers-reflection` 0.1.0
  verifier has no `verify_vector` path for `BaseType::Vector` with
  `element() == BaseType::Union`; rather than let the upstream surface a
  cryptic `TypeNotSupported`, we name the offending table and field.
- **`Vector64` fields** — same upstream gap, same deterministic-error
  treatment.

`(key)`-sorted vectors: the verifier does **not** yet check sortedness; the
on-mode of [`pg_flatbuffers.key_lookup_strict`](#gucs) against a writer that
violated the contract is a silent miss (never a buffer over-read). The
escape hatch is the off mode (linear scan).

## Failure semantics

When the verifier rejects a payload, `query_*` functions raise `ERROR` by
default. Setting [`pg_flatbuffers.strict`](#gucs) `= off` in the session
substitutes `NULL` instead and continues the scan. JSON conversion functions
always raise `ERROR` regardless of `strict` (see
[json-conversion.md](json-conversion.md)).

### Degenerate-payload contract

Specific cases under default `strict = on`:

- Zero-length `bytea` → `NULL` (treated as "absent payload," not as
  "malformed"); under `strict = off` likewise `NULL`.
- `bytea` smaller than the FlatBuffers root-offset minimum (4 bytes) →
  `ERROR` (`strict = on`) or `NULL` (`strict = off`).
- `bytea` whose declared apparent size exceeds `max_apparent_size_mb` →
  `ERROR` always (this is a bound, not a parse failure; `strict` does not
  relax bounds).
- SQL `NULL` `bytea` argument → `NULL` result (standard SQL strict
  semantics; the executor is never invoked).

## Verifier result caching (planned)

The design calls for a verifier-result cache keyed on `(len, hash_of_head,
hash_of_tail)` so a buffer accessed by many path expressions within a single
SQL function call only pays the verifier cost once. **Not yet implemented in
v0.1** — every call to a public `#[pg_extern]` re-verifies; see
[roadmap.md](roadmap.md).

## Panic safety

All Rust panics are caught at the `#[pg_extern]` boundary by pgrx and turned
into Postgres `ERROR`s; panics never unwind across the FFI boundary. The
`schemas` table is `bytea`, so SQL injection is structurally impossible
against the schema; the *query string* is parsed by the path parser into a
typed AST before touching reflection.

## GUCs

GUC governance is part of the safety boundary: a bound that any session can
raise is not a bound. Each GUC is assigned a `GucContext` deliberately —
`SUSET` (superuser-only) for everything that protects the backend from
untrusted input, `USERSET` for knobs that affect only the calling session's
own resource use or diagnostics. Registered in
[`src/guc.rs`](../../crates/pg_flatbuffers/src/guc.rs).

| GUC | Default | `GucContext` | Rationale |
| --- | --- | --- | --- |
| `pg_flatbuffers.max_depth` | 64 | `SUSET` | Verifier DoS bound. |
| `pg_flatbuffers.max_tables` | 1_000_000 | `SUSET` | Verifier DoS bound. |
| `pg_flatbuffers.max_apparent_size_mb` | 64 | `SUSET` | Verifier DoS bound; also caps `from_json` output. |
| `pg_flatbuffers.max_build_depth` *(planned)* | 32 | `SUSET` | `from_json` JSON-nesting bound; v0.1 reuses `max_depth`. |
| `pg_flatbuffers.max_query_length` *(planned)* | 4096 | `SUSET` | Path-parser input bound. |
| `pg_flatbuffers.max_path_depth` *(planned)* | 256 | `SUSET` | Path-parser depth bound. |
| `pg_flatbuffers.schema_cache_mb` *(planned)* | 16 | `SIGHUP` | Per-backend LRU size. |
| `pg_flatbuffers.strict` | `on` | `USERSET` | `on`: verifier failure → ERROR. `off`: structural failures return NULL; bound exceedances still ERROR. |
| `pg_flatbuffers.key_lookup_strict` | `on` | `USERSET` | `on` (default) bisects `(key)` vectors. `off` falls back to linear scan that tolerates unsorted vectors. |
| `pg_flatbuffers.from_json_unknown` | `on` | `USERSET` | `on` (default) ERRORs on unknown JSON keys. `off` drops them silently (forward-compat). |
| `pg_flatbuffers.fill_scalar_defaults` | `on` | `USERSET` | `on` (default): absent scalar → schema default (FlatBuffers reader-API parity). `off`: absent → SQL `NULL` (presence-aware). |
| `pg_flatbuffers.identifier_mismatch` *(planned)* | `warning` | `USERSET` | `file_identifier` mismatch verbosity (`error \| warning \| silent`). |

All `SUSET` GUCs require superuser to change, so an unprivileged caller
cannot lift a bound. The `USERSET` GUCs control opt-in lenient semantics,
read-side interpretation choices, and per-session diagnostic level only —
they cannot weaken the verifier or the parser.

*(planned)* entries are documented in the design but not yet registered in
[`src/guc.rs`](../../crates/pg_flatbuffers/src/guc.rs); see
[roadmap.md](roadmap.md). v0.1 enforces parser bounds at hard-coded defaults
inside [`query/parser.rs`](../../crates/pg_flatbuffers/src/query/parser.rs)
(see `parse_with_bounds`).

## Indexability

postgres-protobuf cannot be used in index expressions because results depend
on a mutable schema; the same fundamental constraint holds here.

However, FlatBuffers' field-id stability + zero-copy reads make a meaningful
subset indexable in a future release: a query expressed *purely* in terms of
field ids and fixed indices (no named lookups, no `[*]`, no `MapKey`) is
deterministic given the bytes alone — the schema is only needed to know the
*type* of the leaf, not to decode it.

A separate function family, e.g. `flatbuffers_get_int(buf, field_id_path int[])`,
that is `IMMUTABLE` and therefore index-expression-safe, is on the v0.2
[roadmap.md](roadmap.md). The v0.1 surface leaves room.
