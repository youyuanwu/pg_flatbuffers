//! Union dispatch — `walk_union` resolves the discriminator and descends into
//! the active variant's value (table / struct / string); `read_union_type_leaf`
//! emits the variant *name* for the `|type` terminal step;
//! `read_union_value_leaf` returns the value of a string-typed variant when
//! the path stops at the union itself (e.g. `Msg:body` for a `string`
//! variant).
//!
//! Variant kinds supported (per design §4.3):
//!
//! - **Table variants** — the union value slot holds a forward `uoffset_t` to
//!   a sub-table; descent recurses through [`walk_table`].
//! - **Struct variants** — the union value slot holds a forward `uoffset_t`
//!   to *out-of-line* struct bytes (unlike a struct *field*, which is
//!   inline); descent recurses through [`walk_struct`].
//! - **String variants** — the union value slot holds a forward
//!   `uoffset_t` to a length-prefixed UTF-8 string; there is no further
//!   descent, so the empty-tail leaf form is the only valid path.

use super::struct_::walk_struct;
use super::walk::walk_table;
use super::{ExecuteError, ExecuteOptions};
use crate::query::ast::Step;
use flatbuffers::{ForwardsUOffset, Table, read_scalar_at};
use flatbuffers_reflection::reflection::{BaseType, EnumVal, Field, Schema};

