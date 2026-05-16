//! Union dispatch — `walk_union` resolves the discriminator and descends into
//! the active variant's value table; `read_union_type_leaf` emits the
//! variant *name* for the `|type` terminal step.

use super::walk::walk_table;
use super::{ExecuteError, ExecuteOptions};
use crate::query::ast::Step;
use flatbuffers::{ForwardsUOffset, Table};
use flatbuffers_reflection::reflection::{BaseType, Field, Schema};

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
/// v0.1 supports **table-typed variants only**: struct or string
/// variants are rejected with [`super::ExecuteError::UnsupportedType`].
/// Empty `tail` is rejected too — the value table itself has no v0.1
/// textual leaf form (matches the absent-tail behaviour for nested
/// tables; descend with `.field` to get a value).
pub(super) fn walk_union(
    table: &Table,
    value_field: &Field,
    schema: &Schema,
    tail: &[Step],
    options: &ExecuteOptions,
) -> Result<Vec<Option<String>>, ExecuteError> {
    let field_name = value_field.name();
    let value_slot = value_field.offset();

    // Discriminator slot is the immediately preceding vtable slot.
    // A vtable slot < 2 would mean the union value is the very first
    // field, leaving no room for the discriminator — flatc would
    // never emit such a layout, so treat it as a corrupted `.bfbs`.
    let disc_slot = match value_slot.checked_sub(2) {
        Some(s) => s,
        None => {
            return Err(ExecuteError::Internal(format!(
                "union value field {field_name:?} has vtable offset {value_slot}; \
                 expected ≥2 to leave room for the discriminator slot"
            )));
        }
    };

    // SAFETY: see `execute`. The verifier validated that the
    // discriminator slot, when present, holds a valid `u8` union
    // tag for `value_field.type_().index()`'s enum.
    let disc = unsafe { table.get::<u8>(disc_slot, Some(0)) }.unwrap_or(0);
    if disc == 0 {
        // `NONE` variant (or discriminator absent altogether) —
        // there is no value to descend into; one virtual `None`
        // leaf, matching the absent-sub-table shape.
        return Ok(vec![None]);
    }

    // Resolve the enum and the matching variant.
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

    // Resolve the variant's underlying type.
    let variant_type = variant.union_type().ok_or_else(|| {
        ExecuteError::Internal(format!(
            "union variant {:?} has no union_type",
            variant.name()
        ))
    })?;
    if variant_type.base_type() != BaseType::Obj {
        return Err(ExecuteError::UnsupportedType {
            field: field_name.to_string(),
            type_name: "union with non-table variant (v0.1 supports table variants only)",
        });
    }
    let variant_obj_idx = variant_type.index();
    if variant_obj_idx < 0 {
        return Err(ExecuteError::Internal(format!(
            "union variant {:?} has Obj union_type but negative object index ({variant_obj_idx})",
            variant.name()
        )));
    }
    let variant_object = schema.objects().get(
        usize::try_from(variant_obj_idx).expect("non-negative after the explicit < 0 guard above"),
    );
    if variant_object.is_struct() {
        // Same as above: struct variants need a separate slice
        // (would dispatch to `walk_struct` once we settle the
        // wire-format check — flatc allows struct unions only via
        // the `(native_inline)` extension, which v0.1 doesn't model).
        return Err(ExecuteError::UnsupportedType {
            field: field_name.to_string(),
            type_name: "union with struct variant (v0.1 supports table variants only)",
        });
    }

    // Note: `tail` is guaranteed non-empty here. `walk_table`'s
    // leaf short-circuit fires before the descent match dispatches
    // to `walk_union`, so the empty-tail case is handled by
    // `read_leaf` (which rejects `BaseType::Union` loudly).

    // SAFETY: see `execute`. Verifier validated that, when the
    // discriminator is non-zero, slot N holds a valid
    // `ForwardsUOffset<Table>` referencing a buffer-bounded table
    // matching `variant_object`. We bypass `get_field_table` here
    // because that helper guards on `field.type_().base_type() ==
    // BaseType::Obj` and would reject a `BaseType::Union` field —
    // but the wire layout for a union value is identical to a
    // sub-table (a forward 32-bit offset to a table), so reading
    // it directly via `Table::get` is sound.
    let value_table_opt =
        unsafe { table.get::<ForwardsUOffset<Table>>(value_field.offset(), None) };
    match value_table_opt {
        Some(value_table) => walk_table(&value_table, &variant_object, schema, tail, options),
        // Defensive: discriminator non-zero but value slot absent.
        // Verifier would normally have rejected this; treat as `None`
        // rather than panicking on a misclassified malformed buffer.
        None => Ok(vec![None]),
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
