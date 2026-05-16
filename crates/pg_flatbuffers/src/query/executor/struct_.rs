//! Inline-struct dispatch — `walk_struct` for table-embedded structs
//! (and recursive nested structs), plus `walk_array` for fixed-size
//! arrays inside structs (`BaseType::Array`).

use super::util::base_type_name;
use super::util::find_field;
use super::ExecuteError;
use crate::query::ast::Step;
use flatbuffers::read_scalar_at;
use flatbuffers_reflection::reflection::{BaseType, Field, Object, Schema};

/// Defensive accessor for `Object::bytesize()`. Reflection stores
/// it as `i32`; flatc never emits negative values for structs (and
/// emits 0 for tables), but a malformed `.bfbs` could in principle.
pub(super) fn struct_bytesize(object: &Object) -> Result<usize, ExecuteError> {
    let raw = object.bytesize();
    if raw <= 0 {
        return Err(ExecuteError::Internal(format!(
            "struct {:?} has non-positive bytesize ({raw})",
            object.name()
        )));
    }
    Ok(usize::try_from(raw).expect("non-negative after the explicit ≤ 0 guard above"))
}

/// Byte size of a scalar `BaseType` when stored inline (in a
/// struct field, a fixed-size array element, or a vector element).
/// Returns `None` for non-scalar types (`Obj`, `String`, `Vector`,
/// `Vector64`, `Union`, `Array`, `None`).
fn scalar_byte_size(base_type: BaseType) -> Option<usize> {
    Some(match base_type {
        BaseType::Bool | BaseType::Byte | BaseType::UByte | BaseType::UType => 1,
        BaseType::Short | BaseType::UShort => 2,
        BaseType::Int | BaseType::UInt | BaseType::Float => 4,
        BaseType::Long | BaseType::ULong | BaseType::Double => 8,
        _ => return None,
    })
}

/// Walk an inline fixed-size struct at byte offset `struct_loc`
/// inside `buf`. Mirrors [`super::walk::walk_table`] but without vtables: every
/// struct field is unconditionally present and lives at
/// `struct_loc + field.offset()` (the reflection metadata stores
/// byte offsets *within the struct* for struct fields, not
/// vtable slots).
///
/// Structs may contain only scalars, nested structs, and fixed-size
/// arrays — no vectors, strings, sub-tables, or unions — so all the
/// vector / map-key step variants are rejected up-front. Fixed-size
/// arrays (`BaseType::Array`) are deferred to a future slice.
///
/// Empty step list at a struct is a hard error: structs have no
/// v0.1 textual leaf form (each field is queryable individually,
/// but the struct as a whole would need a JSON-shaped output that
/// belongs to the §8 round-trip slice).
pub(super) fn walk_struct(
    buf: &[u8],
    struct_loc: usize,
    object: &Object,
    schema: &Schema,
    steps: &[Step],
) -> Result<Vec<Option<String>>, ExecuteError> {
    let object_name = object.name();
    let (head, tail) = match steps.split_first() {
        Some(p) => p,
        // Caller (`walk_table`) already short-circuits empty `tail`
        // before handing off to us; this is purely defensive against
        // future direct callers.
        None => {
            return Err(ExecuteError::UnsupportedType {
                field: object_name.to_string(),
                type_name: "struct (no v0.1 textual leaf form — descend with `.field`)",
            });
        }
    };

    let field_ref = match head {
        Step::Field(fr) => fr,
        // Structs hold no vectors, so all of these are static
        // type-system mismatches at this position.
        Step::Index(_) => {
            return Err(ExecuteError::UnsupportedType {
                field: object_name.to_string(),
                type_name: "[index] inside a struct (structs hold no vectors)",
            });
        }
        Step::All => {
            return Err(ExecuteError::UnsupportedType {
                field: object_name.to_string(),
                type_name: "[*] inside a struct (structs hold no vectors)",
            });
        }
        Step::MapKey(_) => {
            return Err(ExecuteError::UnsupportedType {
                field: object_name.to_string(),
                type_name: "[map-key] inside a struct (structs hold no vectors)",
            });
        }
        Step::MapKeys => {
            return Err(ExecuteError::UnsupportedType {
                field: object_name.to_string(),
                type_name: "|keys inside a struct (structs hold no vectors)",
            });
        }
        Step::UnionType => {
            return Err(ExecuteError::UnsupportedType {
                field: object_name.to_string(),
                type_name: "|type inside a struct (structs hold no unions)",
            });
        }
    };

    let field = find_field(object, field_ref)?;
    let field_name = field.name();
    let field_offset = usize::from(field.offset());
    let base_type = field.type_().base_type();

    match base_type {
        BaseType::Obj => {
            let child_index = field.type_().index();
            if child_index < 0 {
                return Err(ExecuteError::Internal(format!(
                    "schema struct field {field_name:?} has BaseType::Obj but \
                     negative object index ({child_index})"
                )));
            }
            let child_object = schema.objects().get(
                usize::try_from(child_index)
                    .expect("non-negative after the explicit < 0 guard above"),
            );
            // Defensive: an `Obj` field inside a struct must point
            // at another struct (flatc rejects non-struct nesting at
            // compile time). If a malformed `.bfbs` slips a table
            // index in here, surface it as an internal error rather
            // than silently calling `walk_struct` on a table.
            if !child_object.is_struct() {
                return Err(ExecuteError::Internal(format!(
                    "schema struct {object_name:?} has Obj field {field_name:?} \
                     pointing at non-struct object {:?}",
                    child_object.name()
                )));
            }
            // Empty `tail` here means the user stopped at a nested
            // struct field (e.g. `Holder:b.inner`); `walk_struct`'s
            // top-of-function check raises the "no v0.1 textual
            // leaf form" error.
            walk_struct(buf, struct_loc + field_offset, &child_object, schema, tail)
        }
        BaseType::Array => walk_array(buf, struct_loc + field_offset, &field, schema, tail),
        // Scalar field. Empty `tail` is the leaf path; non-empty
        // `tail` is "can't descend into a scalar" (mirrors the
        // analogous arm in `walk_table` via `read_leaf`).
        other => {
            if tail.is_empty() {
                read_struct_scalar_leaf(buf, struct_loc + field_offset, &field, other)
                    .map(|opt| vec![opt])
            } else {
                Err(ExecuteError::UnsupportedType {
                    field: field_name.to_string(),
                    type_name: base_type_name(other),
                })
            }
        }
    }
}

