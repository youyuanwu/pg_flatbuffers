//! Top-level recursive walker: walks `Step`s starting from a verified
//! table, dispatching to vector / union / struct / leaf helpers.

use super::leaf::read_leaf;
use super::struct_::walk_struct;
use super::union::{read_union_type_leaf, walk_union};
use super::util::{base_type_name, find_field, map_reflection_err, scalar_default_string};
use super::vector::walk_vector;
use super::{ExecuteError, ExecuteOptions};
use crate::query::ast::Step;
use flatbuffers::Table;
use flatbuffers_reflection::get_field_table;
use flatbuffers_reflection::reflection::{BaseType, Object, Schema};

/// Walk the path `steps` starting from `table` (whose schema shape is
/// `object`). Returns one or more leaves in wire-format order; see
/// [`super::execute_with_options`] for the length contract.
///
/// # Safety contract
///
/// The caller has already verified the underlying buffer (see
/// [`super::execute_with_options`]). Every unsafe block in this function is sound under
/// that precondition.
pub(super) fn walk_table(
    table: &Table,
    object: &Object,
    schema: &Schema,
    steps: &[Step],
    options: &ExecuteOptions,
) -> Result<Vec<Option<String>>, ExecuteError> {
    let (head, tail) = steps
        .split_first()
        .expect("parser guarantees at least one step");

    let field = match head {
        Step::Field(field_ref) => find_field(object, field_ref)?,
        Step::Index(_) => return Err(ExecuteError::UnsupportedStep { what: "[index]" }),
        Step::All => return Err(ExecuteError::UnsupportedStep { what: "[*]" }),
        Step::MapKey(_) => return Err(ExecuteError::UnsupportedStep { what: "[map-key]" }),
        Step::MapKeys => return Err(ExecuteError::UnsupportedStep { what: "|keys" }),
        Step::UnionType => return Err(ExecuteError::UnsupportedStep { what: "|type" }),
    };

    let field_name = field.name();
    let base_type = field.type_().base_type();

    // `|type` is a tail-position leaf marker on a union value
    // field: it yields the *name* of the active variant ("TableA",
    // "NONE", …) rather than its numeric discriminator (which is
    // available on the auto-generated `<field>_type` UType scalar).
    // Intercepted ahead of the absent-nullable short-circuit so
    // that an absent / NONE union still returns the variant name
    // ("NONE") rather than SQL NULL — symmetric with how
    // `<field>_type` returns "0" for an absent discriminator.
    if let [Step::UnionType, more @ ..] = tail {
        if base_type != BaseType::Union {
            return Err(ExecuteError::UnsupportedStep {
                what: "|type (only valid on union fields)",
            });
        }
        if !more.is_empty() {
            return Err(ExecuteError::UnsupportedStep {
                what: "|type (terminal — cannot descend further)",
            });
        }
        return read_union_type_leaf(table, &field, schema);
    }

    // Presence check via vtable: vtable.get(offset) == 0 means the
    // field is absent in this table instance. This is how we map
    // "field not set" to SQL NULL for nullable types (string,
    // sub-table, vector). Scalar types are handled separately
    // below: an absent scalar yields its schema default by default
    // (§4.3, matches the FlatBuffers reader API); when
    // `pg_flatbuffers.fill_scalar_defaults = off` it surfaces as
    // SQL NULL instead (§10).
    let is_present = table.vtable().get(field.offset()) != 0;
    let is_nullable_type = matches!(
        base_type,
        BaseType::String
            | BaseType::Obj
            | BaseType::Vector
            | BaseType::Vector64
            | BaseType::Union
            | BaseType::Array
    );

    // Vector dispatch must come before *both* the absent-nullable
    // short-circuit and the leaf/descent fork, because the
    // `[i]` vs `[*]` distinction lives in `walk_vector`: an absent
    // vector under `[i]` is `vec![None]` (one virtual entry), but
    // under `[*]` it's `vec![]` (no items to fan out over).
    // `walk_vector` handles both cases internally.
    if matches!(base_type, BaseType::Vector | BaseType::Vector64) {
        return walk_vector(table, &field, schema, tail, options);
    }

    if !is_present && is_nullable_type {
        // One virtual leaf, value `None`. (Vector dispatch above
        // already covered the vector-typed nullable case.)
        return Ok(vec![None]);
    }

    if tail.is_empty() {
        if !is_present {
            // Absent scalar at leaf — the read-side knob decides
            // between filling the schema default (FlatBuffers reader
            // API parity, §4.3) and surfacing SQL NULL
            // (presence-aware, §10). Upstream `get_any_field_string`
            // returns "" for absent *anything*, so when we do fill,
            // we must source the default from
            // `Field::default_integer()` / `default_real()`
            // ourselves. (Required is enforced by the verifier; we
            // never reach here for `field.required() == true`
            // because verification would have failed.)
            if options.fill_scalar_defaults {
                return Ok(vec![Some(scalar_default_string(&field, base_type))]);
            } else {
                return Ok(vec![None]);
            }
        }
        return read_leaf(table, &field, schema, base_type).map(|opt| vec![opt]);
    }

    // Descent — only nested tables are supported in this slice.
    match base_type {
        BaseType::Obj => {
            let child_index = field.type_().index();
            if child_index < 0 {
                return Err(ExecuteError::Internal(format!(
                    "schema field {field_name:?} has BaseType::Obj but negative \
                     object index ({child_index})"
                )));
            }
            let child_object = schema.objects().get(
                usize::try_from(child_index)
                    .expect("non-negative after the explicit < 0 guard above"),
            );
            if child_object.is_struct() {
                // Struct descent. Structs are inline fixed-size; the
                // vtable slot stores the byte offset within the
                // parent table's data area where the struct begins
                // (mirroring `Table::get`'s prelude). Slot 0 means
                // the field is absent — emit one virtual `None` leaf,
                // matching the absent-sub-table case below.
                if tail.is_empty() {
                    return Err(ExecuteError::UnsupportedType {
                        field: field_name.to_string(),
                        type_name: "struct (no v0.1 textual leaf form — descend with `.field`)",
                    });
                }
                let slot = table.vtable().get(field.offset()) as usize;
                if slot == 0 {
                    return Ok(vec![None]);
                }
                let struct_loc = table.loc() + slot;
                return walk_struct(table.buf(), struct_loc, &child_object, schema, tail);
            }
            // SAFETY: see `execute`; the buffer was verified, and
            // `field` came from the schema (so its offset is the
            // verified vtable slot).
            let child_table_opt =
                unsafe { get_field_table(table, &field) }.map_err(map_reflection_err)?;
            match child_table_opt {
                Some(child_table) => walk_table(&child_table, &child_object, schema, tail, options),
                // Defensive: vtable said present, but the deref
                // returned None. Treat as absent rather than
                // panicking.
                None => Ok(vec![None]),
            }
        }
        BaseType::Union => walk_union(table, &field, schema, tail, options),
        other => Err(ExecuteError::UnsupportedType {
            field: field_name.to_string(),
            type_name: base_type_name(other),
        }),
    }
}
