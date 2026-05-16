//! `Step::MapKey` and `Step::MapKeys` dispatch — bisect or linear-scan
//! a `(key)`-annotated vector of tables (design §7.2 step 4 / step 5).

use super::util::base_type_name;
use super::vector::lookup_vector_element_object;
use super::walk::walk_table;
use super::{ExecuteError, ExecuteOptions};
use crate::query::ast::{MapKey, Step};
use flatbuffers::{ForwardsUOffset, Table, Vector};
use flatbuffers_reflection::get_any_field_string;
use flatbuffers_reflection::reflection::{BaseType, Field, Object, Schema};
use std::cmp::Ordering;

/// Resolve the `(key)`-annotated field on `child_object` (the table
/// type of vector elements). FlatBuffers guarantees at most one
/// `(key)` field per table; we still error explicitly if zero are
/// found, so the caller can surface a "not a keyed vector" message
/// rather than silently returning no match.
fn lookup_keyed_field<'a>(child_object: &'a Object) -> Result<Field<'a>, ExecuteError> {
    let fields = child_object.fields();
    let mut found: Option<Field<'a>> = None;
    for i in 0..fields.len() {
        let f = fields.get(i);
        if f.key() {
            // flatc rejects multi-key tables at schema-compile time,
            // so we only need to defend against malformed `.bfbs`.
            if found.is_some() {
                return Err(ExecuteError::Internal(format!(
                    "table {:?} has more than one (key)-annotated field",
                    child_object.name()
                )));
            }
            found = Some(f);
        }
    }
    found.ok_or_else(|| ExecuteError::UnsupportedType {
        field: child_object.name().to_string(),
        type_name: "vector of tables with no (key)-annotated field",
    })
}

/// Compare the `(key)`-annotated field on `elem` against the AST
/// literal `key`. Returns `Ok(true)` on a match, `Ok(false)` on a
/// non-match (including type-mismatch combinations like a textual
/// key against an integer field that fails to parse), and `Err` for
/// genuinely unsupported keyed-field types.
///
/// This slice supports `(key)` fields whose type is `String` or any
/// signed/unsigned integer width. `Float` / `Double` / `Bool` keyed
/// fields are deferred — they're permitted by the FlatBuffers spec
/// but vanishingly rare in practice and would need additional
/// stringification rules to round-trip cleanly.
fn key_matches(
    elem: &Table,
    keyed_field: &Field,
    schema: &Schema,
    key: &MapKey,
) -> Result<bool, ExecuteError> {
    let kbase = keyed_field.type_().base_type();

    // For both string and integer keyed fields the `(key)` field is
    // by convention `required` (flatc enforces this), so the
    // schema-default fallback in `get_any_field_string` never fires
    // for a well-formed buffer.
    //
    // SAFETY: the buffer was verified by `execute`, and `keyed_field`
    // came from the schema's reflected `Object.fields()`.
    let actual = unsafe { get_any_field_string(elem, keyed_field, schema) };

    match (kbase, key) {
        (BaseType::String, MapKey::Text(s)) => Ok(actual == *s),
        // A textual key against a string field that the AST already
        // promoted to `Int` is a non-match, not an error: the user
        // asked for a numeric value where the schema demands a string.
        (BaseType::String, MapKey::Int(_)) => Ok(false),

        // Integer keyed field. The parser only ever emits
        // `MapKey::Text` (see `ast.rs`), so the common path is text
        // → int parse. We compare numerically (rather than as
        // formatted decimals) to dodge leading-zero / sign-formatting
        // ambiguities — `[042]` and `[42]` both match a stored 42.
        (b, MapKey::Text(s)) if is_integer_base(b) => {
            let want: i64 = match s.parse::<i64>() {
                Ok(n) => n,
                // Non-numeric key against an integer field: the user
                // asked for a key the field cannot hold. Treat as
                // "no match" rather than an error so a typo at the
                // SQL site surfaces as `NULL` (consistent with the
                // OOB-index short-circuit) rather than aborting the
                // whole statement.
                Err(_) => return Ok(false),
            };
            // ULong values above i64::MAX would underflow this parse
            // and silently no-match; that's a known v0.1 limitation
            // documented at the call site.
            Ok(actual.parse::<i64>().map(|v| v == want).unwrap_or(false))
        }
        (b, MapKey::Int(n)) if is_integer_base(b) => {
            Ok(actual.parse::<i64>().map(|v| v == *n).unwrap_or(false))
        }

        // Float / Double / Bool / nested table / vector / union /
        // array keyed fields are out of scope for this slice.
        _ => Err(ExecuteError::UnsupportedType {
            field: keyed_field.name().to_string(),
            type_name: base_type_name(kbase),
        }),
    }
}

