# postgres-protobuf: Background Research

Source: [mpartel/postgres-protobuf](https://github.com/mpartel/postgres-protobuf)
License: MIT
Latest release: v0.3.2 (Jan 23, 2025)
Languages: C++ (~73%), Ruby (~20%, test generation), Makefile, Shell, Dockerfile
Postgres compatibility: PG 11–17

## Overview

`postgres-protobuf` is a PostgreSQL extension by Martin Pärtel that allows
storing [Protocol Buffer](https://developers.google.com/protocol-buffers/)
encoded values in regular `bytea` columns and querying their contents in SQL.
It is written in C++ and links against Google's protobuf library.

The project sits in the same design space as Postgres's built-in `json`/`jsonb`
support, but for binary protobuf payloads: it lets applications keep semi-
structured data in the database without exploding every field into its own
column, while still being able to read individual fields out from SQL.

## Motivation

Per the project README, the rationale for storing protobufs in a database is
similar to that for storing JSON:

- Less glue code to convert between application data structures and rows.
- Fewer `ALTER TABLE` migrations as fields are added or removed.

Compared to JSON, protobuf brings:

- A more compact and efficient binary representation.
- Schema-driven evolution (field numbers rather than field names) that
  preserves forward/backward compatibility under renames.

Compared to storing protobufs as their canonical JSON form in `json`/`jsonb`,
this extension avoids the cost of JSON encoding and the field-name coupling
that breaks when proto fields are renamed.

## Feature Set

The extension exposes two main capabilities:

1. Selecting parts of a protobuf via a small path-based query language.
2. Converting protobuf bytes to and from canonical JSON text.

### Schema registration

Schemas are registered by uploading a `FileDescriptorSet` (produced by
`protoc --descriptor_set_out=schema.pb --include_imports`) into a managed
table:

```sql
INSERT INTO protobuf_file_descriptor_sets (name, file_descriptor_set)
VALUES ('default', :contents_of_file);
```

Multiple named descriptor sets can coexist; queries default to the set named
`default`.

### SQL functions

| Function | Returns |
| --- | --- |
| `protobuf_query(query, protobuf)` | First matching field, or `NULL` if missing or proto3 default |
| `protobuf_query_array(query, protobuf)` | All matching fields as a `text[]` |
| `protobuf_query_multi(query, protobuf)` | All matching fields as a set of rows |
| `protobuf_to_json_text(protobuf_type, protobuf)` | Canonical JSON string |
| `protobuf_from_json_text(protobuf_type, json_str)` | Protobuf bytes parsed from JSON |
| `protobuf_extension_version()` | Numeric version `X*10000 + Y*100 + Z` |

### Query language

A query string takes the form:

```
[<descriptor_set>:]<message_name>:<path>
```

The `<path>` supports:

- Field selectors: `submessage.field` (by name or by number).
- Repeated-field index selectors: `field[123]`.
- Map value selectors: `field[123]` or `field[abc]` (numeric or string keys).
- Universal selectors: `field[*]` selects all elements of a repeated field
  or all values of a map.
- Universal map-key selectors: `field|keys` selects all keys of a map.

Example:

```sql
SELECT protobuf_query(
  'MyProto:some_submessage.some_map[some_key]', my_proto_column
)
FROM ...;
```

## Installation

### From source

Requires a Postgres 11+ server with development headers (e.g.
`postgresql-server-dev-$VERSION` on Debian/Ubuntu) and a mostly C++17-capable
compiler:

```
make
sudo make install
```

Then in SQL:

```sql
CREATE EXTENSION postgres_protobuf;
```

### Binary release

Prebuilt binaries (Ubuntu 20.04, AMD64) are published on GitHub Releases. The
package contents map to the standard Postgres `lib/` and `extension/`
directories.

## Implementation Notes

Repository structure (selected):

- `postgres_protobuf.cpp` — extension entry points / SQL function bindings.
- `querying.{cpp,hpp}` — protobuf path query engine.
- `descriptor_db.{cpp,hpp}` — descriptor-set storage and lookup.
- `postgres_utils.{cpp,hpp}` — Postgres / C++ glue (palloc, text encoding,
  float/double formatting tuned to match Postgres output).
- `postgres_protobuf--0.1.sql`, `postgres_protobuf--0.1--0.2.sql` — SQL
  install/upgrade scripts.
- `postgres_protobuf.control` — extension control file.
- `test_protos/`, `generate_test_cases.rb` — Ruby-driven test-case generation.
- `Dockerfile`, `build-and-test.sh`, `docker-build-dist.sh` — reproducible
  build/test environment and release packaging.

## Caveats (from upstream README)

### Security

Because the extension is written in C++, the author cautions against running
untrusted queries or untrusted protobuf bytes through it. Parsing and
re-serializing untrusted data first is recommended. JSON conversion is
considered safer, since it thinly wraps the upstream protobuf library.

### Performance

- Each query loads and scans the entire protobuf column value — large
  payloads are correspondingly slower than columnar storage.
- Descriptor sets are deserialized and cached only for the duration of a
  single transaction; batched reads should be wrapped in a transaction.
- Query functions cannot be used in index expressions, since results depend
  on a mutable schema. The README notes a possible future relaxation for
  queries written purely in terms of field numbers.

### Memory management

- The protobuf library does not support custom allocators, so most allocation
  goes to the default C++ heap and bypasses Postgres's memory accounting.
- Memory use is approximately
  `O(|descriptor sets| + |largest protobuf queried| + |result set|)`.
- Map values are buffered before scanning, so deeply nested maps (especially
  with recursive schemas) can use disproportionate memory; this caveat
  applies to queries but not to JSON conversion.

### Compatibility

- Both proto2 and proto3 work; proto2 `groups` are not supported.
- proto3 default values are returned as `NULL`/absent, since proto3 does not
  store them on the wire.
- Tested on AMD64 only.

### Advanced operations

The extension intentionally does not provide functions for mutating protobuf
contents and has no plans to significantly extend the query language. The
recommended workaround for one-off mutations is to round-trip through JSON
using Postgres's existing JSON operators.

## Comparison with pg_protobuf

The README contrasts the project with the older
[pg_protobuf](https://github.com/afiskon/pg_protobuf), an experiment that is
no longer actively maintained. `pg_protobuf` is much smaller and depends on
neither the protobuf C++ library nor C++ itself, but it requires fields to be
referenced by number and offers fewer conveniences.

## Relevance to `pg_flatbuffers`

For a FlatBuffers-oriented Postgres extension, postgres-protobuf is the
closest prior art and a useful reference for:

- **Schema registration model** — a `bytea` table holding serialized
  schemas/descriptor sets, addressable by a name (the "default" namespace
  pattern).
- **Function surface** — single-result, array, and set-returning query
  variants, plus JSON round-trip helpers.
- **Path query syntax** — field/index/map/universal selectors composable
  into a single string argument.
- **Caveats to design around from day one** — non-indexability of schema-
  dependent extractors, allocator/memory accounting boundaries between
  Postgres and a vendored C++ library, per-transaction schema caching, and
  proto3-style "missing vs. default" semantics (which FlatBuffers also has,
  via default-valued scalars and absent tables).

Areas where FlatBuffers differs and the design will need to diverge:

- FlatBuffers buffers are random-access and zero-copy, so query execution
  can avoid the "scan the whole payload" cost called out in the protobuf
  performance caveat — and may make field-number-only index expressions
  more practical.
- FlatBuffers schemas (`.bfbs` reflection) are structurally different from
  protobuf `FileDescriptorSet`s and will need their own registration table
  and lookup helpers.
- There is no canonical JSON form baked into the wire format; JSON
  round-tripping must go through the FlatBuffers reflection / `flatc`
  tooling.
