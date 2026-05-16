//! Leaf stringification: turn a present scalar / bool / string field
//! into its `Display` representation. Shared by table, vector-element,
//! struct-field, and array-element leaf paths.
//!
//! Float (`f32`) and Double (`f64`) fields are routed through
//! Postgres's own `float4out` / `float8out` (see
//! [`super::pg_text`]) so query leaves match `SELECT col::text`
//! from a `real` / `double precision` column byte-for-byte (design
//! §7.2). The upstream `get_any_field_string` would otherwise
//! format floats with Rust's `f64::Display`, which diverges on
//! `Infinity` / `NaN` and never uses scientific notation.

use super::ExecuteError;
use super::pg_text::{format_float4, format_float8};
use super::util::base_type_name;
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
        // Floats need Postgres-format stringification, so we bypass
        // `get_any_field_string` (which would use Rust's `Display`)
        // and read the underlying scalar directly. The default
        // value supplied to `Table::get` is the schema-declared
        // default, mirroring `get_any_field_string`'s behaviour for
        // an absent scalar with `fill_scalar_defaults = on`.
        BaseType::Float => {
            // SAFETY: see `execute`; the buffer was verified, and
            // the vtable slot for `field.offset()` is validated.
            #[allow(
                clippy::cast_possible_truncation,
                reason = "schema default for a Float field is representable as f32 by construction"
            )]
            let default = field.default_real() as f32;
            let v = unsafe { table.get::<f32>(field.offset(), Some(default)) }.unwrap_or(default);
            Ok(Some(format_float4(v)))
        }
        BaseType::Double => {
            // SAFETY: same as the Float arm above.
            let default = field.default_real();
            let v = unsafe { table.get::<f64>(field.offset(), Some(default)) }.unwrap_or(default);
            Ok(Some(format_float8(v)))
        }
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
