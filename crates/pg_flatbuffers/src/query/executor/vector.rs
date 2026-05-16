//! Vector dispatch — `Step::Index` / `Step::All` over table/struct/scalar/string vectors,
//! plus the helpers that resolve the per-element schema object and read the raw vector body.
//!
//! `Step::MapKey` and `Step::MapKeys` live in [`super::map_key`].

use super::map_key::{walk_vector_at_map_key, walk_vector_map_keys};
use super::struct_::{struct_bytesize, walk_struct};
use super::util::{base_type_name, map_reflection_err};
use super::walk::walk_table;
use super::{ExecuteError, ExecuteOptions};
use crate::query::ast::Step;
use flatbuffers::{read_scalar_at, ForwardsUOffset, Table, Vector, SIZE_UOFFSET};
use flatbuffers_reflection::get_field_vector;
use flatbuffers_reflection::reflection::{BaseType, Field, Object, Schema};

pub(super) fn walk_vector(
    table: &Table,
    field: &Field,
    schema: &Schema,
    steps: &[Step],
    options: &ExecuteOptions,
) -> Result<Vec<Option<String>>, ExecuteError> {
    let field_name = field.name();

    // Vector64 is in scope for the design but not for v0.1: the
    // upstream verifier still treats it the same as Vector for length
    // bookkeeping, but element-offset arithmetic uses 64-bit offsets
    // and our `flatbuffers::Vector<T>` accessors only handle 32-bit.
    // Reject loudly so we don't silently truncate addresses.
    //
    // Belt-and-suspenders: `verify()` (see `verify::reject_unsupported_schema_features`)
    // now pre-empts any Vector64 schema *before* the executor runs,
    // so reaching this branch in production would mean the executor
    // was invoked on a schema that bypassed `verify()`. The check
    // stays as defense-in-depth for unit tests and for any future
    // executor entry point that's added without going through the
    // verifier wrapper.
    if field.type_().base_type() == BaseType::Vector64 {
        return Err(ExecuteError::UnsupportedType {
            field: field_name.to_string(),
            type_name: "vector64",
        });
    }

    // A bare `Order:items` (no indexer) cannot be stringified in
    // v0.1: the design doesn't define a textual form for whole
    // vectors, and the JSON path lives in a future slice.
    let (head, tail) = match steps.split_first() {
        Some(pair) => pair,
        None => {
            return Err(ExecuteError::UnsupportedType {
                field: field_name.to_string(),
                type_name: "vector (use [i] / [*] / [key] / |keys to access elements)",
            });
        }
    };

    let element_base_type = field.type_().element();

    match head {
        Step::Index(idx) => {
            walk_vector_at_index(table, field, schema, *idx, tail, element_base_type, options)
        }
        Step::All => walk_vector_all(table, field, schema, tail, element_base_type, options),
        Step::MapKey(key) => {
            walk_vector_at_map_key(table, field, schema, key, tail, element_base_type, options)
        }
        Step::MapKeys => walk_vector_map_keys(table, field, schema, tail, element_base_type),
        // `|type` makes no sense on a vector field — vectors aren't
        // unions. Reject explicitly so the AST stays exhaustive.
        Step::UnionType => Err(ExecuteError::UnsupportedStep {
            what: "|type (only valid on union fields)",
        }),
        // A `Step::Field` after a vector field is a parser bug — the
        // grammar requires an indexer right after the vector. Be
        // defensive in case the AST grows new arms.
        Step::Field(_) => Err(ExecuteError::Internal(format!(
            "expected `[i]` / `[*]` / `[key]` / `|keys` after vector field {field_name:?}, \
             found a `Field` step"
        ))),
    }
}