/// Resolve a `BaseType::Union` field to its active variant and
/// recursively descend with the remaining steps. The wire layout
/// (design §4.3, "Union types"):
///
/// - The union value sits at vtable slot `N` (== `value_field.offset()`).
/// - The auto-generated `u8` discriminator (`<name>_type`,
///   `BaseType::UType`) sits at slot `N - 2` (flatc convention).
/// - Discriminator `0` is the synthesized `NONE` variant: no value,
///   short-circuits to `Ok(vec![None])` — same shape as an absent
///   sub-table.
///
/// Variant lookup goes through the reflected `Enum` (sorted by
/// `value`, so [`flatbuffers::Vector::lookup_by_key`] is O(log N)).
/// `EnumVal::union_type()` then resolves the underlying `Type`.
///
/// Supported variant base types (design §4.3):
///
/// - `BaseType::Obj` + `!is_struct()` — table variant; descent via
///   [`walk_table`].
/// - `BaseType::Obj` + `is_struct()` — struct variant; descent via
///   [`walk_struct`]. Note that union struct *values* live
///   out-of-line behind a `uoffset_t` (unlike a struct *field*
///   which is inlined into its parent table's body).
/// - `BaseType::String` — string variant; strings carry no
///   sub-fields, so any non-empty `tail` is a type-shape error.
///   The leaf form (`Msg:body` with `tail.is_empty()`) is handled
///   by [`read_union_value_leaf`], not this function.
pub(super) fn walk_union(
    table: &Table,
    value_field: &Field,
    schema: &Schema,
    tail: &[Step],
    options: &ExecuteOptions,
) -> Result<Vec<Option<String>>, ExecuteError> {
    let field_name = value_field.name();

    let variant = match resolve_active_variant(table, value_field, schema)? {
        // `NONE` variant (or discriminator absent altogether) —
        // there is no value to descend into; one virtual `None`
        // leaf, matching the absent-sub-table shape.
        None => return Ok(vec![None]),
        Some(v) => v,
    };

    // Resolve the variant's underlying type.
    let variant_type = variant.union_type().ok_or_else(|| {
        ExecuteError::Internal(format!(
            "union variant {:?} has no union_type",
            variant.name()
        ))
    })?;

    // Note: `tail` is guaranteed non-empty here. `walk_table`'s
    // leaf branch routes empty-tail unions to
    // [`read_union_value_leaf`] before dispatching to `walk_union`.

    match variant_type.base_type() {
        BaseType::Obj => {
            let variant_obj_idx = variant_type.index();
            if variant_obj_idx < 0 {
                return Err(ExecuteError::Internal(format!(
                    "union variant {:?} has Obj union_type but negative object index ({variant_obj_idx})",
                    variant.name()
                )));
            }
            let variant_object = schema.objects().get(
                usize::try_from(variant_obj_idx)
                    .expect("non-negative after the explicit < 0 guard above"),
            );

            if variant_object.is_struct() {
                // Struct variant: out-of-line struct bytes pointed
                // at by the union value slot. Resolve the uoffset
                // ourselves (mirroring `vector_body_at` and the
                // upstream verifier's `verify_union` path) and
                // hand off to `walk_struct`. `walk_struct` itself
                // rejects empty `tail` with the standard
                // "no v0.1 textual leaf form" message.
                match resolve_value_slot_loc(table, value_field) {
                    Some(value_loc) => {
                        let buf = table.buf();
                        // SAFETY: see `execute`. The verifier validated
                        // that this value slot, when present, holds a
                        // valid `u32` forward offset to struct bytes
                        // whose layout matches `variant_object`'s
                        // schema-declared `bytesize`. `read_scalar_at`
                        // performs little-endian-to-host conversion.
                        let forward_offset =
                            unsafe { read_scalar_at::<u32>(buf, value_loc) } as usize;
                        let struct_loc = value_loc + forward_offset;
                        walk_struct(buf, struct_loc, &variant_object, schema, tail)
                    }
                    // Defensive: discriminator non-zero but value
                    // slot absent. Verifier would normally have
                    // rejected this; treat as `None` rather than
                    // panicking on a misclassified malformed buffer.
                    None => Ok(vec![None]),
                }
            } else {
                // Table variant: existing path.
                //
                // SAFETY: see `execute`. Verifier validated that,
                // when the discriminator is non-zero, slot N holds
                // a valid `ForwardsUOffset<Table>` referencing a
                // buffer-bounded table matching `variant_object`.
                // We bypass `get_field_table` here because that
                // helper guards on `field.type_().base_type() ==
                // BaseType::Obj` and would reject a
                // `BaseType::Union` field — but the wire layout
                // for a union value is identical to a sub-table
                // (a forward 32-bit offset to a table), so reading
                // it directly via `Table::get` is sound.
                let value_table_opt =
                    unsafe { table.get::<ForwardsUOffset<Table>>(value_field.offset(), None) };
                match value_table_opt {
                    Some(value_table) => {
                        walk_table(&value_table, &variant_object, schema, tail, options)
                    }
                    // Defensive: see the struct-variant arm above.
                    None => Ok(vec![None]),
                }
            }
        }
        BaseType::String => {
            // String union variant — the FlatBuffers binary
            // format reserves no field-shaped descent for strings
            // (they are scalar leaves of UTF-8 bytes), so any
            // non-empty `tail` is a type-shape error. The valid
            // leaf form (`Msg:body` with empty `tail`) is
            // intercepted by `walk_table` and routed to
            // [`read_union_value_leaf`] before reaching here.
            Err(ExecuteError::UnsupportedType {
                field: field_name.to_string(),
                type_name: "string union variant has no sub-fields \
                            (drop trailing path steps)",
            })
        }
        // FlatBuffers schema rules forbid scalar / vector / array
        // variants in a union (flatc rejects them at compile
        // time). Surface a malformed `.bfbs` rather than
        // misinterpreting one.
        other => Err(ExecuteError::Internal(format!(
            "union variant {:?} has unsupported base type {:?} \
             (flatc only emits Obj / String variants)",
            variant.name(),
            other.variant_name().unwrap_or("?")
        ))),
    }
}

