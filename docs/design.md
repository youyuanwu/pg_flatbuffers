# pg_flatbuffers — Design

Status: draft
Audience: contributors and reviewers of the initial implementation
Related: [background/postgres-protobuf.md](background/postgres-protobuf.md)

## 1. Goal

Provide a PostgreSQL extension, implemented in Rust, that lets users:

1. Store [FlatBuffers](https://flatbuffers.dev/) payloads in regular `bytea`
   columns.
2. Register one or more FlatBuffers schemas with the database.
3. Query individual fields, repeated entries, and nested tables/structs out of
   those payloads in SQL.
4. Round-trip FlatBuffers values to and from JSON using the schema.

The model is deliberately close to
[`mpartel/postgres-protobuf`](https://github.com/mpartel/postgres-protobuf),
both to give users a familiar mental model and to make the design space
small. FlatBuffers' zero-copy / random-access wire format unlocks a few
optimizations that protobuf cannot do; those are called out as we go.

Non-goals (initial release):

- Mutating FlatBuffers values in place.
- A full path-expression language with arithmetic, predicates, or wildcards
  beyond the protobuf-style `[*]`.
- Indexing arbitrary path expressions on disk (see §9 for what we *do*
  intend to support eventually).
- Schema migration tooling beyond storing multiple named schemas side by
  side.

## 2. Why Rust + pgrx

[`pgrx`](https://github.com/pgcentralfoundation/pgrx) is the de-facto
framework for Postgres extensions in Rust. It gives us:

- Safe wrappers around `palloc`/`pfree`, `Datum`, `text`, `bytea`, varlena.
- `#[pg_extern]` for SQL function declarations and `extension_sql_file!` /
  generated SQL for install scripts.
- Set-returning function (`SETOF`) and composite-type return support, which
  we need for `flatbuffers_query_multi`.
- A built-in test harness that spins up real Postgres instances per
  supported major version.

Rust also lets us depend directly on the official
[`flatbuffers`](https://crates.io/crates/flatbuffers) crate plus the
[`flatbuffers-reflection`](https://crates.io/crates/flatbuffers-reflection)
crate (which exposes the generated bindings for the `reflection.fbs` schema
that ships with FlatBuffers itself). This is the FlatBuffers analogue of
protobuf's `DescriptorPool`.

Memory-safety vs. the C++ approach: postgres-protobuf's README spends a
section warning that the extension is C++ and therefore care is warranted
with untrusted inputs. A Rust implementation removes most of that class of
risk by construction; we still have to be defensive about untrusted
*FlatBuffers bytes* (see §10).

## 3. How a FlatBuffers schema is known

FlatBuffers wire data is **not self-describing** — a buffer is just an
offset table and packed scalars; field tags / vtables only make sense
relative to the original `.fbs` schema. We therefore need a registration
step analogous to protobuf's `FileDescriptorSet`:

- `flatc` can emit a **binary schema** (`.bfbs`) with
  `flatc -b --schema my.fbs` (or `--bfbs-comments` / `--bfbs-builtins` as
  needed). The `.bfbs` file is itself a FlatBuffer that conforms to
  `reflection.fbs`, listing every object (table/struct), enum, field,
  default value, and root type.
- Users insert that `.bfbs` blob into an extension-managed table, addressed
  by a name. Queries reference the schema by `(name, root_or_message_type)`.

Schema lookups happen at query time. We never look at the bytes in the
column to guess a type — the SQL caller always tells us which FlatBuffers
*table* the bytes are an instance of.

## 4. Surface area

### 4.1 Catalog table

```sql
CREATE TABLE flatbuffers_schemas (
    name             text PRIMARY KEY,
    bfbs             bytea       NOT NULL,
    -- Optional convenience metadata, populated on insert by a trigger:
    root_table       text,                       -- from reflection.Schema.root_table
    file_identifier  text,                       -- 4-char fid if present
    inserted_at      timestamptz NOT NULL DEFAULT now()
);
```

The `'default'` row is the implicit schema, mirroring postgres-protobuf.

#### Access control (security boundary)

The catalog is a **trust boundary**: every value in `bfbs` is fed to the
reflection parser and then used to walk untrusted user payloads. A
malicious or careless write to this table can silently corrupt every
query that resolves through the affected schema name. The extension
therefore treats the catalog as privileged infrastructure by default:

- The extension install script creates a dedicated role
  `flatbuffers_admin` (NOLOGIN, NOINHERIT) and makes it the owner of
  `flatbuffers_schemas`.
- `INSERT`, `UPDATE`, `DELETE`, and `TRUNCATE` on the table are
  `REVOKE`d from `PUBLIC`. `SELECT` is granted to `PUBLIC` (read access
  is required by the query functions).
- `flatbuffers_admin` is the only role with write access by default.
  Operators grant the role to specific application or migration roles
  with `GRANT flatbuffers_admin TO my_app_owner`.
- Schema validation (`flatbuffers_validate_schema(bfbs)`) runs as a
  `CHECK` constraint or `BEFORE INSERT OR UPDATE` trigger on the table
  so that no row reaches the cache without passing the same verifier
  bounds applied to user payloads (see §10).

Install-script SQL (illustrative):

```sql
CREATE ROLE flatbuffers_admin NOLOGIN NOINHERIT;
ALTER TABLE flatbuffers_schemas OWNER TO flatbuffers_admin;
REVOKE ALL                ON flatbuffers_schemas FROM PUBLIC;
GRANT  SELECT             ON flatbuffers_schemas TO   PUBLIC;
GRANT  INSERT, UPDATE,
       DELETE, TRUNCATE   ON flatbuffers_schemas TO   flatbuffers_admin;
```

Readme guidance: never `GRANT INSERT ON flatbuffers_schemas TO PUBLIC`
and never run application code as `flatbuffers_admin` unless schema
registration is explicitly part of that code path. Schema rotation
should happen out-of-band (migration tooling) under the admin role.

A regression test in §13 asserts that an unprivileged role's `INSERT`
into `flatbuffers_schemas` fails by default.

### 4.2 SQL functions

Names mirror postgres-protobuf almost one-for-one to keep the surface
familiar:

| Function | Returns | Notes |
| --- | --- | --- |
| `flatbuffers_query(query text, buf bytea)` | `text` | First match, or `NULL` if absent. Verifier failure raises `ERROR` (see §10 `strict`). |
| `flatbuffers_query_array(query text, buf bytea)` | `text[]` | All matches, absent values skipped. |
| `flatbuffers_query_multi(query text, buf bytea)` | `SETOF text` | Matches as rows in wire-format order (see §7.2). |
| `flatbuffers_to_json(table_name text, buf bytea)` | `jsonb` | Reflection-driven encoding; always `ERROR` on verifier failure. |
| `flatbuffers_to_json_text(table_name text, buf bytea)` | `text` | Raw JSON form (matches `flatc --strict-json`). |
| `flatbuffers_from_json(table_name text, j jsonb)` | `bytea` | Builds a FlatBuffer from JSON. Subject to §10 build bounds. |
| `flatbuffers_from_json_text(table_name text, j text)` | `bytea` | Same, text input. |
| `flatbuffers_validate_schema(bfbs bytea)` | `void` | Verifies a `.bfbs` blob; called by the catalog `CHECK` (see §4.1). Raises `ERROR` on malformed input. |
| `flatbuffers_verify(table_name text, buf bytea)` | `boolean` | Returns whether `buf` parses as `table_name` under current bounds; suitable for `CHECK` constraints. |
| `flatbuffers_root_type(schema_name text)` | `text` | Diagnostic helper. |
| `flatbuffers_extension_version()` | `int` | `X*10000 + Y*100 + Z`. |

#### Privileges

All functions are `SECURITY INVOKER` (the pgrx default) — they run with
the caller's privileges and never escalate. The install script grants
`EXECUTE` on the read-only functions (`flatbuffers_query*`,
`flatbuffers_to_json*`, `flatbuffers_root_type`,
`flatbuffers_extension_version`, `flatbuffers_verify`) to `PUBLIC`.
Write-side functions (`flatbuffers_from_json*`,
`flatbuffers_validate_schema`) are also granted to `PUBLIC` by default,
but `from_json` is the most expensive entry point in the API and
operators handling untrusted input may want to
`REVOKE EXECUTE ON FUNCTION flatbuffers_from_json(text, jsonb), flatbuffers_from_json_text(text, text) FROM PUBLIC`.

We deliberately return `jsonb` (not just `text`) for the JSON form, because
in Postgres `jsonb` composes with the rich existing JSON operator set.
Users who want a string can wrap with `::text` or use the `_text` variant.

Composite-typed query results (the "give me the value with its declared
FlatBuffers type" form) are explicitly out of scope for v1; everything is
stringified. We can add typed accessors (`flatbuffers_query_int`,
`flatbuffers_query_double`, …) in a later release if real usage shows it
matters.

### 4.3 Query language

Same shape as postgres-protobuf:

```
[<schema>:]<table_name>:<path>
```

`<schema>` is the row name in `flatbuffers_schemas`, defaulting to
`'default'`. `<table_name>` is a fully-qualified FlatBuffers object name
(namespace dot-separated), e.g. `myco.orders.Order`. `<path>` is a sequence
of:

- Field selectors: `field`, `submessage.field`. Fields may be referenced
  by name (`field`) or by FlatBuffers field id (`#7`). The id form
  resolves through `reflection::Field::id()` from the schema — i.e., the
  value of the `(id: N)` annotation on the field, never the declaration
  order. A field without an explicit `(id:)` annotation has no id form
  and must be referenced by name.
- Index selectors on vectors: `field[7]`. Returns `NULL` (does not
  `ERROR`) when the index is `>= len(vector)` or the vector is empty.
  Negative indices are a parser error. Path traversal short-circuits at
  the first `NULL` step.
- Universal selectors on vectors: `field[*]`.
- Map-like selectors: FlatBuffers has no first-class map type, but vectors
  of tables with a `key` field are conventionally treated as maps. We
  support `field[abc]` against such vectors when one field is annotated
  `(key)`. Lookup follows the `flatc` convention: the vector must be
  sorted by the keyed field and lookup uses **binary search**. The
  verifier rejects unsorted `(key)` vectors at read time (see §10); set
  `pg_flatbuffers.key_lookup_strict = off` to fall back to a linear scan
  for buffers produced by non-conforming writers.
- `field|keys` to enumerate the key field of a `(key)`-annotated vector.

Differences vs. protobuf:

- **Unions.** A FlatBuffers union field `f: U` is stored as two slots:
  the discriminator `f_type: U` and the value `f` (a forward
  `uoffset_t` to the variant's content). The query engine treats
  `submessage.f` transparently — it reads the discriminator, then
  routes to the active member. A trailing `.<member_name>` is allowed
  for explicit disambiguation.
  - **Variant kinds.** Three variant kinds are supported (matching the
    upstream `flatc` set):
    - **Table variants** (the original union shape) — descent via
      `walk_table`; reached by `Msg:body.<field>`.
    - **Struct variants** (`flatc` ≥ 1.12) — descent via `walk_struct`;
      the union value slot's `uoffset_t` points at *out-of-line*
      struct bytes (in contrast to a struct *field*, whose bytes are
      inlined into the parent table's body).
    - **String variants** (`flatc` ≥ 2.0) — leaf form only; `Msg:body`
      returns the UTF-8 content directly, since strings carry no
      sub-fields. Any trailing `.<field>` is rejected as a type-shape
      error.
    - The `|type` terminal step returns the active variant's
      reflected `EnumVal` name (e.g. `"S"`, `"string"`, `"NONE"`) for
      all variant kinds.
  - **Vectors of unions** (`f: [U]`) use *three* parallel vectors on the
    wire: `f_type` is a `[U]` of discriminators, `f` is a vector of
    value offsets, and a `(deprecated)` length-aligned slot is reserved
    by `flatc` for compatibility. The executor would read the i-th
    entry by pairing `f_type[i]` with `f[i]`. **Deferred to v0.2**:
    the upstream `flatbuffers-reflection` 0.1.0 verifier has no
    vector-of-union path (`verify_vector` returns `TypeNotSupported`
    for `BaseType::Vector` with `element() == BaseType::Union`), and
    landing it in v0.1 would require either vendoring the upstream
    verifier or upstreaming a fix and waiting. For now the
    schema-feature pre-scan in `verify.rs` rejects any schema
    containing a vector-of-union field with a deterministic error
    that names the table and field (rather than letting the upstream
    surface a cryptic `TypeNotSupported`).
- **Structs.** Struct fields are inlined and have no vtable; the engine
  treats them identically to tables for path traversal purposes but uses
  a faster fixed-offset reader.
- **Defaults and absence.** FlatBuffers distinguishes several cases that
  collapse to one in protobuf. The executor consults the reflection
  schema (`reflection::Field::optional()`, `Field::required()`,
  `Field::default_*()`) per field:
  - `required` field — always present on the wire; reading returns the
    value, never `NULL` (even if the value equals the declared default).
  - Optional scalar with `= null` (FlatBuffers ≥ 2.0) — the wire format
    encodes "absent" distinctly from any value. Absent → `NULL`;
    present (including present-equals-default) → the value.
  - Plain scalar with declared default `D` and no `(force_defaults)`
    — omitted from the wire when value equals `D`. Reading an omitted
    slot returns `D`, not `NULL` — matching the upstream FlatBuffers
    reader API. (This differs from postgres-protobuf's proto3 behavior;
    the `USERSET` GUC `pg_flatbuffers.fill_scalar_defaults = off`
    surfaces absent scalars as SQL `NULL` instead, recovering
    presence-aware semantics for users porting workloads or who need
    to distinguish "writer set field to 0" from "writer never set
    field". Default `on` preserves the FlatBuffers reader-API shape.)
  - Buffer built with `(force_defaults)` — default-valued slots are on
    the wire and read as their stored value, indistinguishable from
    explicitly-set defaults. The executor never value-compares against
    the declared default.
  - Sub-tables, strings, and vectors that are absent from the parent
    vtable → `NULL`.

### 4.4 Examples

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

## 5. Module layout

The repository is a Cargo **workspace** so future companion crates
(reusable parsers, fixture generators, benchmark harnesses, alternate
front-ends) can live alongside the extension without entangling their
build graph with `pgrx`. The extension itself is a single crate at
`crates/pg_flatbuffers/`.

```
pg_flatbuffers/                              # repo root = workspace root
├── Cargo.toml                               # [workspace] manifest only
├── Cargo.lock
├── rust-toolchain.toml
├── docs/                                    # (this file lives here)
├── crates/
│   └── pg_flatbuffers/                      # the extension crate
│       ├── Cargo.toml                       # [package] + pgrx feature flags
│       ├── pg_flatbuffers.control
│       ├── sql/
│       │   ├── pg_flatbuffers--0.1.sql      # generated by pgrx
│       │   └── pg_flatbuffers--0.1--0.2.sql # future upgrades
│       ├── src/
│       │   ├── lib.rs                       # pgrx entrypoint, #[pg_module_magic]
│       │   ├── catalog.rs                   # flatbuffers_schemas table CRUD helpers
│       │   ├── schema_cache.rs              # per-backend LRU + sinval (see §6)
│       │   ├── verify.rs                    # bounded verifier + structural checks
│       │   ├── query/
│       │   │   ├── mod.rs                   # public entry: parse + execute
│       │   │   ├── parser.rs                # path syntax -> AST
│       │   │   ├── ast.rs                   # Step / Path types
│       │   │   └── executor.rs              # walks reflection.fbs against a buffer
│       │   ├── json.rs                      # to_json / from_json via reflection
│       │   └── functions.rs                 # #[pg_extern] wrappers, error mapping
│       └── tests/
│           ├── fixtures/                    # .fbs + .bfbs + sample buffers
│           ├── query_tests.rs
│           └── json_tests.rs
└── target/                                  # workspace-shared build output
```

Workspace `Cargo.toml` (root) declares `members = ["crates/*"]` and
hoists shared dependency versions and lints into `[workspace.dependencies]`
/ `[workspace.lints]` so any future crate inherits them with
`workspace = true`. The extension's `Cargo.toml` lives at
`crates/pg_flatbuffers/Cargo.toml` and pulls those workspace dependencies
plus its own `pgrx` feature flag (`pg18`).

`cargo pgrx` commands run against the extension crate explicitly:

```sh
cargo pgrx run  pg18 --package pg_flatbuffers
cargo pgrx test pg18 --package pg_flatbuffers
cargo pgrx package    --package pg_flatbuffers
```

CI (§11) invokes the same `--package pg_flatbuffers` form so a future
sibling crate cannot break extension builds.

Key dependencies (declared once in the workspace root, inherited by the
extension crate):

- `pgrx` (latest stable line with PG 18 support — see §11).
- `flatbuffers` — runtime reader.
- `flatbuffers-reflection` — generated bindings to `reflection.fbs`,
  giving us `Schema`, `Object`, `Field`, `Type`, `BaseType`, etc.
- `serde_json` for JSON construction (we go straight to/from `jsonb` via
  `pgrx::JsonB`).
- `thiserror` for typed errors mapped to `ereport(ERROR, …)`.

## 6. Schema cache

Schemas change rarely and are read by every query, so caching the parsed
`reflection::Schema` is essential. Two design constraints shape the
strategy:

1. **Cluster-wide invalidation must be reliable.** A long-lived
   connection pool with N backends will otherwise serve stale schemas
   for an unbounded window after a schema rotation.
2. **Per-statement re-lookup must be cheap** so single-statement
   transactions (PostgREST, ORMs, pgBouncer in `transaction` mode) do
   not re-parse the schema per query.

Design:

- **Per-backend LRU keyed by `schema_name`** holding the parsed
  `reflection::Schema` plus, for each `Object`, a pre-built
  `name -> Field` hash so field lookup at query time is O(1) per step
  rather than O(fields).
- **Cluster-wide invalidation via `CacheRegisterRelcacheCallback` on
  `flatbuffers_schemas`.** When *any* backend commits an
  INSERT/UPDATE/DELETE on the catalog, the resulting relcache
  invalidation message is delivered to every other backend, which drops
  the entries it holds for the affected names. This is the standard
  Postgres mechanism for catalog-driven cache coherence; it removes the
  need for tuple-version (xmin/xmax) keying and its associated MVCC
  edge cases (HOT updates, rollback, VACUUM).
- **Statement-scoped "current view" memoization.** A `HashMap<String,
  Arc<CachedSchema>>` is held in the per-statement memory context so a
  query that resolves the same schema name many times (e.g.,
  `LATERAL flatbuffers_query_multi(...)`) hits a `Arc::clone` rather
  than the LRU. Scope is single-statement (cleared on statement
  end), not whole-transaction, so a mid-transaction schema replacement
  is visible to the next statement.
- **Cache size** is bounded by `pg_flatbuffers.schema_cache_mb`
  (default 16 MiB; `SIGHUP` per §10); eviction is LRU on insert. Reducing
  the GUC takes effect on the next backend start.
- **Lazy field-index construction.** A cached entry's `name -> Field`
  hash for a given `Object` is built on first access to that `Object`
  rather than at cache load, so registering a 500-table schema does not
  cost the first query that touches only one of those tables.

The LRU is a Rust-side `parking_lot::Mutex<lru::LruCache<String,
Arc<CachedSchema>>>` initialized in `_PG_init`. It is per-backend (each
Postgres backend is single-threaded with respect to its own caches; we
never share across backends).

**MVCC contract for `flatbuffers_schemas`.** A statement reads the
schema row visible to its snapshot at first lookup; that view is pinned
for the rest of the statement via the per-statement memoization. The
next statement re-checks the relcache invalidation queue and, if the
row has been replaced, re-parses. The catalog is therefore safe to
`UPDATE` from another session while a long scan is in progress (the
scan keeps using the schema it started with), and the next statement
in any backend sees the new schema once the writing transaction commits.

## 7. Query execution

The executor is the heart of the extension.

### 7.1 Parse

`parser.rs` consumes the query string and produces a small AST:

```rust
pub struct Query {
    pub schema: Option<String>,    // "default" if omitted
    pub root: String,              // fully-qualified table name
    pub steps: Vec<Step>,
}

pub enum Step {
    Field(FieldRef),               // Name(String) | Id(u16)
    Index(usize),                  // vec[7]
    All,                           // vec[*]
    MapKey(MapKey),                // map[abc] / map[123]
    MapKeys,                       // map|keys
    UnionMember(String),           // explicit ".VariantName"
}
```

The parser is hand-written; the grammar is small enough that a parser
combinator crate would be overkill.

### 7.2 Execute

Execution holds the bytes plus a borrowed `Schema`. State during traversal
is `(current_table_or_struct: &Object, cursor: AnyTable)`.

For each step:

1. **Field by name/id** → look up `Field` on the current `Object` via the
   pre-built `name -> Field` hash from the schema cache (§6). For unions,
   read the discriminator slot and route to the active member; for
   vectors of unions, the field is paired with its `_type` discriminator
   vector (§4.3) and per-element dispatch happens during step 2/3.
2. **Index** → read the `Vector<T>`, bounds-check, descend. Returns
   `NULL` on out-of-range index (see §4.3); subsequent path steps short-
   circuit.
3. **All** → fan out: produce a stream of leaves rather than a single
   one. **Emission order is wire-format order**: ascending vector index
   for `[*]`, depth-first left-to-right for nested `[*]`. This order is
   stable and observable via `WITH ORDINALITY` in `query_multi`.
4. **MapKey** → binary search over the `(key)`-annotated vector for the
   entry whose key field equals the path literal; key is converted to the
   field's declared type before comparing. The verifier asserts the
   vector is sorted by the key field and contains no duplicates; an
   unsorted or duplicate-bearing vector raises `ERROR` (or, with
   `pg_flatbuffers.key_lookup_strict = off`, the executor falls back to
   a linear scan and returns the first match in wire order).
5. **MapKeys** → fan out the key field of every entry, in wire order.

Output assembly:

- `flatbuffers_query` consumes the first leaf and returns it.
- `flatbuffers_query_array` collects into a `Vec<Option<String>>` →
  `text[]` preserving wire order; absent values are skipped.
- `flatbuffers_query_multi` returns a `SetOfIterator<&str>` from pgrx in
  wire-format order, suitable for `WITH ORDINALITY`.

Output assembly:

- `flatbuffers_query` consumes the first leaf and returns it.
- `flatbuffers_query_array` collects into a `Vec<Option<String>>` →
  `text[]` (with proto3-style "skip missing/default" semantics).
- `flatbuffers_query_multi` returns a `SetOfIterator<&str>` from pgrx.

Leaf scalars are stringified with the same formatters Postgres uses for
`float4`/`float8` (this matters for round-trip stability — the protobuf
extension specifically called this out and we will reuse Postgres's
`float4out`/`float8out` via pgrx's built-in conversions, not Rust's
default `Display`).

### 7.3 Random access advantage

Because FlatBuffers is random-access, scanning for `Order:items[3].sku`
involves: deref root vtable → deref `items` offset → read vtable[3] →
deref `sku` offset → read `bytes`. There is no need to walk the entire
buffer the way protobuf must. We will *not* claim "indexable" yet (see
§9), but expect single-field lookups to be measurably faster than the
protobuf extension's per-call cost for large payloads.

## 8. JSON conversion

We implement `flatbuffers_to_json` / `from_json` ourselves using the
reflection schema rather than shelling out to `flatc`:

- `to_json` walks the same executor primitives but emits a
  `serde_json::Value` instead of stringifying leaves. It uses `flatc`'s
  JSON conventions, summarized below.
- `from_json` uses `flatbuffers::FlatBufferBuilder` to construct a
  buffer bottom-up, driven by the schema's field ordering. The builder
  is bounded by the same GUCs as the read path: `max_apparent_size_mb`
  caps the resulting buffer size, and `max_build_depth` (default 32)
  caps JSON nesting. A pre-walk of the input JSON enforces these
  bounds *before* allocation, so a malicious payload cannot drive the
  builder to OOM.

### Per-type JSON encoding

| FlatBuffers type | JSON shape |
| --- | --- |
| numeric scalars (`int8`…`int64`, `uint8`…`uint64`, `float`, `double`) | JSON number |
| `bool` | JSON boolean |
| enum | JSON string (member name); numeric on round-trip if the value is unknown |
| `string` | JSON string (UTF-8) |
| `[ubyte]` / `[u8]` | JSON string, base64-encoded (default); honors `(hex)` attribute as lowercase hex |
| `[T]` for other `T` | JSON array |
| table / struct | JSON object |
| vector of `(key)`-annotated tables | JSON object keyed by the `(key)` field's value; `from_json` also accepts a JSON array, and sorts the resulting vector by the key field per the FlatBuffers contract |
| union | flatc-style sibling field pair: `<name>_type` (string) + `<name>` (variant value); NONE emits only the type field |

#### v0.1 implementation status

`flatbuffers_to_json` / `flatbuffers_to_json_text` and
`flatbuffers_from_json` / `flatbuffers_from_json_text` are live and
cover the table above, including the `(key)`-annotated vector ↔
JSON object sugar (both directions: `to_json` emits a JSON object
keyed by the `(key)` field; `from_json` accepts either a JSON
object or a JSON array and sorts the resulting vector by the key
field per the FlatBuffers contract). The `(hex)` attribute on
`[ubyte]` is honored both ways (`to_json` emits lowercase hex,
`from_json` accepts either case). The `pg_flatbuffers.max_apparent_size_mb`
bound is enforced on `from_json` output (the finished buffer is
discarded with `FromJsonError::OutputTooLarge` if it exceeds the
cap). Remaining carve-outs:

- **Non-finite floats** (`NaN`, `±Infinity`) raise `ERROR` on the
  `to_json` side rather than serializing as a lossy JSON string. They
  have no native JSON representation, and silently coercing them would
  break round-trip through `from_json`.
- **`from_json` deferrals** (rejected with a `Unsupported` error that
  names the offending field):
  - **Struct sizes / alignments not in the v0.1 dispatch table.** The
    `(size, align)` table in [`crate::from_json`] covers all common
    combinations (align ∈ {1, 2, 4, 8}, size up to 256 bytes) for
    inline placement, out-of-line union values, *and* vector
    elements. Schemas with unusual `bytesize` × `minalign` pairs
    (e.g. `(96, 4)`) raise a clear error pointing at the field;
    extending the table is a one-line addition.

### `from_json` policies

- **Missing `required` field** → `ERROR`.
- **Unknown JSON key** (no field of that name on the target table)
  → `ERROR` by default; set `pg_flatbuffers.from_json_unknown = ignore`
  (USERSET) to silently drop unknown keys for forward-compat workflows.
- **Type mismatch** (e.g., string where number expected) → `ERROR`.
- **Resulting buffer would exceed `max_apparent_size_mb`** → `ERROR`
  raised before the buffer is finalized.
- **Nesting exceeds `max_build_depth`** → `ERROR` raised during the
  pre-walk, before any builder allocation.

JSON conversion is the safest entry point for untrusted *bytes* (because
it makes a single linear pass via reflection without exposing the raw
query language); the bounded-build policy above makes it equally safe
for untrusted *JSON*.

## 9. Indexability (deferred)

postgres-protobuf cannot be used in index expressions because results
depend on a mutable schema. We have the same fundamental constraint.

However, FlatBuffers' field-id stability + zero-copy reads make a
meaningful subset indexable in a future release:

- A query expressed *purely* in terms of field ids and fixed indices (no
  named lookups, no `[*]`, no `MapKey`) is deterministic given the bytes
  alone — the schema is only needed to know the *type* of the leaf, not to
  decode it.
- We can offer a separate function family, e.g.
  `flatbuffers_get_int(buf, field_id_path int[])`, that is `IMMUTABLE` and
  therefore index-expression-safe. v1 ships without these; the design
  leaves room.

## 10. Safety / untrusted input

FlatBuffers' minimal validation is famously a footgun: a buffer crafted
by an attacker can produce out-of-bounds reads if read with the unchecked
APIs. We always go through the verifier:

- `flatbuffers::root_with_opts::<T>` (or the reflection-based equivalent)
  with `VerifierOptions` that bound `max_depth`, `max_tables`, and
  `max_apparent_size`. These bounds are exposed as GUCs (see the table
  below for `GucContext` assignments).
- The same verifier with the same bounds is applied at INSERT/UPDATE
  time on `flatbuffers_schemas.bfbs` (via `flatbuffers_validate_schema`),
  so a malformed reflection blob cannot reach the cache.
- The verifier additionally checks structural invariants the executor
  relies on: `(key)`-annotated vectors must be sorted by the key field
  and free of duplicates (see §7.2 step 4); violations raise `ERROR`
  unless `pg_flatbuffers.key_lookup_strict = off`. Vector-of-union
  fields are rejected at *schema* registration (`v0.1` deferral; see
  §4.3) — a v0.2 follow-on will lift this once the upstream
  `flatbuffers-reflection` verifier (or a vendored fork) gains the
  matching-length / per-element checks.
- **Failure semantics.** When the verifier rejects a payload (malformed
  bytes, exceeded bound, broken `(key)` invariant), `query_*` functions
  raise `ERROR` by default; setting `pg_flatbuffers.strict = off` in the
  session substitutes `NULL` instead and continues the scan. JSON
  conversion functions (`flatbuffers_to_json*`, `flatbuffers_from_json*`)
  always raise `ERROR` regardless of `strict`, since a partially-valid
  document has no defensible JSON encoding.
- **Degenerate-payload contract.** Specific cases under default
  `strict = on`:
  - Zero-length `bytea` → `NULL` (treated as "absent payload," not as
    "malformed"); under `strict = off` likewise `NULL`.
  - `bytea` smaller than the FlatBuffers root-offset minimum (4 bytes)
    → `ERROR` (`strict = on`) or `NULL` (`strict = off`).
  - `bytea` whose declared apparent size exceeds
    `max_apparent_size_mb` → `ERROR` always (this is a bound, not a
    parse failure; `strict` does not relax bounds).
  - SQL `NULL` `bytea` argument → `NULL` result (standard SQL strict
    semantics; the executor is never invoked).
- **Verifier result caching.** The verifier runs once per `(buffer
  content, root_type)` per query. The cache key is a 128-bit hash
  derived from `(len, blake3_of_first_4KiB, blake3_of_last_4KiB)` of the
  detoasted payload — *not* the raw `Datum` pointer, since varlena/TOAST
  pointers are not stable across rows in a SETOF iteration. Cache scope
  is the current SQL function call; entries are dropped when the
  per-statement memory context is reset.
- All Rust panics are caught at the `#[pg_extern]` boundary by pgrx and
  turned into Postgres `ERROR`s; we never let a panic unwind across the
  FFI boundary.
- The `schemas` table is bytea, so SQL injection is structurally
  impossible against the schema; the *query string* is parsed by our own
  parser into a typed AST before touching reflection.

### GUCs and `GucContext` assignments

GUC governance is part of the safety boundary: a bound that any session
can raise is not a bound. The matrix below assigns each GUC to a
`GucContext` deliberately, with `SUSET` (superuser-only) for everything
that protects the backend from untrusted input, and `USERSET` reserved
for knobs that affect only the calling session's own resource use or
diagnostics.

| GUC | Default | `GucContext` | Rationale |
| --- | --- | --- | --- |
| `pg_flatbuffers.max_depth` | 64 | `SUSET` | Verifier DoS bound; bypass would defeat §10. |
| `pg_flatbuffers.max_tables` | 1_000_000 | `SUSET` | Verifier DoS bound. |
| `pg_flatbuffers.max_apparent_size_mb` | 64 | `SUSET` | Verifier DoS bound; also applied to `from_json` build path. |
| `pg_flatbuffers.max_build_depth` | 32 | `SUSET` | `from_json` JSON-nesting bound; symmetric to `max_depth`. |
| `pg_flatbuffers.max_query_length` | 4096 | `SUSET` | Path-parser input bound. |
| `pg_flatbuffers.max_path_depth` | 256 | `SUSET` | Path-parser depth bound. |
| `pg_flatbuffers.schema_cache_mb` | 16 | `SIGHUP` | Per-backend LRU size; takes effect on reload, applies to new backends. |
| `pg_flatbuffers.strict` | `on` | `USERSET` | Per-session: verifier failure → `ERROR` (default) vs. `NULL`. Affects only the calling session's results. |
| `pg_flatbuffers.key_lookup_strict` | `on` | `USERSET` | Per-session: `on` (default) bisects `(key)`-annotated vectors under the FlatBuffers key-sorted contract (matches the upstream `LookupByKey` accessor); `off` falls back to a linear scan that is correct on any vector but O(n). Read-side only — neither weakens the verifier nor relaxes any DoS bound; the on-mode worst case against a writer that violated the contract is a silent miss, never a buffer over-read. |
| `pg_flatbuffers.identifier_mismatch` | `warning` | `USERSET` | Per-session diagnostic verbosity for `file_identifier` mismatches; values `error \| warning \| silent`. |
| `pg_flatbuffers.from_json_unknown` | `error` | `USERSET` | `from_json` policy for unknown JSON keys; values `error \| ignore`. |
| `pg_flatbuffers.fill_scalar_defaults` | `on` | `USERSET` | When `on` (default), an absent scalar table field reads back as its schema default (matches the FlatBuffers reader API). When `off`, it surfaces as SQL `NULL` instead, so callers can distinguish "writer set field to 0" from "writer never set field" (postgres-protobuf-style presence). See §4.3 "Defaults and absence". |

All `SUSET` GUCs require superuser to change, so an unprivileged caller
cannot lift a bound. The `USERSET` GUCs (`strict`, `key_lookup_strict`,
`fill_scalar_defaults`, `identifier_mismatch`, `from_json_unknown`)
control opt-in lenient semantics, read-side interpretation choices, and
per-session diagnostic level only — they cannot weaken the verifier or
the parser, and the on-mode of `key_lookup_strict` against a
contract-violating writer is a silent miss, not a buffer over-read.

A regression test in §13 asserts that a non-superuser `SET` of any
`SUSET` GUC fails with `ERROR: permission denied`.

## 11. Postgres compatibility

Primary target for v0.1: **PG 18 only**. This is the version we develop
against, run in CI on every commit, and ship binary packages for.

PG 17 is a planned target for v0.2 (see §15) once the v0.1 surface is
stable; the design carries no PG-18-only assumptions, so adding PG 17
should reduce to enabling the `pg17` pgrx feature flag and a CI matrix
row. Older majors (PG 14–16) are not a goal: they may continue to work
incidentally as long as pgrx supports them and no PG 18-only APIs are
used, but we do not test against them and bug reports specific to those
versions will be closed as out of scope. PG 13 and earlier are explicitly
unsupported.

We will run the pgrx test harness against PG 18 in CI
(`cargo pgrx test pg18 --package pg_flatbuffers`).

## 12. Build and packaging

- `cargo pgrx package --package pg_flatbuffers` produces an installable
  tree (`lib/`, `extension/`) that can be tar'd up, matching what
  postgres-protobuf does manually.
- Dockerfile pinned to a pgrx-supported toolchain, used by CI to publish
  binaries for at least Debian bookworm and Ubuntu 24.04.
- `make install` wraps `cargo pgrx install --release --package pg_flatbuffers`
  for users who prefer the conventional invocation.
- The workspace `target/` directory is shared across crates; reproducible
  release builds set `CARGO_TARGET_DIR` explicitly in the Docker image.

## 13. Testing strategy

1. **Unit tests** in Rust for the path parser and the executor against
   hand-built `flatbuffers::Table` instances.
2. **Reflection-driven tests** against fixtures: `tests/fixtures/*.fbs` →
   `flatc -b --schema` + `flatc -b` for sample data → round-trip through
   every public SQL function.
3. **Generative tests** using a small `proptest` strategy that builds
   arbitrary buffers from a simple schema and asserts
   `from_json(to_json(b)) == b` (modulo field ordering).
4. **pgrx integration tests** that boot a real Postgres, install the
   extension, register a schema, and run the documented examples
   end-to-end.
5. **Fuzz target** (`cargo +nightly fuzz`) on the path parser and on the
   verifier+executor combo with a tiny corpus of valid buffers as seed.
6. **Privilege regression tests** that assert the safety boundary holds:
   - An unprivileged role's `INSERT` into `flatbuffers_schemas` fails
     with `ERROR: permission denied` (see §4.1).
   - A non-superuser's `SET pg_flatbuffers.max_*` on any `SUSET` GUC
     fails with `ERROR: permission denied` (see §10).
   - `flatbuffers_validate_schema` rejects a malformed `.bfbs` blob at
     INSERT time with no row reaching the cache.

## 14. Open questions

- Cost-model hints (`SUPPORT` functions, `ROWS`, `COST`) — v0.1 ships
  static `ROWS=10` and a payload-size-proportional `COST` annotation;
  a real `SUPPORT` function lands in v0.2 once we have measurement.
- Whether to expose typed accessor families
  (`flatbuffers_query_int`/`_double`/`_bool`) before v0.3, given they
  also unlock indexable extraction (§9). Defer until usage signal exists.
- Whether to ship a `flatbuffers_compose(table_name, jsonb) -> bytea`
  helper distinct from `from_json` for the common case of building from
  a Postgres-native `jsonb` accumulated by other operators.

## 15. Roadmap

- **v0.1** — schema catalog, `flatbuffers_query{,_array,_multi}`, JSON
  round-trip, verifier integration, PG 18, source builds.
- **v0.2** — add PG 17 to the CI matrix and binary releases;
  `IMMUTABLE` field-id-only accessors (`flatbuffers_get_*`) enabling
  functional indexes; binary release packaging.
- **v0.3** — typed accessors and operator-class sketches; richer error
  messages with path location.
- **Later** — exploring partial deserialization for very large vectors,
  optional in-place mutation API, native `composite type` returns.