/// Materialise a single indexed element. Out-of-range / absent →
/// `vec![None]`. Element type `Obj` may consume more `tail` steps
/// for descent; scalar / string element types must be at leaf.
fn walk_vector_at_index(
    table: &Table,
    field: &Field,
    schema: &Schema,
    idx: usize,
    tail: &[Step],
    element_base_type: BaseType,
    options: &ExecuteOptions,
) -> Result<Vec<Option<String>>, ExecuteError> {
    let field_name = field.name();

    if element_base_type == BaseType::Obj {
        let child_object = lookup_vector_element_object(field, schema)?;
        if child_object.is_struct() {
            // Vector of inline structs: forward to `walk_struct`
            // with the element's absolute byte location.
            if tail.is_empty() {
                return Err(ExecuteError::UnsupportedType {
                    field: field_name.to_string(),
                    type_name: "vector-of-struct element (no v0.1 textual leaf form — descend with `.field`)",
                });
            }
            // SAFETY: see `execute`; the buffer was verified, and
            // `field` came from the schema. The verifier asserts
            // the vector slot, when present, points at a vector
            // body sized for `child_object.bytesize() * count`.
            let (body_loc, count) = match unsafe { vector_body_at(table, field) } {
                Some(b) => b,
                // Defensive: vtable said present but follow returned
                // None (or vtable said absent). Match the
                // absent-vector-under-`[i]` contract.
                None => return Ok(vec![None]),
            };
            if idx >= count {
                return Ok(vec![None]);
            }
            let bytesize = struct_bytesize(&child_object)?;
            // Element layout: `body_loc` is the location of the
            // element-count u32; element bytes start at `body_loc + 4`,
            // each occupying `bytesize` bytes (no per-element padding
            // because `flatc` aligns the vector body to the struct's
            // `minalign`).
            let elem_loc = body_loc + SIZE_UOFFSET + idx * bytesize;
            return walk_struct(table.buf(), elem_loc, &child_object, schema, tail);
        }

        // SAFETY: see `execute`; the buffer was verified, and `field`
        // came from the schema. The verifier asserts the vector slot
        // resolves to a well-formed Vector<ForwardsUOffset<Table>>
        // when `field.type_().element() == Obj`.
        let vec_opt = unsafe {
            table.get::<ForwardsUOffset<Vector<ForwardsUOffset<Table>>>>(field.offset(), None)
        };
        let vec = match vec_opt {
            Some(v) => v,
            // Defensive: vtable said present but follow returned
            // None. Match the absent-vector contract.
            None => return Ok(vec![None]),
        };
        if idx >= vec.len() {
            return Ok(vec![None]);
        }
        let elem_table = vec.get(idx);

        if tail.is_empty() {
            // `items[3]` on a vector of tables produces a sub-table
            // *value*, which has no v0.1 textual form. Same rationale
            // as bare-table-at-leaf in `read_leaf`.
            return Err(ExecuteError::UnsupportedType {
                field: field_name.to_string(),
                type_name: "vector-of-table element (sub-table at leaf)",
            });
        }
        return walk_table(&elem_table, &child_object, schema, tail, options);
    }

    // Scalar / string element types: this *must* be the terminal
    // step. `items[3].sub` against a Vector<int> is a path error.
    if !tail.is_empty() {
        return Err(ExecuteError::UnsupportedType {
            field: field_name.to_string(),
            type_name: base_type_name(element_base_type),
        });
    }
    read_vector_element(table, field, idx, element_base_type).map(|opt| vec![opt])
}

