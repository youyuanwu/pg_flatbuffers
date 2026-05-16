# SQL surface

Naming mirrors `postgres-protobuf` almost one-for-one to keep the surface
familiar. All functions are `SECURITY INVOKER` (the pgrx default) — they run
with the caller's privileges and never escalate.

## Functions

| Function | Returns | Implementation |
| --- | --- | --- |
| `flatbuffers_query(query text, buf bytea)` | `text` | [`functions/query.rs`](../../crates/pg_flatbuffers/src/functions/query.rs) |
| `flatbuffers_query_array(query text, buf bytea)` | `text[]` | [`functions/query_array.rs`](../../crates/pg_flatbuffers/src/functions/query_array.rs) |
| `flatbuffers_query_multi(query text, buf bytea)` | `SETOF text` | [`functions/query_multi.rs`](../../crates/pg_flatbuffers/src/functions/query_multi.rs) |
| `flatbuffers_to_json(table_name text, buf bytea)` | `jsonb` | [`functions/to_json.rs`](../../crates/pg_flatbuffers/src/functions/to_json.rs) |
| `flatbuffers_to_json_text(table_name text, buf bytea)` | `text` | [`functions/to_json.rs`](../../crates/pg_flatbuffers/src/functions/to_json.rs) |
| `flatbuffers_from_json(table_name text, j jsonb)` | `bytea` | [`functions/from_json.rs`](../../crates/pg_flatbuffers/src/functions/from_json.rs) |
| `flatbuffers_from_json_text(table_name text, j text)` | `bytea` | [`functions/from_json.rs`](../../crates/pg_flatbuffers/src/functions/from_json.rs) |
| `flatbuffers_validate_schema(bfbs bytea)` | `boolean` | [`catalog.rs`](../../crates/pg_flatbuffers/src/catalog.rs) |
| `flatbuffers_verify(table_name text, buf bytea)` | `boolean` | [`functions/verify.rs`](../../crates/pg_flatbuffers/src/functions/verify.rs) |
| `flatbuffers_root_type(schema_name text)` | `text` | [`functions/root_type.rs`](../../crates/pg_flatbuffers/src/functions/root_type.rs) |
| `flatbuffers_extension_version()` | `int` | [`src/lib.rs`](../../crates/pg_flatbuffers/src/lib.rs) — `MAJOR * 10000 + MINOR * 100 + PATCH` |