/// Read and stringify a single scalar at `loc` inside `buf`,
/// dispatching on `base_type`. Mirrors the scalar-arm formatting
/// used by [`read_leaf`] / [`read_vector_element`] (integers via
/// `Display`, floats via `Display`, bool rendered as `0`/`1`) so a
/// value rendered out of a struct field matches the same value
/// rendered out of a table or vector element.
fn read_struct_scalar_leaf(
    buf: &[u8],
    loc: usize,
    field: &Field,
    base_type: BaseType,
) -> Result<Option<String>, ExecuteError> {
    // Helper: the buffer was verified, so `loc + size_of::<T>()` is
    // in bounds; `read_scalar_at` is `unsafe` solely on that bound.
    macro_rules! read {
        ($t:ty) => {{
            // SAFETY: see `execute`. Verifier validated that the
            // struct's enclosing field has the schema-declared
            // bytesize, which is the sum of its field offsets +
            // their scalar sizes. `read_scalar_at` already performs
            // little-endian-to-host conversion internally.
            let v: $t = unsafe { read_scalar_at::<$t>(buf, loc) };
            v.to_string()
        }};
    }

    let s = match base_type {
        // Bool is wire-encoded as `u8`; render as `0`/`1` to match
        // `read_leaf`'s upstream `get_any_field_string` output.
        BaseType::Bool | BaseType::UByte => read!(u8),
        BaseType::Byte => read!(i8),
        BaseType::Short => read!(i16),
        BaseType::UShort => read!(u16),
        BaseType::Int => read!(i32),
        BaseType::UInt => read!(u32),
        BaseType::Long => read!(i64),
        BaseType::ULong => read!(u64),
        BaseType::Float => read!(f32),
        BaseType::Double => read!(f64),
        // Structs cannot legally contain strings, vectors,
        // sub-tables, or unions per the FlatBuffers schema rules
        // (flatc rejects them at compile time). Surface a
        // user-facing error if a malformed `.bfbs` does anyway.
        other => {
            return Err(ExecuteError::UnsupportedType {
                field: field.name().to_string(),
                type_name: base_type_name(other),
            });
        }
    };
    Ok(Some(s))
}

