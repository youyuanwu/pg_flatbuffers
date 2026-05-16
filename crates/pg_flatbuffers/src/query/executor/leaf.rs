//! Leaf stringification: turn a present scalar / bool / string field
//! into its `Display` representation. Shared by table, vector-element,
//! struct-field, and array-element leaf paths.

use super::util::base_type_name;
use super::ExecuteError;
use flatbuffers::Table;
use flatbuffers_reflection::get_any_field_string;
use flatbuffers_reflection::reflection::{BaseType, Field, Schema};

pub(super) fn read_leaf(
    table: &Table,
    field: &Field,
    schema: &Schema,
    base_type: BaseType,
) -> Result<Option<String>, ExecuteError> {
    let field_name = field.name();
    match base_type {
        BaseType::Bool
        | BaseType::Byte
        | BaseType::UByte
        | BaseType::UType
        | BaseType::Short
        | BaseType::UShort
        | BaseType::Int
        | BaseType::UInt
        | BaseType::Long
        | BaseType::ULong
        | BaseType::Float
        | BaseType::Double
        | BaseType::String => {
            // SAFETY: see `execute`. `get_any_field_string` reads via
            // the same offset accessors the verifier validated. For
            // scalars: returns the schema default if absent (the
            // default §4.3 behaviour with
            // `fill_scalar_defaults = on`). For strings: we already
            // returned `Ok(None)` above for absent strings, so a
            // returned empty string here means an explicit empty
            // string in the buffer.
            let s = unsafe { get_any_field_string(table, field, schema) };
            Ok(Some(s))
        }
        BaseType::Obj
        | BaseType::Vector
        | BaseType::Vector64
        | BaseType::Union
        | BaseType::Array
        | BaseType::None => Err(ExecuteError::UnsupportedType {
            field: field_name.to_string(),
            type_name: base_type_name(base_type),
        }),
        // `BaseType` is a `pub struct BaseType(pub i8)` newtype, so
        // the compiler can't prove the match above is exhaustive.
        // Treat any out-of-range value as an internal corruption.
        _ => Err(ExecuteError::Internal(format!(
            "schema field {field_name:?} has unknown BaseType ({})",
            base_type.0
        ))),
    }
}