/// Read the active variant *name* of a union field as a single
/// string leaf (e.g. `"TableA"`, `"NONE"`). Used by the `|type`
/// terminal step. Mirrors the discriminator-resolution prelude of
/// [`walk_union`] but never descends into the value sub-table.
///
/// Unlike the descent path, an absent / NONE union does **not**
/// short-circuit to SQL NULL: it returns the variant name `"NONE"`
/// (or whatever the schema's value-0 enum entry is named) so callers
/// can `WHERE flatbuffers_query('Msg:body|type', buf) = 'NONE'`
/// without juggling NULLs.
pub(super) fn read_union_type_leaf(
    table: &Table,
    value_field: &Field,
    schema: &Schema,
) -> Result<Vec<Option<String>>, ExecuteError> {
    let field_name = value_field.name();
    let value_slot = value_field.offset();
    let disc_slot = match value_slot.checked_sub(2) {
        Some(s) => s,
        None => {
            return Err(ExecuteError::Internal(format!(
                "union value field {field_name:?} has vtable offset {value_slot}; \
                 expected ≥2 to leave room for the discriminator slot"
            )));
        }
    };

    // SAFETY: see `execute`. The verifier validated the discriminator
    // slot, when present, holds a valid `u8` union tag for the enum.
    let disc = unsafe { table.get::<u8>(disc_slot, Some(0)) }.unwrap_or(0);

    let enum_index = value_field.type_().index();
    if enum_index < 0 {
        return Err(ExecuteError::Internal(format!(
            "schema field {field_name:?} has BaseType::Union but negative \
             enum index ({enum_index})"
        )));
    }
    let enum_idx =
        usize::try_from(enum_index).expect("non-negative after the explicit < 0 guard above");
    let enums = schema.enums();
    if enum_idx >= enums.len() {
        return Err(ExecuteError::Internal(format!(
            "schema field {field_name:?} references enum index {enum_idx} \
             but schema has only {} enums",
            enums.len()
        )));
    }
    let enum_def = enums.get(enum_idx);
    let variant = enum_def
        .values()
        .lookup_by_key(disc as i64, |v, k| v.key_compare_with_value(*k))
        .ok_or_else(|| {
            ExecuteError::Internal(format!(
                "union field {field_name:?} discriminator {disc} not found in \
                 enum {:?}",
                enum_def.name()
            ))
        })?;

    Ok(vec![Some(variant.name().to_string())])
}

/// Read the *value* of a union field as a leaf, for the empty-tail
/// path (e.g. `Msg:body` for a `string` variant). Called by
/// [`super::walk::walk_table`] in place of the generic
/// [`super::leaf::read_leaf`] so the union case can peek at the
/// active variant's base type:
///
/// - `NONE` discriminator → `Ok(vec![None])`, mirroring the absent
///   sub-table shape used by the descent path.
/// - String variant → reads the length-prefixed UTF-8 string and
///   returns it as a single leaf. Absent value slot with a
///   non-zero discriminator is defensively treated as `None`.
/// - Table / struct variant → no v0.1 textual leaf form; reject
///   with `UnsupportedType` and hint the caller to descend with
///   `.field`. Symmetric with the leaf-form rejections for the
///   non-union `Obj` and struct cases (see `walk_table` and
///   `walk_struct`).
pub(super) fn read_union_value_leaf(
    table: &Table,
    value_field: &Field,
    schema: &Schema,
) -> Result<Vec<Option<String>>, ExecuteError> {
    let field_name = value_field.name();

    let variant = match resolve_active_variant(table, value_field, schema)? {
        None => return Ok(vec![None]),
        Some(v) => v,
    };

    let variant_type = variant.union_type().ok_or_else(|| {
        ExecuteError::Internal(format!(
            "union variant {:?} has no union_type",
            variant.name()
        ))
    })?;

    match variant_type.base_type() {
        BaseType::String => {
            // SAFETY: see `execute`. The verifier validated that,
            // when the discriminator selects a string variant,
            // the value slot holds a valid `ForwardsUOffset<&str>`
            // referencing a buffer-bounded length-prefixed UTF-8
            // string.
            let s_opt = unsafe { table.get::<ForwardsUOffset<&str>>(value_field.offset(), None) };
            Ok(vec![s_opt.map(|s| s.to_string())])
        }
        BaseType::Obj => Err(ExecuteError::UnsupportedType {
            field: field_name.to_string(),
            type_name: "union with table/struct variant has no leaf form \
                        — descend with `.field`",
        }),
        other => Err(ExecuteError::Internal(format!(
            "union variant {:?} has unsupported base type {:?} \
             (flatc only emits Obj / String variants)",
            variant.name(),
            other.variant_name().unwrap_or("?")
        ))),
    }
}

