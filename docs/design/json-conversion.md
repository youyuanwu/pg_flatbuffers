# JSON conversion

`flatbuffers_to_json` / `flatbuffers_to_json_text` and `flatbuffers_from_json`
/ `flatbuffers_from_json_text` are reflection-driven and never shell out to
`flatc`.

- **`to_json`** walks the same executor primitives but emits a
  `serde_json::Value` instead of stringifying leaves.
  Implementation: [`src/to_json.rs`](../../crates/pg_flatbuffers/src/to_json.rs);
  SQL wrappers in
  [`src/functions/to_json.rs`](../../crates/pg_flatbuffers/src/functions/to_json.rs).
- **`from_json`** uses `flatbuffers::FlatBufferBuilder` to construct a buffer
  bottom-up, driven by the schema's field ordering.
  Implementation: [`src/from_json.rs`](../../crates/pg_flatbuffers/src/from_json.rs);
  SQL wrappers in
  [`src/functions/from_json.rs`](../../crates/pg_flatbuffers/src/functions/from_json.rs).

Both directions are bounded by the same GUCs as the read path —
`max_apparent_size_mb` caps the resulting buffer size on the build side, and
`max_depth` caps JSON nesting on both sides (see [safety.md](safety.md#gucs)).

## Per-type JSON encoding

| FlatBuffers type | JSON shape |
| --- | --- |
| numeric scalars (`int8`…`int64`, `uint8`…`uint64`, `float`, `double`) | JSON number |
| `bool` | JSON boolean |
| enum | JSON string (member name); numeric on round-trip if the value is unknown |
| `string` | JSON string (UTF-8) |
| `[ubyte]` / `[u8]` | JSON string, base64-encoded by default; lowercase hex when the field carries the `(hex)` attribute |
| `[T]` for other `T` | JSON array |
| table / struct | JSON object |
| vector of `(key)`-annotated tables | JSON object keyed by the `(key)` field's stringified value (`from_json` also accepts a JSON array, and sorts the result by the key field per the FlatBuffers contract) |
| union | flatc-style sibling field pair: `<name>_type` (string) + `<name>` (variant value); NONE emits only the type field |

## `from_json` policies

Implemented in
[`from_json.rs::FromJsonError`](../../crates/pg_flatbuffers/src/from_json.rs):

| Condition | Behavior |
| --- | --- |
| Missing `required` field | `ERROR` (`FromJsonError::MissingRequiredField`) |
| Unknown JSON key | `ERROR` by default; set [`pg_flatbuffers.from_json_unknown`](safety.md#gucs) `= off` to drop silently for forward-compat workflows |
| Type mismatch | `ERROR` (`FromJsonError::TypeMismatch`) |
| Integer out of range for declared scalar width | `ERROR` (`FromJsonError::IntegerOutOfRange`) |
| Unknown enum-member name | `ERROR` (`FromJsonError::UnknownEnumName`) |
| Invalid base64 / hex on `[ubyte]` | `ERROR` (`InvalidBase64` / `InvalidHex`) |
| Resulting buffer would exceed `max_apparent_size_mb` | `ERROR` (`FromJsonError::OutputTooLarge`) — buffer discarded |
| Nesting exceeds `max_depth` | `ERROR` (`FromJsonError::NestingTooDeep`) |

JSON conversion is the safest entry point for untrusted *bytes* (a single
linear pass via reflection without exposing the raw query language). The
bounded-build policy above makes it equally safe for untrusted *JSON*.

## Failure semantics vs. `strict`

JSON conversion functions **always raise `ERROR`** on verifier failure,
regardless of `pg_flatbuffers.strict`. A partially-valid document has no
defensible JSON encoding, so silently returning NULL would hide corruption.
This is enforced in
[`functions/to_json.rs`](../../crates/pg_flatbuffers/src/functions/to_json.rs)
and
[`functions/from_json.rs`](../../crates/pg_flatbuffers/src/functions/from_json.rs).

## Carve-outs and limitations

- **Non-finite floats** (`NaN`, `±Infinity`) raise `ERROR` on the `to_json`
  side rather than serializing as a lossy JSON string. They have no native
  JSON representation, and silently coercing them would break round-trip
  through `from_json`. Surfaced as `ToJsonError::NonFiniteFloat`.

- **Struct sizes / alignments not in the v0.1 dispatch table.** The
  `(size, align)` table in
  [`from_json.rs`](../../crates/pg_flatbuffers/src/from_json.rs) covers all
  common combinations (align ∈ {1, 2, 4, 8}, size up to 256 bytes) for
  inline placement, out-of-line union values, *and* vector elements.
  Schemas with unusual `bytesize` × `minalign` pairs (e.g. `(96, 4)`) raise
  a clear error pointing at the field; extending the table is a one-line
  addition.

- **Vector-of-union schemas** are rejected at schema registration time
  ([`verify.rs::reject_unsupported_schema_features`](../../crates/pg_flatbuffers/src/verify.rs)),
  so they never reach either JSON path. Tracked for v0.2 in
  [roadmap.md](roadmap.md).