/// All integer-width FlatBuffers scalars (signed and unsigned). The
/// `(key)` annotation supports any of them per the FlatBuffers spec.
fn is_integer_base(b: BaseType) -> bool {
    matches!(
        b,
        BaseType::Byte
            | BaseType::UByte
            | BaseType::Short
            | BaseType::UShort
            | BaseType::Int
            | BaseType::UInt
            | BaseType::Long
            | BaseType::ULong
    )
}

/// AST `MapKey` projected onto the keyed field's natural comparison
/// type. Built once per lookup by [`compile_key`] and consumed by
/// [`compare_actual_to_compiled`] inside the binary-search loop.
pub(super) enum CompiledKey<'a> {
    /// String-keyed field: compare actual bytes against the key
    /// lexicographically. Borrowed from the AST so no allocation
    /// occurs in the hot loop.
    Text(&'a str),
    /// Integer-keyed field: compare actual (parsed from the field's
    /// formatted decimal) against the key numerically. Numeric
    /// rather than lexicographic so `[42]` matches stored 42
    /// regardless of leading-zero / sign-formatting differences and
    /// so the bisect's comparator agrees with the writer's natural
    /// sort order (flatc's `CreateVectorOfSortedTables` sorts
    /// numerically for integer keyed fields).
    Int(i64),
}

/// Project the AST `key` onto the keyed field's natural comparison
/// type, or short-circuit to a no-match sentinel for combinations
/// that cannot possibly match any well-formed element.
///
/// Returns:
/// - `Ok(Some(CompiledKey::...))` for a structurally-valid lookup
///   (string key vs. string field, int / parseable-int-text key
///   vs. integer field).
/// - `Ok(None)` for a structurally-impossible lookup (int key
///   vs. string field, non-parseable text vs. integer field): the
///   binary search short-circuits to `vec![None]` without reading
///   any element, mirroring `key_matches`'s `Ok(false)` arm so the
///   linear-scan and binary-search paths agree on misses.
/// - `Err(UnsupportedType)` for a keyed field whose type this
///   slice doesn't handle (Float / Double / Bool / nested table /
///   vector / union / array). Same contract as `key_matches`.
pub(super) fn compile_key<'a>(
    keyed_field: &Field,
    key: &'a MapKey,
) -> Result<Option<CompiledKey<'a>>, ExecuteError> {
    let kbase = keyed_field.type_().base_type();
    match (kbase, key) {
        (BaseType::String, MapKey::Text(s)) => Ok(Some(CompiledKey::Text(s))),
        (BaseType::String, MapKey::Int(_)) => Ok(None),
        (b, MapKey::Text(s)) if is_integer_base(b) => {
            Ok(s.parse::<i64>().ok().map(CompiledKey::Int))
        }
        (b, MapKey::Int(n)) if is_integer_base(b) => Ok(Some(CompiledKey::Int(*n))),
        _ => Err(ExecuteError::UnsupportedType {
            field: keyed_field.name().to_string(),
            type_name: base_type_name(kbase),
        }),
    }
}