/// Fan out across every element of `field` in wire-format order.
/// Absent / empty vector → `vec![]`. For tables, recurse with `tail`
/// per-element and concatenate (depth-first left-to-right per
/// §7.2 step 3); for scalars / strings, `tail` must be empty and we
/// stringify each element.
fn walk_vector_all(
    table: &Table,
    field: &Field,
    schema: &Schema,
    tail: &[Step],
    element_base_type: BaseType,
    options: &ExecuteOptions,
) -> Result<Vec<Option<String>>, ExecuteError> {
    let field_name = field.name();

    if element_base_type == BaseType::Obj {
        let child_object = lookup_vector_element_object(field, schema)?;
        if child_object.is_struct() {
            // Vector of inline structs under `[*]`: same
            // element-window math as `walk_vector_at_index`, but
            // iterate over every element. Empty / absent vector
            // → `vec![]` (no leaves to fan out), matching the
            // table-element contract above.
            if tail.is_empty() {
                return Err(ExecuteError::UnsupportedType {
                    field: field_name.to_string(),
                    type_name: "vector-of-struct element (no v0.1 textual leaf form — descend with `.field`)",
                });
            }
            // SAFETY: see `execute`. Same vector-body shape as
            // `walk_vector_at_index`'s struct branch.
            let (body_loc, count) = match unsafe { vector_body_at(table, field) } {
                Some(b) => b,
                None => return Ok(vec![]),
            };
            let bytesize = struct_bytesize(&child_object)?;
            // Pre-size to `count` as a floor; nested `[*]` may grow
            // the result further per element (mirrors the
            // table-element loop below).
            let mut out: Vec<Option<String>> = Vec::with_capacity(count);
            for i in 0..count {
                let elem_loc = body_loc + SIZE_UOFFSET + i * bytesize;
                let mut sub = walk_struct(table.buf(), elem_loc, &child_object, schema, tail)?;
                out.append(&mut sub);
            }
            return Ok(out);
        }

        // SAFETY: see `execute`. Same Vector<ForwardsUOffset<Table>>
        // shape as `walk_vector_at_index`.
        let vec_opt = unsafe {
            table.get::<ForwardsUOffset<Vector<ForwardsUOffset<Table>>>>(field.offset(), None)
        };
        let vec = match vec_opt {
            Some(v) => v,
            // Absent vector under `[*]` → no fanout, zero leaves.
            None => return Ok(vec![]),
        };

        if tail.is_empty() {
            return Err(ExecuteError::UnsupportedType {
                field: field_name.to_string(),
                type_name: "vector-of-table element (sub-table at leaf)",
            });
        }

        // Pre-size to vec.len() as a floor; nested `[*]` may grow
        // the result further per element.
        let mut out: Vec<Option<String>> = Vec::with_capacity(vec.len());
        for elem_table in vec.iter() {
            let mut sub = walk_table(&elem_table, &child_object, schema, tail, options)?;
            out.append(&mut sub);
        }
        return Ok(out);
    }

    // Scalar / string element types: must be the terminal step.
    if !tail.is_empty() {
        return Err(ExecuteError::UnsupportedType {
            field: field_name.to_string(),
            type_name: base_type_name(element_base_type),
        });
    }
    read_vector_all(table, field, element_base_type)
}

/// Resolve the `Object` referenced by a vector-of-table field's
/// `field.type_().index()`. Factored out because both the indexed
/// and the fanout paths need it.
pub(super) fn lookup_vector_element_object<'a>(
    field: &Field,
    schema: &'a Schema,
) -> Result<Object<'a>, ExecuteError> {
    let child_index = field.type_().index();
    if child_index < 0 {
        return Err(ExecuteError::Internal(format!(
            "vector field {:?} has element BaseType::Obj but negative object index ({})",
            field.name(),
            child_index
        )));
    }
    Ok(schema.objects().get(
        usize::try_from(child_index).expect("non-negative after the explicit < 0 guard above"),
    ))
}

/// Compute the byte location of the element-count word and the
/// element count for a (32-bit) vector field, returning `None` if
/// the vector is absent (vtable slot 0). The count is the *number
/// of elements*, not bytes; element layout starts at
/// `body_loc + SIZE_UOFFSET`.
///
/// # Safety contract
///
/// The caller has verified the buffer (see [`execute_with_options`]). When the
/// vtable slot is non-zero, the `u32` at the field location holds a
/// valid forward offset to a vector body whose first 4 bytes are a
/// `u32` element count.
unsafe fn vector_body_at(table: &Table, field: &Field) -> Option<(usize, usize)> {
    let vtable_offset = table.vtable().get(field.offset()) as usize;
    if vtable_offset == 0 {
        return None;
    }
    let buf = table.buf();
    let field_loc = table.loc() + vtable_offset;
    // SAFETY: `field_loc` is in-bounds because the verifier sized the
    // table region against the vtable; `read_scalar_at` does the
    // little-endian-to-host conversion.
    let forward_offset = unsafe { read_scalar_at::<u32>(buf, field_loc) } as usize;
    let body_loc = field_loc + forward_offset;
    // SAFETY: `body_loc` is in-bounds for the same reason.
    let count = unsafe { read_scalar_at::<u32>(buf, body_loc) } as usize;
    Some((body_loc, count))
}

