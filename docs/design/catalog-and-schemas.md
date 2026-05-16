# Schema registration and catalog

FlatBuffers wire data is **not self-describing** — a buffer is just an offset
table and packed scalars; field tags / vtables only make sense relative to the
original `.fbs` schema. The extension therefore needs a registration step
analogous to protobuf's `FileDescriptorSet`.

## The `.bfbs` binary schema

`flatc` can emit a **binary schema** with `flatc -b --schema my.fbs`
(`--bfbs-comments` / `--bfbs-builtins` as needed). The `.bfbs` file is itself
a FlatBuffer that conforms to `reflection.fbs`, listing every object
(table/struct), enum, field, default value, and root type.

Users insert that `.bfbs` blob into the extension-managed
[`flatbuffers_schemas`](#the-flatbuffers_schemas-catalog) table, addressed by a
name. Queries reference the schema by `(name, root_or_message_type)`. Schema
lookups happen at query time. The extension never inspects the bytes in the
data column to guess a type — the SQL caller always tells us which FlatBuffers
*table* the bytes are an instance of.

## The `flatbuffers_schemas` catalog

The catalog DDL lives in [`sql/catalog.sql`](../../crates/pg_flatbuffers/sql/catalog.sql)
(emitted by pgrx via `extension_sql_file!` in
[`src/lib.rs`](../../crates/pg_flatbuffers/src/lib.rs)):

```sql
CREATE TABLE flatbuffers_schemas (
    name             text         PRIMARY KEY,
    bfbs             bytea        NOT NULL,
    root_table       text         NOT NULL,
    file_identifier  text,
    inserted_at      timestamptz  NOT NULL DEFAULT now(),
    CONSTRAINT flatbuffers_schemas_bfbs_valid
        CHECK (flatbuffers_validate_schema(bfbs))
);
```

The CHECK constraint calls
[`flatbuffers_validate_schema`](../../crates/pg_flatbuffers/src/catalog.rs)
on INSERT and UPDATE so a malformed `.bfbs` never reaches the cache. The
function runs the same verifier (with the same bounds) as the read path; see
[safety.md](safety.md).

A row named `'default'` is the implicit schema, mirroring postgres-protobuf.

## Access control (security boundary)

The catalog is a **trust boundary**: every value in `bfbs` is fed to the
reflection parser and then used to walk untrusted user payloads. A malicious
or careless write can silently corrupt every query that resolves through the
affected schema name. The DDL therefore treats the catalog as privileged
infrastructure by default:

- A dedicated role `flatbuffers_admin` (NOLOGIN, NOINHERIT) is created
  idempotently and owns `flatbuffers_schemas`.
- `INSERT`, `UPDATE`, `DELETE`, and `TRUNCATE` are `REVOKE`d from `PUBLIC`.
  `SELECT` is `GRANT`ed to `PUBLIC` (read access is required by the query
  functions).
- `flatbuffers_admin` is the only role with write access by default.
  Operators grant the role to specific application or migration roles with
  `GRANT flatbuffers_admin TO my_app_owner`.

PostgreSQL reserves role names beginning with `pg_`, so the role uses the
`flatbuffers_` prefix.

**Operator guidance.** Never `GRANT INSERT ON flatbuffers_schemas TO PUBLIC`
and never run application code as `flatbuffers_admin` unless schema
registration is explicitly part of that code path. Schema rotation should
happen out-of-band (migration tooling) under the admin role.

## Cache invalidation trigger

Plain DML on user tables does not emit relcache invalidation messages, so
[`sql/catalog.sql`](../../crates/pg_flatbuffers/sql/catalog.sql) installs a
STATEMENT-level `AFTER` trigger
([`flatbuffers_schemas_invalidate_trigger`](../../crates/pg_flatbuffers/src/catalog.rs))
that calls `CacheInvalidateRelcacheByRelid` so every other backend drops its
cached entries on the next statement. The cache-side wiring is described in
[schema-cache.md](schema-cache.md).
