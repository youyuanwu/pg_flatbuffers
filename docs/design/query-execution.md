# Query execution

The executor is the heart of the extension. Entry point:
[`query::execute_with_options`](../../crates/pg_flatbuffers/src/query/executor/mod.rs).

## Parse

[`query/parser.rs`](../../crates/pg_flatbuffers/src/query/parser.rs) is a
hand-written parser (no parser-combinator crate — grammar is small). It
consumes the path syntax described in
[sql-surface.md → Query language](sql-surface.md#query-language) and
produces the AST in
[`query/ast.rs`](../../crates/pg_flatbuffers/src/query/ast.rs):

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
    UnionType,                     // |type discriminator-name leaf
}
```

Parser-time bounds (`pg_flatbuffers.max_query_length`,
`pg_flatbuffers.max_path_depth`) are SUSET; see [safety.md](safety.md#gucs).

## Execute

[`query/executor/`](../../crates/pg_flatbuffers/src/query/executor/) is split
by step kind:

| Step kind | Implementation |
| --- | --- |
| Table descent | [`walk.rs`](../../crates/pg_flatbuffers/src/query/executor/walk.rs) |
| Struct descent | [`struct_.rs`](../../crates/pg_flatbuffers/src/query/executor/struct_.rs) |
| Vector index / fan-out | [`vector.rs`](../../crates/pg_flatbuffers/src/query/executor/vector.rs) |
| Union dispatch | [`union.rs`](../../crates/pg_flatbuffers/src/query/executor/union.rs) |
| `(key)` lookup | [`map_key.rs`](../../crates/pg_flatbuffers/src/query/executor/map_key.rs) |
| Leaf stringification | [`leaf.rs`](../../crates/pg_flatbuffers/src/query/executor/leaf.rs), [`pg_text.rs`](../../crates/pg_flatbuffers/src/query/executor/pg_text.rs) |

Execution state during traversal is `(current_table_or_struct: &Object,
cursor: AnyTable)`. For each step:

1. **Field by name/id** → look up `Field` on the current `Object` via the
   schema's pre-built `name → Field` hash. For unions, read the discriminator
   slot and route to the active member.
2. **Index** → read the `Vector<T>`, bounds-check, descend. Returns `NULL`
   on out-of-range index; subsequent path steps short-circuit.
3. **All** → fan out: produce a stream of leaves rather than a single one.
   **Emission order is wire-format order**: ascending vector index for `[*]`,
   depth-first left-to-right for nested `[*]`. Stable and observable via
   `WITH ORDINALITY` in `query_multi`.
4. **MapKey** → binary-search (default) or linear-scan
   ([`map_key.rs`](../../crates/pg_flatbuffers/src/query/executor/map_key.rs))
   for the entry whose `(key)`-annotated field equals the path literal.
   Strategy is driven by
   [`pg_flatbuffers.key_lookup_strict`](safety.md#gucs).
5. **MapKeys** → fan out the key field of every entry, in wire order.

### Output assembly

- `flatbuffers_query` consumes the first leaf and returns it
  ([`functions/query.rs`](../../crates/pg_flatbuffers/src/functions/query.rs)).
- `flatbuffers_query_array` collects into `Vec<Option<String>>` → `text[]`
  preserving wire order; absent values are skipped
  ([`functions/query_array.rs`](../../crates/pg_flatbuffers/src/functions/query_array.rs)).
- `flatbuffers_query_multi` returns a `SetOfIterator<String>` from pgrx in
  wire-format order, suitable for `WITH ORDINALITY`
  ([`functions/query_multi.rs`](../../crates/pg_flatbuffers/src/functions/query_multi.rs)).

Leaf scalars are stringified using the same formatters Postgres itself uses
for `float4`/`float8`
([`pg_text.rs`](../../crates/pg_flatbuffers/src/query/executor/pg_text.rs))
so round-trip stability matches Postgres's own `text` cast — important for
parity with the protobuf extension.

## Random-access advantage

Because FlatBuffers is random-access, scanning for `Order:items[3].sku`
involves: deref root vtable → deref `items` offset → read vtable[3] → deref
`sku` offset → read `bytes`. There is no need to walk the entire buffer the
way protobuf must. Single-field lookups against large payloads benefit
proportionally.
