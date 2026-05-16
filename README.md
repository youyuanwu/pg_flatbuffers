# pg_flatbuffers

A PostgreSQL 18 extension that lets you store [FlatBuffers](https://flatbuffers.dev/)
payloads in `bytea` columns and query, verify, or round-trip them to JSON in
SQL — all driven by a registered reflection schema.

Implemented in Rust on [pgrx](https://github.com/pgcentralfoundation/pgrx),
with FlatBuffers' zero-copy reads where they help.

## Quick example

```sql
CREATE EXTENSION pg_flatbuffers;

-- Register a schema produced by `flatc -b --schema orders.fbs`
INSERT INTO flatbuffers_schemas (name, bfbs)
VALUES ('default', pg_read_binary_file('/tmp/orders.bfbs'));

-- Single-value extraction
SELECT flatbuffers_query('myco.orders.Order:customer.email', payload)
FROM   orders_raw
WHERE  id = 42;

-- Vector fan-out as rows (suitable for joins)
SELECT o.id, sku
FROM   orders_raw o,
       LATERAL flatbuffers_query_multi(
         'myco.orders.Order:items[*].sku', o.payload) AS sku;

-- JSON round-trip
SELECT flatbuffers_to_json('myco.orders.Order', payload) -> 'customer'
FROM   orders_raw;
```

See [docs/design/sql-surface.md](docs/design/sql-surface.md) for the full
function and query-language reference.

## SQL surface

| Function | Returns | Purpose |
| --- | --- | --- |
| `flatbuffers_query(q, buf)` | `text` | First leaf matched by a path query |
| `flatbuffers_query_array(q, buf)` | `text[]` | All leaves, wire order |
| `flatbuffers_query_multi(q, buf)` | `SETOF text` | All leaves as rows |
| `flatbuffers_to_json{,_text}(t, buf)` | `jsonb` / `text` | Reflection-driven JSON encode |
| `flatbuffers_from_json{,_text}(t, j)` | `bytea` | Build a buffer from JSON |
| `flatbuffers_verify(t, buf)` | `boolean` | Bounded verifier, suitable for `CHECK` |
| `flatbuffers_validate_schema(bfbs)` | `boolean` | Verify a `.bfbs` registration blob |
| `flatbuffers_root_type(name)` | `text` | Diagnostic |
| `flatbuffers_extension_version()` | `int` | Packed `X*10000 + Y*100 + Z` |

## Building from source

Requires Rust 1.95 (pinned in [rust-toolchain.toml](rust-toolchain.toml)) and
the system packages listed by `just doctor`. The repo is a Cargo workspace
with the extension at [crates/pg_flatbuffers/](crates/pg_flatbuffers/).

```sh
# One-time: provision PG 18 in the repo-local .pgrx/ (~5–10 min, ~1.5 GB)
just init

# Inner loop
just unit            # pure-Rust unit tests, no Postgres needed
just test pg18      # full #[pg_test] suite against PG 18
just regress pg18   # pg_regress golden-file end-to-end tests
just check          # fmt-check + clippy + unit + pg18 tests + regress
just package pg18   # produces an installable extension tree
```

CI runs [`just check`](Justfile) on every push and pull request — see
[.github/workflows/ci.yml](.github/workflows/ci.yml).

## Safety notes

Every read path runs the bounded reflection verifier
([crates/pg_flatbuffers/src/verify.rs](crates/pg_flatbuffers/src/verify.rs))
before any field access. Resource bounds (`max_depth`, `max_tables`,
`max_apparent_size_mb`) are SUSET GUCs so an unprivileged session cannot
lift them. The `flatbuffers_schemas` catalog is owned by a dedicated
`flatbuffers_admin` role with `INSERT`/`UPDATE` REVOKEd from `PUBLIC`.

Details in [docs/design/safety.md](docs/design/safety.md).

## Documentation

- [docs/design/README.md](docs/design/README.md) — design index
- [docs/design/sql-surface.md](docs/design/sql-surface.md) — functions and query language
- [docs/design/json-conversion.md](docs/design/json-conversion.md) — per-type JSON encoding
- [docs/design/safety.md](docs/design/safety.md) — verifier, GUCs, indexability
- [docs/design/roadmap.md](docs/design/roadmap.md) — v0.2 / v0.3 plans

## Status

v0.1 — PG 18 only, source builds. PG 17, binary releases, and field-id
accessors enabling functional indexes are planned for v0.2; see
[roadmap.md](docs/design/roadmap.md).

## License

[MIT](LICENSE).
