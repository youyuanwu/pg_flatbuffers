//! Small shared utilities — field lookup, reflection-error mapping,
//! scalar-default stringification, and `BaseType` → name conversion.

use super::ExecuteError;
use super::pg_text::{format_float4, format_float8};
use crate::query::ast::FieldRef;
use flatbuffers_reflection::FlatbufferError;
use flatbuffers_reflection::reflection::{BaseType, Field, Object};

pub(super) fn find_field<'a>(
    object: &'a Object<'a>,
    field_ref: &FieldRef,
) -> Result<Field<'a>, ExecuteError> {
    let fields = object.fields();
    let table_name = object.name();
    match field_ref {
        FieldRef::Name(name) => fields
            // FlatBuffers schemas store `Object.fields` sorted by
            // name (the upstream crate also relies on this for
            // `lookup_by_key` binary search), so this is O(log N).
            .lookup_by_key(name.as_str(), |f, key| f.key_compare_with_value(key))
            .ok_or_else(|| ExecuteError::FieldNotFound {
                what: name.clone(),
                table: table_name.to_string(),
            }),
        FieldRef::Id(id) => {
            // Field IDs are dense and small in practice (typically
            // 0..N), so a linear scan is fine. We can't binary-search
            // because the vector is sorted by name, not by id.
            for f in &fields {
                if f.id() == *id {
                    return Ok(f);
                }
            }
            Err(ExecuteError::FieldNotFound {
                what: format!("#{id}"),
                table: table_name.to_string(),
            })
        }
    }
}

pub(super) fn map_reflection_err(e: FlatbufferError) -> ExecuteError {
    match e {
        FlatbufferError::FieldNotFound => ExecuteError::FieldNotFound {
            what: "<unknown>".to_owned(),
            table: "<unknown>".to_owned(),
        },
        other => ExecuteError::Internal(other.to_string()),
    }
}

/// Stringify a scalar field's schema default, matching the
/// formatter used by `flatbuffers_reflection::get_any_field_string`
/// for *present* values: `i64::Display` for integral/bool and
/// Postgres's `float4out` / `float8out` for floats (design §7.2;
/// see [`super::pg_text`]). We deliberately do not special-case
/// bool to `"true"` / `"false"` because the upstream stringifier
/// emits `"0"` / `"1"` for present bools and we want absent
/// bools to round-trip identically.
pub(super) fn scalar_default_string(field: &Field, base_type: BaseType) -> String {
    match base_type {
        BaseType::Float => {
            // Reflection stores all numeric defaults as `f64`; the
            // schema guarantees the value is representable as `f32`
            // for a `Float` field, so the narrowing cast is
            // round-trip safe.
            #[allow(
                clippy::cast_possible_truncation,
                reason = "schema default for a Float field is representable as f32 by construction"
            )]
            let v = field.default_real() as f32;
            format_float4(v)
        }
        BaseType::Double => format_float8(field.default_real()),
        // Integral types and bool: `default_integer()` is i64.
        _ => field.default_integer().to_string(),
    }
}

pub(super) fn base_type_name(b: BaseType) -> &'static str {
    match b {
        BaseType::None => "none",
        BaseType::UType => "union-discriminator",
        BaseType::Bool => "bool",
        BaseType::Byte => "byte",
        BaseType::UByte => "ubyte",
        BaseType::Short => "short",
        BaseType::UShort => "ushort",
        BaseType::Int => "int",
        BaseType::UInt => "uint",
        BaseType::Long => "long",
        BaseType::ULong => "ulong",
        BaseType::Float => "float",
        BaseType::Double => "double",
        BaseType::String => "string",
        BaseType::Vector => "vector",
        BaseType::Vector64 => "vector64",
        BaseType::Obj => "object",
        BaseType::Union => "union",
        BaseType::Array => "array",
        _ => "unknown",
    }
}