Read-side functions return SQL `NULL` for an absent leaf or empty buffer; the
contract for malformed input is governed by
[`pg_flatbuffers.strict`](safety.md#gucs). JSON conversion always raises
`ERROR` on a verifier failure regardless of `strict`
(see [json-conversion.md](json-conversion.md)).

`flatbuffers_extension_version()` returns `X*10000 + Y*100 + Z`, sourced from
the package version at compile time via `CARGO_PKG_VERSION_*` env vars.

The `jsonb` return for `flatbuffers_to_json` composes with Postgres's existing
JSON operator set; callers wanting a string can wrap with `::text` or use the
`_text` variant.

### Privileges

The install script grants `EXECUTE` on read-only functions
(`flatbuffers_query*`, `flatbuffers_to_json*`, `flatbuffers_root_type`,
`flatbuffers_extension_version`, `flatbuffers_verify`) to `PUBLIC`.

Write-side functions (`flatbuffers_from_json*`, `flatbuffers_validate_schema`)
are also granted to `PUBLIC` by default. `from_json` is the most expensive
entry point in the API, so operators handling untrusted input may want to
`REVOKE EXECUTE ON FUNCTION flatbuffers_from_json(text, jsonb),
flatbuffers_from_json_text(text, text) FROM PUBLIC`.

Composite-typed query results (the "give me the value with its declared
FlatBuffers type" form) are explicitly out of scope for v0.1; everything is
stringified. Typed accessors (`flatbuffers_query_int`, `_double`, …) are a
[roadmap.md](roadmap.md) item.

## Query language

```
[<schema>:]<table_name>:<path>
```

Grammar lives in [`query/parser.rs`](../../crates/pg_flatbuffers/src/query/parser.rs)
and produces the AST defined in [`query/ast.rs`](../../crates/pg_flatbuffers/src/query/ast.rs).

- `<schema>` is the row name in `flatbuffers_schemas`, defaulting to
  `'default'`.
- `<table_name>` is a fully-qualified FlatBuffers object name (namespace
  dot-separated), e.g. `myco.orders.Order`.
- `<path>` is a sequence of:
  - **Field selectors:** `field`, `submessage.field`. Fields may be
    referenced by name (`field`) or by FlatBuffers field id (`#7`). The id
    form resolves through `reflection::Field::id()` (the value of the `(id: N)`
    annotation), never declaration order. A field without an explicit `(id:)`
    must be referenced by name.
  - **Index selectors on vectors:** `field[7]`. Returns `NULL` (does not
    `ERROR`) when the index is out of range or the vector is empty.
    Negative indices are a parser error. Path traversal short-circuits at
    the first `NULL` step.
  - **Universal selectors on vectors:** `field[*]`.
  - **Map-like selectors:** FlatBuffers has no first-class map type, but
    vectors of tables with a `(key)`-annotated field are conventionally
    treated as maps. We support `field[abc]` against such vectors. Lookup
    strategy is governed by
    [`pg_flatbuffers.key_lookup_strict`](safety.md#gucs); on (default)
    bisects the vector under the FlatBuffers key-sorted contract, off falls
    back to a linear scan. Implementation:
    [`query/executor/map_key.rs`](../../crates/pg_flatbuffers/src/query/executor/map_key.rs).
  - **`field|keys`** to enumerate the key field of a `(key)`-annotated vector.

### Differences vs. protobuf

#### Unions

A FlatBuffers union field `f: U` is stored as two slots: the discriminator
`f_type: U` and the value `f` (a forward `uoffset_t` to the variant's
content). The executor treats `submessage.f` transparently — reads the
discriminator, then routes to the active member. A trailing `.<member_name>`
is allowed for explicit disambiguation. Implementation:
[`query/executor/union.rs`](../../crates/pg_flatbuffers/src/query/executor/union.rs).

**Variant kinds.** Three are supported (matching the upstream `flatc` set):

- **Table variants** (original union shape) — descent via `walk_table`;
  reached by `Msg:body.<field>`.
- **Struct variants** (`flatc` ≥ 1.12) — descent via `walk_struct`; the
  union value slot's `uoffset_t` points at *out-of-line* struct bytes (in
  contrast to a struct *field*, whose bytes are inlined into the parent
  table's body).
- **String variants** (`flatc` ≥ 2.0) — leaf form only; `Msg:body` returns
  the UTF-8 content directly. Any trailing `.<field>` is rejected as a
  type-shape error.

The `|type` terminal step returns the active variant's reflected `EnumVal`
name (e.g. `"S"`, `"string"`, `"NONE"`) for all variant kinds.

**Vectors of unions** (`f: [U]`) are rejected at *schema* registration time;
the `flatbuffers-reflection` 0.1.0 verifier has no vector-of-union path, so
[`verify.rs::reject_unsupported_schema_features`](../../crates/pg_flatbuffers/src/verify.rs)
surfaces a deterministic error that names the table and field. Tracked for v0.2
in [roadmap.md](roadmap.md).

#### Structs

Struct fields are inlined and have no vtable; the executor treats them
identically to tables for path traversal but uses a faster fixed-offset
reader. Implementation:
[`query/executor/struct_.rs`](../../crates/pg_flatbuffers/src/query/executor/struct_.rs).

#### Defaults and absence

FlatBuffers distinguishes several cases that collapse to one in protobuf.
The executor consults the reflection schema
(`reflection::Field::optional()`, `Field::required()`, `Field::default_*()`)
per field:

- `required` field — always present on the wire; reading returns the value,
  never `NULL` (even if equal to the declared default).
- Optional scalar with `= null` (FlatBuffers ≥ 2.0) — absent → `NULL`;
  present (including present-equals-default) → the value.
- Plain scalar with declared default `D` and no `(force_defaults)` — omitted
  from the wire when value equals `D`. Reading an omitted slot returns `D`
  by default (matches the FlatBuffers reader API). The USERSET GUC
  [`pg_flatbuffers.fill_scalar_defaults`](safety.md#gucs) `= off` surfaces
  absent scalars as SQL `NULL` instead, recovering presence-aware semantics.
- Buffer built with `(force_defaults)` — default-valued slots are on the
  wire and read as their stored value, indistinguishable from explicitly-set
  defaults. The executor never value-compares against the declared default.
- Sub-tables, strings, and vectors that are absent from the parent vtable
  → `NULL`.

## Examples

```sql
-- Register a schema produced by `flatc -b --schema orders.fbs`
INSERT INTO flatbuffers_schemas (name, bfbs)
VALUES ('default', pg_read_binary_file('/tmp/orders.bfbs'));

-- Single-value extraction
SELECT flatbuffers_query('myco.orders.Order:customer.email', payload)
FROM orders_raw
WHERE id = 42;

-- All SKUs across line items
SELECT flatbuffers_query_array(
         'myco.orders.Order:items[*].sku', payload)
FROM orders_raw;

-- As rows, suitable for joins / aggregation
SELECT o.id, sku
FROM orders_raw o,
     LATERAL flatbuffers_query_multi(
       'myco.orders.Order:items[*].sku', o.payload) AS sku;

-- JSON round-trip
SELECT flatbuffers_to_json('myco.orders.Order', payload)->'customer'->>'email'
FROM orders_raw;
```