/// Compare an element's stringified `(key)` value (as produced by
/// `get_any_field_string`) against the [`CompiledKey`] from
/// [`compile_key`].
///
/// `Ordering::Less` means `actual < compiled` (the bisect should
/// move *right*, lo := mid + 1); `Greater` means `actual > compiled`
/// (move *left*, hi := mid).
///
/// For integer keys we first parse `actual` as `i64`. A well-formed
/// verified buffer with an `is_integer_base` keyed field always
/// stringifies to a value that round-trips through `parse::<i64>`,
/// with one corner: `ULong` values above `i64::MAX` overflow. That's
/// a known v0.1 limitation; treating the parse failure as
/// `Ordering::Less` keeps the bisect deterministic (the affected
/// element is sorted as if it were the smallest possible value),
/// which only misorders entries within the `> i64::MAX` band — a
/// band the bisect can't represent in its `i64` comparand either.
pub(super) fn compare_actual_to_compiled(actual: &str, compiled: &CompiledKey<'_>) -> Ordering {
    match compiled {
        CompiledKey::Text(s) => actual.cmp(*s),
        CompiledKey::Int(n) => match actual.parse::<i64>() {
            Ok(v) => v.cmp(n),
            Err(_) => Ordering::Less,
        },
    }
}

/// `field[abc]` — find the entry whose `(key)`-annotated field
/// equals `key`. Strategy is driven by
/// [`ExecuteOptions::key_lookup_strict`] (Postgres-side knob:
/// `pg_flatbuffers.key_lookup_strict`):
///
/// - `true` (default): binary search under the FlatBuffers `(key)`-
///   sorted contract (matches the upstream `LookupByKey` accessor's
///   semantics, design §7.2 step 4). O(log n) lookups, but a writer
///   that violated the sort contract will see *silent misses* on
///   keys whose true position is past a comparison the bisect made
///   the wrong call on. Never a buffer over-read — the verifier
///   already bounds reads to vetted memory.
/// - `false`: linear scan, first match in wire order. Correct on
///   any vector regardless of sortedness, at O(n) per lookup
///   (design §10 escape hatch).
///
/// On miss / absent / empty vector / structurally-impossible key
/// shape (e.g. `[42]` against a string-keyed field), returns
/// `vec![None]` — same short-circuit as `Step::Index` OOB and
/// `key_matches`'s linear-scan no-match path.
pub(super) fn walk_vector_at_map_key(
    table: &Table,
    field: &Field,
    schema: &Schema,
    key: &MapKey,
    tail: &[Step],
    element_base_type: BaseType,
    options: &ExecuteOptions,
) -> Result<Vec<Option<String>>, ExecuteError> {
    let field_name = field.name();

    // Map-key lookup is only defined over vectors of `(key)`-
    // annotated tables. Scalar / string vectors don't have a "key"
    // distinct from the element value, so refuse rather than
    // silently treat the literal as an index.
    if element_base_type != BaseType::Obj {
        return Err(ExecuteError::UnsupportedType {
            field: field_name.to_string(),
            type_name: base_type_name(element_base_type),
        });
    }

    let child_object = lookup_vector_element_object(field, schema)?;
    if child_object.is_struct() {
        // Struct elements have no `(key)` annotation (the
        // attribute is only valid on table fields), so map-key
        // lookup is meaningless here. Use `[i]` / `[*]` instead.
        return Err(ExecuteError::UnsupportedType {
            field: field_name.to_string(),
            type_name: "vector-of-struct element ([key] not supported — use [i] / [*])",
        });
    }

    let keyed_field = lookup_keyed_field(&child_object)?;

    if tail.is_empty() {
        // `items[abc]` lands at a sub-table value with no v0.1
        // textual form. Same rationale as `items[3]` /
        // `items[*]` (no `.field` continuation).
        return Err(ExecuteError::UnsupportedType {
            field: field_name.to_string(),
            type_name: "vector-of-table element (sub-table at leaf)",
        });
    }

    // SAFETY: see `execute`. Same `Vector<ForwardsUOffset<Table>>`
    // shape used by `walk_vector_at_index` / `walk_vector_all`.
    let vec_opt = unsafe {
        table.get::<ForwardsUOffset<Vector<ForwardsUOffset<Table>>>>(field.offset(), None)
    };
    let vec = match vec_opt {
        Some(v) => v,
        None => return Ok(vec![None]),
    };

    if options.key_lookup_strict {
        // Binary search under the FlatBuffers (key)-sorted contract.
        // `compile_key` projects the AST `MapKey` to the keyed
        // field's natural comparison type once up front; a
        // structurally-impossible key (e.g. `[42]` against a string
        // field, or `[abc]` against an int field) compiles to
        // `None`, which short-circuits to the no-match sentinel
        // before any element is read. Returning `Err` on a
        // genuinely-unsupported keyed field type (Float / Double /
        // Bool / ...) preserves the existing `key_matches` contract.
        let compiled = match compile_key(&keyed_field, key)? {
            Some(c) => c,
            None => return Ok(vec![None]),
        };

        let mut lo = 0usize;
        let mut hi = vec.len();
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let elem = vec.get(mid);
            // SAFETY: see `key_matches`. The keyed field came from
            // the schema's reflected `Object.fields()`, and the
            // buffer was verified.
            let actual = unsafe { get_any_field_string(&elem, &keyed_field, schema) };
            match compare_actual_to_compiled(&actual, &compiled) {
                Ordering::Equal => {
                    return walk_table(&elem, &child_object, schema, tail, options);
                }
                Ordering::Less => lo = mid + 1,
                Ordering::Greater => hi = mid,
            }
        }
        return Ok(vec![None]);
    }

    // key_lookup_strict = off: linear scan that's correct on any
    // vector regardless of sortedness.
    for elem in vec.iter() {
        if key_matches(&elem, &keyed_field, schema, key)? {
            return walk_table(&elem, &child_object, schema, tail, options);
        }
    }
    // No element matched — short-circuit, same as out-of-range index.
    Ok(vec![None])
}