/// Walk a fixed-size array field at byte offset `array_loc` inside
/// `buf`. Arrays only legally appear *inside structs*, so this is
/// reached only from `walk_struct`'s `BaseType::Array` arm.
///
/// Wire layout (FlatBuffers schema rules):
///
/// - The N elements live contiguously starting at `array_loc`.
/// - Element type comes from `field.type_().element()` and may be
///   either a scalar or `Obj` (where the referenced `Object` is a
///   struct — flatc forbids array-of-table / array-of-string).
/// - Element stride is the scalar size for scalar elements or the
///   struct's `bytesize()` for struct elements.
/// - `field.type_().fixed_length()` is the (`u16`) element count
///   declared in the schema; the buffer always carries exactly
///   that many elements (no run-time count word).
///
/// Empty step list at an array is a hard error (mirrors the
/// "no v0.1 textual leaf form" behaviour of structs and
/// vector-of-structs); callers descend with `[i]` or `[*]`.
/// `[map-key]` / `|keys` are rejected because arrays carry no
/// `(key)` annotation, and `.field` / `|type` are static
/// type-system mismatches at this position.
fn walk_array(
    buf: &[u8],
    array_loc: usize,
    field: &Field,
    schema: &Schema,
    steps: &[Step],
) -> Result<Vec<Option<String>>, ExecuteError> {
    let field_name = field.name();
    let element_type = field.type_().element();
    let fixed_length = usize::from(field.type_().fixed_length());

    // Resolve element stride (and child object, when struct-typed).
    let (elem_size, child_object): (usize, Option<Object>) = match element_type {
        BaseType::Obj => {
            let child_index = field.type_().index();
            if child_index < 0 {
                return Err(ExecuteError::Internal(format!(
                    "schema array field {field_name:?} has element BaseType::Obj \
                     but negative object index ({child_index})"
                )));
            }
            let child_object = schema.objects().get(
                usize::try_from(child_index)
                    .expect("non-negative after the explicit < 0 guard above"),
            );
            // flatc rejects array-of-table at schema-compile time;
            // surface a malformed `.bfbs` rather than silently
            // calling `walk_struct` on table-shaped bytes.
            if !child_object.is_struct() {
                return Err(ExecuteError::Internal(format!(
                    "schema array field {field_name:?} has Obj element pointing \
                     at non-struct object {:?}",
                    child_object.name()
                )));
            }
            let bytesize = struct_bytesize(&child_object)?;
            (bytesize, Some(child_object))
        }
        scalar => {
            let size = scalar_byte_size(scalar).ok_or_else(|| ExecuteError::UnsupportedType {
                field: field_name.to_string(),
                type_name: base_type_name(scalar),
            })?;
            (size, None)
        }
    };

    let (head, tail) = match steps.split_first() {
        Some(p) => p,
        None => {
            return Err(ExecuteError::UnsupportedType {
                field: field_name.to_string(),
                type_name: "fixed-size array (no v0.1 textual leaf form — descend with [i] or [*])",
            });
        }
    };

    match head {
        Step::Index(i) => {
            let idx = *i;
            if idx >= fixed_length {
                // OOB short-circuits to NULL per design §4.3,
                // matching the vector arms.
                return Ok(vec![None]);
            }
            let elem_loc = array_loc + idx * elem_size;
            walk_array_element(
                buf,
                elem_loc,
                element_type,
                child_object.as_ref(),
                field,
                schema,
                tail,
            )
        }
        Step::All => {
            // Fanout: one leaf per element, wire-format order.
            let mut out = Vec::with_capacity(fixed_length);
            for idx in 0..fixed_length {
                let elem_loc = array_loc + idx * elem_size;
                let mut sub = walk_array_element(
                    buf,
                    elem_loc,
                    element_type,
                    child_object.as_ref(),
                    field,
                    schema,
                    tail,
                )?;
                out.append(&mut sub);
            }
            Ok(out)
        }
        Step::MapKey(_) => Err(ExecuteError::UnsupportedType {
            field: field_name.to_string(),
            type_name:
                "[map-key] on fixed-size array (arrays have no (key) annotation; use [i] or [*])",
        }),
        Step::MapKeys => Err(ExecuteError::UnsupportedType {
            field: field_name.to_string(),
            type_name: "|keys on fixed-size array (arrays have no (key) annotation)",
        }),
        Step::Field(_) => Err(ExecuteError::UnsupportedType {
            field: field_name.to_string(),
            type_name: ".field on fixed-size array (use [i] or [*] first)",
        }),
        Step::UnionType => Err(ExecuteError::UnsupportedType {
            field: field_name.to_string(),
            type_name: "|type on fixed-size array (arrays hold no unions)",
        }),
    }
}

/// Read or descend into a single fixed-size array element at
/// `elem_loc`. Splits on the element's `BaseType`:
///
/// - `Obj` (struct) with non-empty `tail` → descend via
///   [`walk_struct`]. Empty `tail` is rejected (struct elements
///   have no v0.1 textual leaf form, mirroring vector-of-struct).
/// - Scalar with empty `tail` → stringify via
///   [`read_struct_scalar_leaf`] (same scalar formatting as table /
///   struct / vector-element leaves).
/// - Scalar with non-empty `tail` → "can't descend into a scalar"
///   rejection.
fn walk_array_element(
    buf: &[u8],
    elem_loc: usize,
    element_type: BaseType,
    child_object: Option<&Object>,
    field: &Field,
    schema: &Schema,
    tail: &[Step],
) -> Result<Vec<Option<String>>, ExecuteError> {
    let field_name = field.name();
    if element_type == BaseType::Obj {
        let child = child_object.expect("Obj element implies child_object resolved by walk_array");
        if tail.is_empty() {
            return Err(ExecuteError::UnsupportedType {
                field: field_name.to_string(),
                type_name: "fixed-size-array-of-struct element (no v0.1 textual leaf form — descend with `.field`)",
            });
        }
        return walk_struct(buf, elem_loc, child, schema, tail);
    }

    if !tail.is_empty() {
        return Err(ExecuteError::UnsupportedType {
            field: field_name.to_string(),
            type_name: base_type_name(element_type),
        });
    }

    read_struct_scalar_leaf(buf, elem_loc, field, element_type).map(|opt| vec![opt])
}