fn read_vector_element(
    table: &Table,
    field: &Field,
    idx: usize,
    element_base_type: BaseType,
) -> Result<Option<String>, ExecuteError> {
    // Helper: read a typed scalar vector and stringify the indexed
    // element via `Display`. Mirrors the `i64::Display` / `f64::Display`
    // formatting used by `read_leaf` / `scalar_default_string` so an
    // element value matches what the same value would render as if
    // stored directly in a scalar field.
    macro_rules! scalar {
        ($t:ty) => {{
            // SAFETY: see `execute`; the buffer was verified, and
            // `field.type_().element()` matches `$t`. The
            // `get_field_vector` helper additionally checks that
            // `field.type_().base_type() == BaseType::Vector`.
            let vec_opt =
                unsafe { get_field_vector::<$t>(table, field) }.map_err(map_reflection_err)?;
            let vec = match vec_opt {
                Some(v) => v,
                None => return Ok(None),
            };
            if idx >= vec.len() {
                return Ok(None);
            }
            Ok(Some(vec.get(idx).to_string()))
        }};
    }

    match element_base_type {
        // Bool is wire-encoded as `u8`; stringify as `0`/`1` to
        // match the way present bool *fields* render through the
        // upstream `get_any_field_string` (see `read_leaf`).
        BaseType::Bool | BaseType::UByte => scalar!(u8),
        BaseType::Byte => scalar!(i8),
        BaseType::Short => scalar!(i16),
        BaseType::UShort => scalar!(u16),
        BaseType::Int => scalar!(i32),
        BaseType::UInt => scalar!(u32),
        BaseType::Long => scalar!(i64),
        BaseType::ULong => scalar!(u64),
        BaseType::Float => scalar!(f32),
        BaseType::Double => scalar!(f64),
        BaseType::String => {
            // SAFETY: see `execute`. The schema asserts the element
            // type is `String`, so the vector slot is a
            // `ForwardsUOffset<Vector<ForwardsUOffset<&str>>>`. We
            // can't use the `get_field_vector` helper here because
            // its `T: Follow<Inner = T>` bound rejects
            // `ForwardsUOffset<&str>` (whose `Inner` is `&str`).
            let vec_opt = unsafe {
                table.get::<ForwardsUOffset<Vector<ForwardsUOffset<&str>>>>(field.offset(), None)
            };
            let vec = match vec_opt {
                Some(v) => v,
                None => return Ok(None),
            };
            if idx >= vec.len() {
                return Ok(None);
            }
            Ok(Some(vec.get(idx).to_string()))
        }
        // Vectors of unions / vectors-of-vectors / vector-of-array
        // need their own slices.
        other => Err(ExecuteError::UnsupportedType {
            field: field.name().to_string(),
            type_name: base_type_name(other),
        }),
    }
}

/// Stringify *every* element of a vector whose elements are scalars
/// or strings, in wire-format order. Returns `Ok(vec![])` for an
/// absent vector. Mirrors [`read_vector_element`] arm-for-arm so
/// element formatting is identical.
fn read_vector_all(
    table: &Table,
    field: &Field,
    element_base_type: BaseType,
) -> Result<Vec<Option<String>>, ExecuteError> {
    macro_rules! scalar {
        ($t:ty) => {{
            // SAFETY: see `read_vector_element`.
            let vec_opt =
                unsafe { get_field_vector::<$t>(table, field) }.map_err(map_reflection_err)?;
            let vec = match vec_opt {
                Some(v) => v,
                None => return Ok(vec![]),
            };
            Ok(vec.iter().map(|e| Some(e.to_string())).collect())
        }};
    }

    match element_base_type {
        BaseType::Bool | BaseType::UByte => scalar!(u8),
        BaseType::Byte => scalar!(i8),
        BaseType::Short => scalar!(i16),
        BaseType::UShort => scalar!(u16),
        BaseType::Int => scalar!(i32),
        BaseType::UInt => scalar!(u32),
        BaseType::Long => scalar!(i64),
        BaseType::ULong => scalar!(u64),
        BaseType::Float => scalar!(f32),
        BaseType::Double => scalar!(f64),
        BaseType::String => {
            // SAFETY: see `read_vector_element` — same direct
            // `table.get::<ForwardsUOffset<Vector<ForwardsUOffset<&str>>>>`
            // workaround for the `Follow<Inner = Self>` bound.
            let vec_opt = unsafe {
                table.get::<ForwardsUOffset<Vector<ForwardsUOffset<&str>>>>(field.offset(), None)
            };
            let vec = match vec_opt {
                Some(v) => v,
                None => return Ok(vec![]),
            };
            Ok(vec.iter().map(|s| Some(s.to_string())).collect())
        }
        other => Err(ExecuteError::UnsupportedType {
            field: field.name().to_string(),
            type_name: base_type_name(other),
        }),
    }
}