/// `field|keys` — fan out the `(key)`-annotated field of every
/// element of a vector of tables, in wire-format order. The keys
/// **are** the leaves: `tail` must be empty, since there is nothing
/// to descend into past a stringified key.
///
/// Symmetric to [`walk_vector_all`] but constrained to the keyed
/// field rather than recursing with arbitrary `tail`. Absent /
/// empty vector → `vec![]` (no items to fan out, matching the
/// `[*]` semantics).
pub(super) fn walk_vector_map_keys(
    table: &Table,
    field: &Field,
    schema: &Schema,
    tail: &[Step],
    element_base_type: BaseType,
) -> Result<Vec<Option<String>>, ExecuteError> {
    let field_name = field.name();

    if element_base_type != BaseType::Obj {
        return Err(ExecuteError::UnsupportedType {
            field: field_name.to_string(),
            type_name: base_type_name(element_base_type),
        });
    }

    let child_object = lookup_vector_element_object(field, schema)?;
    if child_object.is_struct() {
        // Same reasoning as `walk_vector_at_map_key`: structs
        // can't carry a `(key)` annotation, so `|keys` is
        // meaningless on a vector of struct elements.
        return Err(ExecuteError::UnsupportedType {
            field: field_name.to_string(),
            type_name:
                "vector-of-struct element (|keys not supported — use [*] with a struct field)",
        });
    }

    let keyed_field = lookup_keyed_field(&child_object)?;

    if !tail.is_empty() {
        // The parser allows `items|keys.foo` (see `parser.rs`); the
        // executor rejects it here because `|keys` already produced
        // a leaf — there's nothing to descend into.
        return Err(ExecuteError::UnsupportedType {
            field: field_name.to_string(),
            type_name: "trailing path step after `|keys` (keys are terminal leaves)",
        });
    }

    // SAFETY: see `execute`. Same `Vector<ForwardsUOffset<Table>>`
    // shape as `walk_vector_at_map_key`.
    let vec_opt = unsafe {
        table.get::<ForwardsUOffset<Vector<ForwardsUOffset<Table>>>>(field.offset(), None)
    };
    let vec = match vec_opt {
        Some(v) => v,
        None => return Ok(vec![]),
    };

    // Pre-size to vec.len(); each element contributes exactly one
    // key entry (the keyed field is `(key)` and conventionally
    // required, so it's always present).
    let mut out: Vec<Option<String>> = Vec::with_capacity(vec.len());
    for elem in vec.iter() {
        // SAFETY: see `key_matches`. The keyed field came from the
        // schema's reflected `Object.fields()`.
        let key = unsafe { get_any_field_string(&elem, &keyed_field, schema) };
        out.push(Some(key));
    }
    Ok(out)
}
