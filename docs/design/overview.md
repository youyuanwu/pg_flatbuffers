# Overview

## Goal

`pg_flatbuffers` is a PostgreSQL 18 extension, implemented in Rust on
[pgrx](https://github.com/pgcentralfoundation/pgrx), that lets users:

1. Store [FlatBuffers](https://flatbuffers.dev/) payloads in regular `bytea`
   columns.
2. Register one or more FlatBuffers schemas with the database
   ([catalog-and-schemas.md](catalog-and-schemas.md)).
3. Query individual fields, repeated entries, and nested tables/structs out of
   those payloads in SQL ([sql-surface.md](sql-surface.md)).
4. Round-trip FlatBuffers values to and from JSON using the schema
   ([json-conversion.md](json-conversion.md)).

The model is deliberately close to
[`mpartel/postgres-protobuf`](https://github.com/mpartel/postgres-protobuf),
both to give users a familiar mental model and to make the design space small.
FlatBuffers' zero-copy / random-access wire format unlocks a few optimizations
that protobuf cannot do; those are called out where relevant.

**Non-goals (initial release):**

- Mutating FlatBuffers values in place.
- A full path-expression language with arithmetic, predicates, or wildcards
  beyond the protobuf-style `[*]`.
- Indexing arbitrary path expressions on disk
  (see [safety.md → indexability](safety.md#indexability)).
- Schema migration tooling beyond storing multiple named schemas side by side.

## Why Rust + pgrx

[pgrx](https://github.com/pgcentralfoundation/pgrx) is the de-facto framework
for Postgres extensions in Rust and gives us:

- Safe wrappers around `palloc`/`pfree`, `Datum`, `text`, `bytea`, varlena.
- `#[pg_extern]` for SQL function declarations, with the install-script DDL
  emitted via `extension_sql_file!` (see [`src/lib.rs`](../../crates/pg_flatbuffers/src/lib.rs)).
- Set-returning function (`SETOF`) and composite-type return support — used by
  [`flatbuffers_query_multi`](../../crates/pg_flatbuffers/src/functions/query_multi.rs).
- A built-in test harness ([`pgrx-tests`](https://github.com/pgcentralfoundation/pgrx))
  that spins up real Postgres instances per supported major version.

Rust also lets us depend directly on the official
[`flatbuffers`](https://crates.io/crates/flatbuffers) crate plus the
[`flatbuffers-reflection`](https://crates.io/crates/flatbuffers-reflection)
crate (which exposes the generated bindings for the `reflection.fbs` schema
that ships with FlatBuffers itself). This is the FlatBuffers analogue of
protobuf's `DescriptorPool`.

**Memory-safety vs. C++.** postgres-protobuf is C++ and spends a section in its
README warning that care is warranted with untrusted inputs. A Rust
implementation removes most of that class of risk by construction; defensive
handling of untrusted *FlatBuffers bytes* still applies (see
[safety.md](safety.md)).

## Module layout

The repository is a Cargo **workspace** so future companion crates (reusable
parsers, fixture generators, benchmark harnesses, alternate front-ends) can
live alongside the extension without entangling their build graphs with pgrx.
The extension itself is the single crate at `crates/pg_flatbuffers/`.

```
pg_flatbuffers/                              # workspace root
├── Cargo.toml                               # [workspace] manifest
├── Cargo.lock
├── rust-toolchain.toml
├── Justfile                                 # `just init`, `just test`, `just check`
├── docs/
│   ├── design/                              # (this folder)
│   └── background/
├── crates/
│   └── pg_flatbuffers/
│       ├── Cargo.toml                       # [package] + pgrx feature flags
│       ├── pg_flatbuffers.control
│       ├── sql/
│       │   └── catalog.sql                  # see catalog-and-schemas.md
│       └── src/
│           ├── lib.rs                       # #[pg_module_magic], _PG_init
│           ├── catalog.rs                   # flatbuffers_schemas DDL helpers
│           ├── schema_cache.rs              # per-backend LRU + sinval
│           ├── verify.rs                    # bounded verifier + structural checks
│           ├── guc.rs                       # GUC registration (max_*, strict, …)
│           ├── to_json.rs                   # flatbuffers → JSON walker
│           ├── from_json.rs                 # JSON → flatbuffers builder
│           ├── query/                       # parser + executor
│           │   ├── parser.rs
│           │   ├── ast.rs
│           │   └── executor/
│           └── functions/                   # #[pg_extern] SQL wrappers
└── target/                                  # workspace-shared build output
```

The workspace [`Cargo.toml`](../../Cargo.toml) declares `members = ["crates/*"]`
and hoists shared dependency versions into `[workspace.dependencies]`. The
extension's [`crates/pg_flatbuffers/Cargo.toml`](../../crates/pg_flatbuffers/Cargo.toml)
pulls those workspace dependencies plus its own pgrx feature flag (`pg18`).

`cargo pgrx` commands run against the extension crate explicitly (the
[`Justfile`](../../Justfile) wraps the common ones — `just init`, `just test pg18`,
`just package pg18`).

### Key dependencies

Declared once in the workspace root, inherited by the extension crate:

- `pgrx` (pinned via `=0.18.0` — pgrx requires exact-version pinning).
- `flatbuffers` (25.x) — runtime reader.
- `flatbuffers-reflection` (0.1.x — independent version line from the
  FlatBuffers project) — generated bindings to `reflection.fbs`, giving us
  `Schema`, `Object`, `Field`, `Type`, `BaseType`, etc.
- `serde_json` for JSON construction (we go straight to/from `jsonb` via
  `pgrx::JsonB`).
- `thiserror` for typed errors mapped to `ereport(ERROR, …)`.
- `parking_lot` + `lru` for the per-backend schema cache.
- `base64` for `[ubyte]` JSON encoding (lowercase hex when `(hex)` is set).