/// Resolve a union field's active variant via its discriminator
/// slot. Returns `Ok(None)` for the `NONE` (zero) discriminator —
/// callers translate that to the appropriate empty/null shape for
/// their context (see [`walk_union`], [`read_union_value_leaf`]).
///
/// Shared between the descent path and the empty-tail leaf path
/// so they observe the same discriminator → variant resolution
/// (and the same set of `Internal` corruption errors when a
/// malformed `.bfbs` reaches the executor past the verifier).
fn resolve_active_variant<'a>(
    table: &Table,
    value_field: &Field,
    schema: &'a Schema<'a>,
) -> Result<Option<EnumVal<'a>>, ExecuteError> {
    let field_name = value_field.name();
    let value_slot = value_field.offset();

    // Discriminator slot is the immediately preceding vtable slot.
    // A vtable slot < 2 would mean the union value is the very
    // first field, leaving no room for the discriminator — flatc
    // would never emit such a layout, so treat it as a corrupted
    // `.bfbs`.
    let disc_slot = value_slot.checked_sub(2).ok_or_else(|| {
        ExecuteError::Internal(format!(
            "union value field {field_name:?} has vtable offset {value_slot}; \
             expected ≥2 to leave room for the discriminator slot"
        ))
    })?;

    // SAFETY: see `execute`. The verifier validated that, when
    // present, the discriminator slot holds a valid `u8` union
    // tag for `value_field.type_().index()`'s enum.
    let disc = unsafe { table.get::<u8>(disc_slot, Some(0)) }.unwrap_or(0);
    if disc == 0 {
        return Ok(None);
    }

    let enum_index = value_field.type_().index();
    if enum_index < 0 {
        return Err(ExecuteError::Internal(format!(
            "schema field {field_name:?} has BaseType::Union but negative \
             enum index ({enum_index})"
        )));
    }
    let enum_idx =
        usize::try_from(enum_index).expect("non-negative after the explicit < 0 guard above");
    let enums = schema.enums();
    if enum_idx >= enums.len() {
        return Err(ExecuteError::Internal(format!(
            "schema field {field_name:?} references enum index {enum_idx} \
             but schema has only {} enums",
            enums.len()
        )));
    }
    let enum_def = enums.get(enum_idx);

    // EnumVal vector is sorted by `value` (per the upstream
    // `key_compare_*` impls), so lookup is O(log N).
    let variant = enum_def
        .values()
        .lookup_by_key(disc as i64, |v, k| v.key_compare_with_value(*k))
        .ok_or_else(|| {
            ExecuteError::Internal(format!(
                "union field {field_name:?} discriminator {disc} not found in \
                 enum {:?}",
                enum_def.name()
            ))
        })?;
    Ok(Some(variant))
}

/// Compute the in-buffer byte location of a union value slot
/// (i.e. the location of the `uoffset_t` that points to the
/// variant's content). Returns `None` if the vtable slot is 0
/// (value absent).
///
/// Used for non-`ForwardsUOffset<T>`-typed dispatch — currently
/// the struct-variant arm of [`walk_union`], where there is no
/// concrete `Struct` Rust type to drive the typed `Table::get`
/// path used by the table and string variants.
fn resolve_value_slot_loc(table: &Table, value_field: &Field) -> Option<usize> {
    let slot_in_vtable = table.vtable().get(value_field.offset()) as usize;
    if slot_in_vtable == 0 {
        return None;
    }
    Some(table.loc() + slot_in_vtable)
}
