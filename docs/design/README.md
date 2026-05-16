# pg_flatbuffers — Design

Status: living document — describes the v0.1 design and points at the code that
implements it. When the implementation moves, update the links.

Audience: contributors, reviewers, and operators integrating the extension.

Related background: [../background/postgres-protobuf.md](../background/postgres-protobuf.md)

## Topic index

| Topic | File | Implements |
| --- | --- | --- |
| Goals, language choice, module layout | [overview.md](overview.md) | [`crates/pg_flatbuffers/Cargo.toml`](../../crates/pg_flatbuffers/Cargo.toml), [`src/lib.rs`](../../crates/pg_flatbuffers/src/lib.rs) |
| Schema registration + catalog table | [catalog-and-schemas.md](catalog-and-schemas.md) | [`sql/catalog.sql`](../../crates/pg_flatbuffers/sql/catalog.sql), [`src/catalog.rs`](../../crates/pg_flatbuffers/src/catalog.rs) |
| SQL surface (functions, query language, examples) | [sql-surface.md](sql-surface.md) | [`src/functions/`](../../crates/pg_flatbuffers/src/functions/), [`src/query/parser.rs`](../../crates/pg_flatbuffers/src/query/parser.rs) |
| Per-backend schema cache | [schema-cache.md](schema-cache.md) | [`src/schema_cache.rs`](../../crates/pg_flatbuffers/src/schema_cache.rs) |
| Query parser + executor | [query-execution.md](query-execution.md) | [`src/query/`](../../crates/pg_flatbuffers/src/query/) |
| JSON conversion (to_json / from_json) | [json-conversion.md](json-conversion.md) | [`src/to_json.rs`](../../crates/pg_flatbuffers/src/to_json.rs), [`src/from_json.rs`](../../crates/pg_flatbuffers/src/from_json.rs) |
| Safety, verifier bounds, GUCs, indexability | [safety.md](safety.md) | [`src/verify.rs`](../../crates/pg_flatbuffers/src/verify.rs), [`src/guc.rs`](../../crates/pg_flatbuffers/src/guc.rs) |
| Postgres compatibility, build, packaging, testing | [compatibility-build-test.md](compatibility-build-test.md) | [`Cargo.toml`](../../Cargo.toml), [`Justfile`](../../Justfile) |
| Open questions and roadmap | [roadmap.md](roadmap.md) | — |
