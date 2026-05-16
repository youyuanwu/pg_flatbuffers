//! Reflection-driven JSON → FlatBuffers builder (design §8).
//!
//! Entry point: [`json_to_buf`]. Given a [`serde_json::Value`] and the
//! reflected `Schema`, builds a FlatBuffer matching `flatc
//! --strict-json` input conventions (the inverse of [`crate::to_json`]).
//!
//! Per-type decoding (one-to-one with the design §8 table):
//!
//! | JSON shape | FlatBuffers type |
//! | --- | --- |
//! | JSON number | numeric scalars (`int8`…`int64`, `uint8`…`uint64`, `float`, `double`) |
//! | JSON boolean | `bool` |
//! | JSON string (member name) | enum (numeric fallback for unknown name) |
//! | JSON string | `string` |
//! | JSON string (base64) | `[ubyte]` / `[u8]` |
//! | JSON array | `[T]` for non-`ubyte` element types |
//! | JSON object | table |
//! | sibling `<name>_type` + `<name>` pair | union (string + table variants) |
//!
//! ## v0.1 deferrals (rejected with [`FromJsonError::Unsupported`]):
//!
//! - **Inline struct fields in tables** (e.g. `Point { pos:Vec3 }`).
//!   Inlining variable-sized struct bytes via the FlatBuffers builder
//!   requires a `Push` impl with the struct's static byte size, and
//!   reflection-driven struct sizes are dynamic — adding it cleanly is
//!   a separate sub-slice (const-generic dispatch on `bytesize`).
//! - **Struct union variants** — same reason.
//! - **Fixed-size arrays** (`BaseType::Array`) — only legal inside
//!   structs, which we don't yet handle.
//! - **`(hex)` attribute on `[ubyte]`** — always interpreted as base64.
//! - **`pg_flatbuffers.from_json_unknown = ignore` GUC** — unknown keys
//!   always raise [`FromJsonError::UnknownField`] in v0.1.
//! - **`max_apparent_size_mb` output cap** — the FlatBuffers builder
//!   itself is bounded by Postgres's per-backend memory; surfacing a
//!   pre-allocation cap is a follow-up.
//! - **Pre-walk depth limit** — depth tracking happens during the build,
//!   not in a separate pre-pass; check happens before each recursive
//!   call.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use flatbuffers::{FlatBufferBuilder, ForwardsUOffset, UnionWIPOffset, WIPOffset};
use flatbuffers_reflection::reflection::{BaseType, Enum, Field, Object, Schema};
use serde_json::Value;
use thiserror::Error;

/// Errors raised by [`json_to_buf`].
#[derive(Debug, Error)]
pub enum FromJsonError {
    /// The `bfbs` schema has no `root_table`. Mirrors
    /// [`crate::to_json::ToJsonError::NoRootTable`].
    #[error("schema has no root_table (cannot build a typeless buffer)")]
    NoRootTable,

    /// The top-level JSON value is not an object. The root of a
    /// FlatBuffer is always a table → must be a JSON object.
    #[error("top-level JSON value must be an object (got {got})")]
    RootNotObject { got: &'static str },

    /// A JSON object is missing a `required` field declared in the
    /// schema.
    #[error("table {table:?} is missing required field {field:?}")]
    MissingRequiredField { table: String, field: String },

    /// A JSON object contains a key that doesn't correspond to any
    /// field on the target table. Always raised in v0.1; a future
    /// `pg_flatbuffers.from_json_unknown = ignore` GUC will allow
    /// silently dropping unknowns for forward-compat workflows
    /// (design §8).
    #[error("table {table:?} has no field named {field:?}")]
    UnknownField { table: String, field: String },

    /// JSON value's shape doesn't match the FlatBuffers field type
    /// (e.g. JSON string where a number was expected).
    #[error("field {field:?}: expected {expected}, got JSON {got}")]
    TypeMismatch {
        field: String,
        expected: &'static str,
        got: &'static str,
    },

    /// An integer JSON value is out of range for the field's
    /// declared scalar type (e.g. 300 for a `byte` field).
    #[error("field {field:?}: integer value {value} out of range for {kind}")]
    IntegerOutOfRange {
        field: String,
        value: i128,
        kind: &'static str,
    },

    /// A JSON string was offered as an enum value but no
    /// `EnumVal::name` matched.
    #[error("field {field:?}: {value:?} is not a known member of enum {enum_name:?}")]
    UnknownEnumName {
        field: String,
        enum_name: String,
        value: String,
    },

    /// A JSON string offered for a `[ubyte]` field failed base64
    /// decoding.
    #[error("field {field:?}: invalid base64 in [ubyte] field: {error}")]
    InvalidBase64 { field: String, error: String },

    /// A union object is missing the `<name>_type` discriminator key.
    #[error("union field {field:?} requires sibling key {disc_key:?}")]
    MissingUnionDiscriminator { field: String, disc_key: String },

    /// Build nesting exceeded `pg_flatbuffers.max_depth`.
    #[error("JSON nesting exceeds max_depth ({max})")]
    NestingTooDeep { max: usize },

    /// A FlatBuffers feature this slice hasn't implemented yet.
    /// Carries the field name + a short description.
    #[error("field {field:?}: {what} (deferred to a future slice)")]
    Unsupported { field: String, what: &'static str },

    /// An internal reflection-look-up failure (indicates a malformed
    /// `.bfbs` slipping past `validate_schema`).
    #[error("internal: {0}")]
    Internal(String),
}

/// Top-level entry: build a FlatBuffer from `json` under `schema`.
/// The root is the schema's registered `root_table`.
///
/// `max_depth` caps the JSON nesting (object/array depth) the
/// walker descends. A depth exceedance raises
/// [`FromJsonError::NestingTooDeep`].
pub fn json_to_buf(
    json: &Value,
    schema: &Schema,
    max_depth: usize,
) -> Result<Vec<u8>, FromJsonError> {
    let root_object = schema.root_table().ok_or(FromJsonError::NoRootTable)?;
    let obj = json
        .as_object()
        .ok_or_else(|| FromJsonError::RootNotObject {
            got: value_kind(json),
        })?;

    let mut fbb = FlatBufferBuilder::with_capacity(1024);
    let root = build_table(obj, &root_object, schema, &mut fbb, 0, max_depth)?;
    fbb.finish_minimal(root);
    Ok(fbb.finished_data().to_vec())
}

/// Build a table from a JSON object. Returns a `WIPOffset` to the
/// finished table within `fbb`.
///
/// Build order (FlatBuffers requires children before parents):
/// 1. Pre-build all variable-length children (strings, vectors,
///    sub-tables) and collect their `WIPOffset`s in a per-field map.
/// 2. `start_table`.
/// 3. Push every slot: scalars inline with their schema default,
///    non-scalars via `push_slot_always(slot, wipoffset)`.
/// 4. `end_table`.
fn build_table<'fbb>(
    obj: &serde_json::Map<String, Value>,
    object: &Object,
    schema: &Schema,
    fbb: &mut FlatBufferBuilder<'fbb>,
    depth: usize,
    max_depth: usize,
) -> Result<WIPOffset<flatbuffers::TableFinishedWIPOffset>, FromJsonError> {
    check_depth(depth, max_depth)?;
    let object_name = object.name().to_string();

    // Reject unknown keys early so the build is deterministic.
    // Build a name → Field lookup once and consult it for both
    // dispatch and validation.
    let fields = object.fields();
    let mut by_name = std::collections::HashMap::<&str, Field<'_>>::with_capacity(fields.len());
    let mut union_disc_keys = std::collections::HashSet::<String>::new();
    for i in 0..fields.len() {
        let f = fields.get(i);
        by_name.insert(f.name(), f);
        if f.type_().base_type() == BaseType::Union {
            // The `<name>_type` discriminator field is also a legal
            // JSON key (flatc emits it alongside the value), but we
            // process it implicitly while consuming the value field.
            union_disc_keys.insert(format!("{}_type", f.name()));
        }
    }
    for key in obj.keys() {
        if !by_name.contains_key(key.as_str()) && !union_disc_keys.contains(key) {
            return Err(FromJsonError::UnknownField {
                table: object_name,
                field: key.clone(),
            });
        }
    }

    // Pre-build phase: build children (strings, sub-tables, vectors,
    // union values) outside any open table. We can't open the table
    // first because FlatBuffers forbids nested table starts.
    //
    // Layout of the collected offsets:
    //   - per non-scalar field name → (slot, offset)
    //   - for unions: also need the discriminator byte (push as scalar
    //     in the table phase) and the value offset (push as
    //     uoffset).
    enum SlotPush {
        Scalar(ScalarSlot),
        Offset(WIPOffset<UnionWIPOffset>),
        UnionPair {
            disc: u8,
            value_off: Option<WIPOffset<UnionWIPOffset>>,
        },
    }
    let mut pending: Vec<(Field<'_>, SlotPush)> = Vec::new();

    for i in 0..fields.len() {
        let field = fields.get(i);
        let field_name = field.name();
        let base_type = field.type_().base_type();

        if base_type == BaseType::Union {
            let pair =
                build_union_pair(obj, &field, schema, fbb, &object_name, depth + 1, max_depth)?;
            match pair {
                Some((disc, value_off)) => {
                    pending.push((field, SlotPush::UnionPair { disc, value_off }));
                }
                None => {
                    if field.required() {
                        return Err(FromJsonError::MissingRequiredField {
                            table: object_name,
                            field: field_name.to_string(),
                        });
                    }
                }
            }
            continue;
        }

        let json_val = obj.get(field_name);
        // JSON null is treated as "field absent" (flatc parity).
        let value = match json_val {
            Some(v) if !v.is_null() => v,
            _ => {
                if field.required() {
                    return Err(FromJsonError::MissingRequiredField {
                        table: object_name,
                        field: field_name.to_string(),
                    });
                }
                continue;
            }
        };

        // Scalar (incl. enum-typed scalar): push as scalar in the
        // table-write phase using the schema default for force-or-not
        // semantics.
        if is_scalar(base_type) {
            let slot = encode_scalar(&field, value, schema)?;
            pending.push((field, SlotPush::Scalar(slot)));
            continue;
        }

        // Non-scalar: build the child OUT of any open table.
        let off = match base_type {
            BaseType::String => match value.as_str() {
                Some(s) => fbb.create_string(s).as_union_value(),
                None => {
                    return Err(FromJsonError::TypeMismatch {
                        field: field_name.to_string(),
                        expected: "string",
                        got: value_kind(value),
                    });
                }
            },
            BaseType::Obj => {
                // Table or struct. Structs are deferred (see module
                // docs); reject cleanly.
                let child_idx = u_index(field.type_().index(), "Obj field")?;
                let child_object = schema.objects().get(child_idx);
                if child_object.is_struct() {
                    return Err(FromJsonError::Unsupported {
                        field: field_name.to_string(),
                        what: "inline struct field in table (deferred)",
                    });
                }
                let sub_obj = value
                    .as_object()
                    .ok_or_else(|| FromJsonError::TypeMismatch {
                        field: field_name.to_string(),
                        expected: "object",
                        got: value_kind(value),
                    })?;
                let sub_off =
                    build_table(sub_obj, &child_object, schema, fbb, depth + 1, max_depth)?;
                let raw: WIPOffset<UnionWIPOffset> = WIPOffset::new(sub_off.value());
                raw
            }
            BaseType::Vector => build_vector(value, &field, schema, fbb, depth + 1, max_depth)?,
            BaseType::Vector64 => {
                return Err(FromJsonError::Internal(format!(
                    "Vector64 field {field_name:?} reached from_json — should have been \
                     rejected at verify time"
                )));
            }
            BaseType::Array => {
                return Err(FromJsonError::Internal(format!(
                    "field {field_name:?} has BaseType::Array at table-field position \
                     (flatc only emits Array inside structs)"
                )));
            }
            BaseType::Union => unreachable!("handled above"),
            BaseType::None => {
                return Err(FromJsonError::Internal(format!(
                    "field {field_name:?} has BaseType::None (only valid as a union NONE marker)"
                )));
            }
            other => {
                return Err(FromJsonError::Internal(format!(
                    "unexpected non-scalar BaseType {:?} for field {field_name:?}",
                    other.variant_name().unwrap_or("?")
                )));
            }
        };
        pending.push((field, SlotPush::Offset(off)));
    }

    // Write phase: open the table, push every pending slot, close.
    let t = fbb.start_table();
    for (field, push) in &pending {
        let slot = field.offset();
        match push {
            SlotPush::Scalar(s) => push_scalar(fbb, slot, s),
            SlotPush::Offset(off) => {
                fbb.push_slot_always::<WIPOffset<UnionWIPOffset>>(slot, *off);
            }
            SlotPush::UnionPair { disc, value_off } => {
                // The discriminator slot is always slot-2 (flatc
                // convention).
                if let Some(disc_slot) = slot.checked_sub(2) {
                    fbb.push_slot::<u8>(disc_slot, *disc, 0);
                } else {
                    return Err(FromJsonError::Internal(format!(
                        "union value field {:?} has vtable offset {slot}; \
                         expected ≥2 to leave room for the discriminator slot",
                        field.name()
                    )));
                }
                if let Some(off) = value_off {
                    fbb.push_slot_always::<WIPOffset<UnionWIPOffset>>(slot, *off);
                }
            }
        }
    }
    Ok(fbb.end_table(t))
}

/// Build a vector field. Returns a type-erased `WIPOffset`; the
/// caller's `push_slot_always` reinterprets it as a uoffset.
fn build_vector<'fbb>(
    value: &Value,
    field: &Field,
    schema: &Schema,
    fbb: &mut FlatBufferBuilder<'fbb>,
    depth: usize,
    max_depth: usize,
) -> Result<WIPOffset<UnionWIPOffset>, FromJsonError> {
    check_depth(depth, max_depth)?;
    let field_name = field.name().to_string();
    let element_type = field.type_().element();

    // [ubyte] / [u8] — JSON string (base64). Tolerate empty array as well.
    if matches!(element_type, BaseType::UByte | BaseType::UType) {
        let bytes = match value {
            Value::String(s) => {
                BASE64
                    .decode(s.as_bytes())
                    .map_err(|e| FromJsonError::InvalidBase64 {
                        field: field_name.clone(),
                        error: e.to_string(),
                    })?
            }
            Value::Array(arr) => {
                // Also accept a JSON array of numbers for [ubyte]
                // (flatc accepts both forms on input).
                let mut out = Vec::with_capacity(arr.len());
                for (i, v) in arr.iter().enumerate() {
                    let n = v.as_u64().ok_or_else(|| FromJsonError::TypeMismatch {
                        field: format!("{field_name}[{i}]"),
                        expected: "byte",
                        got: value_kind(v),
                    })?;
                    if n > u8::MAX as u64 {
                        return Err(FromJsonError::IntegerOutOfRange {
                            field: format!("{field_name}[{i}]"),
                            value: n as i128,
                            kind: "u8",
                        });
                    }
                    out.push(n as u8);
                }
                out
            }
            _ => {
                return Err(FromJsonError::TypeMismatch {
                    field: field_name,
                    expected: "string (base64) or array of bytes",
                    got: value_kind(value),
                });
            }
        };
        let off = fbb.create_vector::<u8>(&bytes);
        return Ok(WIPOffset::new(off.value()));
    }

    // All other element types require a JSON array.
    let arr = value
        .as_array()
        .ok_or_else(|| FromJsonError::TypeMismatch {
            field: field_name.clone(),
            expected: "array",
            got: value_kind(value),
        })?;

    macro_rules! scalar_vec {
        ($t:ty, $decode:expr) => {{
            let mut out: Vec<$t> = Vec::with_capacity(arr.len());
            for (i, v) in arr.iter().enumerate() {
                out.push($decode(field, v, i)?);
            }
            let off = fbb.create_vector::<$t>(&out);
            return Ok(WIPOffset::new(off.value()));
        }};
    }

    match element_type {
        BaseType::Bool => scalar_vec!(bool, |f: &Field, v: &Value, i: usize| {
            v.as_bool().ok_or_else(|| FromJsonError::TypeMismatch {
                field: format!("{}[{i}]", f.name()),
                expected: "boolean",
                got: value_kind(v),
            })
        }),
        BaseType::Byte => scalar_vec!(i8, |f: &Field, v: &Value, i: usize| {
            decode_signed_int::<i8>(f, v, i, i64::from(i8::MIN), i64::from(i8::MAX), "i8")
        }),
        BaseType::Short => scalar_vec!(i16, |f: &Field, v: &Value, i: usize| {
            decode_signed_int::<i16>(f, v, i, i64::from(i16::MIN), i64::from(i16::MAX), "i16")
        }),
        BaseType::UShort => scalar_vec!(u16, |f: &Field, v: &Value, i: usize| {
            decode_unsigned_int::<u16>(f, v, i, u64::from(u16::MAX), "u16")
        }),
        BaseType::Int => {
            if field.type_().index() >= 0 {
                // Vector of int-backed enums: decode each element by
                // EnumVal name (string) or numeric.
                let enum_def = lookup_enum(field, schema)?;
                let mut out: Vec<i32> = Vec::with_capacity(arr.len());
                for (i, v) in arr.iter().enumerate() {
                    let n = decode_enum_value(field, v, i, &enum_def)?;
                    let n32 = i32::try_from(n).map_err(|_| FromJsonError::IntegerOutOfRange {
                        field: format!("{}[{i}]", field.name()),
                        value: i128::from(n),
                        kind: "i32",
                    })?;
                    out.push(n32);
                }
                let off = fbb.create_vector::<i32>(&out);
                return Ok(WIPOffset::new(off.value()));
            }
            scalar_vec!(i32, |f: &Field, v: &Value, i: usize| {
                decode_signed_int::<i32>(f, v, i, i64::from(i32::MIN), i64::from(i32::MAX), "i32")
            })
        }
        BaseType::UInt => scalar_vec!(u32, |f: &Field, v: &Value, i: usize| {
            decode_unsigned_int::<u32>(f, v, i, u64::from(u32::MAX), "u32")
        }),
        BaseType::Long => scalar_vec!(i64, |f: &Field, v: &Value, i: usize| {
            decode_signed_int::<i64>(f, v, i, i64::MIN, i64::MAX, "i64")
        }),
        BaseType::ULong => scalar_vec!(u64, |f: &Field, v: &Value, i: usize| {
            decode_unsigned_int::<u64>(f, v, i, u64::MAX, "u64")
        }),
        BaseType::Float => scalar_vec!(f32, |f: &Field, v: &Value, i: usize| {
            decode_float(f, v, i).map(|x| x as f32)
        }),
        BaseType::Double => scalar_vec!(f64, |f: &Field, v: &Value, i: usize| {
            decode_float(f, v, i)
        }),
        BaseType::String => {
            // Pre-build each string out-of-line, then create the
            // vector of uoffsets.
            let mut offs: Vec<WIPOffset<&str>> = Vec::with_capacity(arr.len());
            for (i, v) in arr.iter().enumerate() {
                let s = v.as_str().ok_or_else(|| FromJsonError::TypeMismatch {
                    field: format!("{field_name}[{i}]"),
                    expected: "string",
                    got: value_kind(v),
                })?;
                offs.push(fbb.create_string(s));
            }
            let off = fbb.create_vector(&offs);
            Ok(WIPOffset::new(off.value()))
        }
        BaseType::Obj => {
            let child_idx = u_index(field.type_().index(), "Vector of Obj")?;
            let child_object = schema.objects().get(child_idx);
            if child_object.is_struct() {
                return Err(FromJsonError::Unsupported {
                    field: field_name,
                    what: "vector of structs (deferred)",
                });
            }
            // Vector of tables: pre-build each sub-table.
            let mut offs: Vec<WIPOffset<flatbuffers::TableFinishedWIPOffset>> =
                Vec::with_capacity(arr.len());
            for (i, v) in arr.iter().enumerate() {
                let sub_obj = v.as_object().ok_or_else(|| FromJsonError::TypeMismatch {
                    field: format!("{field_name}[{i}]"),
                    expected: "object",
                    got: value_kind(v),
                })?;
                offs.push(build_table(
                    sub_obj,
                    &child_object,
                    schema,
                    fbb,
                    depth + 1,
                    max_depth,
                )?);
            }
            // The flatbuffers Rust API doesn't expose a one-shot
            // `create_vector` for `WIPOffset<TableFinished>`. Build
            // the vector manually:
            fbb.start_vector::<ForwardsUOffset<flatbuffers::Table<'_>>>(offs.len());
            for off in offs.iter().rev() {
                // start_vector writes elements in reverse — push the
                // uoffsets in reverse order so they end up in
                // declaration order in the final buffer.
                fbb.push::<WIPOffset<flatbuffers::TableFinishedWIPOffset>>(*off);
            }
            let v = fbb.end_vector::<ForwardsUOffset<flatbuffers::Table<'_>>>(offs.len());
            Ok(WIPOffset::new(v.value()))
        }
        BaseType::Union => Err(FromJsonError::Internal(format!(
            "field {field_name:?} is a vector of unions but reached from_json \
             (should have been rejected at verify time)"
        ))),
        other => Err(FromJsonError::Internal(format!(
            "vector field {field_name:?} has unsupported element type {:?}",
            other.variant_name().unwrap_or("?")
        ))),
    }
}

/// `(discriminator_byte, optional_value_uoffset)` returned by the
/// union-builder helper. `None` value means NONE (no value slot to
/// push) or value-slot omission for non-NONE variants (defensive).
type UnionPair = (u8, Option<WIPOffset<UnionWIPOffset>>);

/// Build the discriminator + value for a union field. Returns
/// `Some((disc, value_off))` on success, `None` if both
/// `<name>_type` and `<name>` are absent from the JSON. For NONE
/// (disc == 0), `value_off` is `None`.
fn build_union_pair<'fbb>(
    obj: &serde_json::Map<String, Value>,
    value_field: &Field,
    schema: &Schema,
    fbb: &mut FlatBufferBuilder<'fbb>,
    parent_table_name: &str,
    depth: usize,
    max_depth: usize,
) -> Result<Option<UnionPair>, FromJsonError> {
    let field_name = value_field.name();
    let disc_key = format!("{field_name}_type");

    let disc_json = obj.get(&disc_key);
    let value_json = obj.get(field_name);

    if disc_json.is_none() && (value_json.is_none() || value_json.is_some_and(Value::is_null)) {
        // Both sides absent — treat as "field absent".
        return Ok(None);
    }

    let disc_str = match disc_json {
        Some(Value::String(s)) => s.as_str(),
        Some(other) => {
            return Err(FromJsonError::TypeMismatch {
                field: disc_key,
                expected: "string (enum member name)",
                got: value_kind(other),
            });
        }
        None => {
            return Err(FromJsonError::MissingUnionDiscriminator {
                field: field_name.to_string(),
                disc_key,
            });
        }
    };

    let enum_def = lookup_enum(value_field, schema)?;
    let variant = enum_def
        .values()
        .iter()
        .find(|v| v.name() == disc_str)
        .ok_or_else(|| FromJsonError::UnknownEnumName {
            field: disc_key.clone(),
            enum_name: enum_def.name().to_string(),
            value: disc_str.to_string(),
        })?;
    let disc = u8::try_from(variant.value()).map_err(|_| {
        FromJsonError::Internal(format!(
            "union {:?} variant {:?} has out-of-range discriminator value {}",
            enum_def.name(),
            variant.name(),
            variant.value()
        ))
    })?;

    if disc == 0 {
        // NONE: no value to push. If a value was provided alongside,
        // ignore (matches flatc tolerance).
        return Ok(Some((0, None)));
    }

    let variant_type = variant.union_type().ok_or_else(|| {
        FromJsonError::Internal(format!(
            "union variant {:?} has no union_type",
            variant.name()
        ))
    })?;

    let value_v = value_json.ok_or_else(|| FromJsonError::MissingRequiredField {
        table: parent_table_name.to_string(),
        field: field_name.to_string(),
    })?;

    let value_off = match variant_type.base_type() {
        BaseType::Obj => {
            let obj_idx = u_index(variant_type.index(), "union variant Obj")?;
            let variant_object = schema.objects().get(obj_idx);
            if variant_object.is_struct() {
                return Err(FromJsonError::Unsupported {
                    field: field_name.to_string(),
                    what: "struct union variant (deferred)",
                });
            }
            let sub_obj = value_v
                .as_object()
                .ok_or_else(|| FromJsonError::TypeMismatch {
                    field: field_name.to_string(),
                    expected: "object",
                    got: value_kind(value_v),
                })?;
            let sub_off = build_table(sub_obj, &variant_object, schema, fbb, depth, max_depth)?;
            WIPOffset::new(sub_off.value())
        }
        BaseType::String => {
            let s = value_v
                .as_str()
                .ok_or_else(|| FromJsonError::TypeMismatch {
                    field: field_name.to_string(),
                    expected: "string",
                    got: value_kind(value_v),
                })?;
            fbb.create_string(s).as_union_value()
        }
        other => {
            return Err(FromJsonError::Internal(format!(
                "union variant {:?} has unsupported base type {:?}",
                variant.name(),
                other.variant_name().unwrap_or("?")
            )));
        }
    };

    Ok(Some((disc, Some(value_off))))
}

// -- scalar encoding --

/// Pre-decoded scalar payload for the table-write phase. Each arm
/// carries the bytes + the schema default that
/// `push_slot::<T>(slot, value, default)` needs.
enum ScalarSlot {
    Bool { value: bool, default: bool },
    I8 { value: i8, default: i8 },
    U8 { value: u8, default: u8 },
    I16 { value: i16, default: i16 },
    U16 { value: u16, default: u16 },
    I32 { value: i32, default: i32 },
    U32 { value: u32, default: u32 },
    I64 { value: i64, default: i64 },
    U64 { value: u64, default: u64 },
    F32 { value: f32, default: f32 },
    F64 { value: f64, default: f64 },
}

fn push_scalar(fbb: &mut FlatBufferBuilder<'_>, slot: u16, s: &ScalarSlot) {
    match s {
        ScalarSlot::Bool { value, default } => fbb.push_slot::<bool>(slot, *value, *default),
        ScalarSlot::I8 { value, default } => fbb.push_slot::<i8>(slot, *value, *default),
        ScalarSlot::U8 { value, default } => fbb.push_slot::<u8>(slot, *value, *default),
        ScalarSlot::I16 { value, default } => fbb.push_slot::<i16>(slot, *value, *default),
        ScalarSlot::U16 { value, default } => fbb.push_slot::<u16>(slot, *value, *default),
        ScalarSlot::I32 { value, default } => fbb.push_slot::<i32>(slot, *value, *default),
        ScalarSlot::U32 { value, default } => fbb.push_slot::<u32>(slot, *value, *default),
        ScalarSlot::I64 { value, default } => fbb.push_slot::<i64>(slot, *value, *default),
        ScalarSlot::U64 { value, default } => fbb.push_slot::<u64>(slot, *value, *default),
        ScalarSlot::F32 { value, default } => fbb.push_slot::<f32>(slot, *value, *default),
        ScalarSlot::F64 { value, default } => fbb.push_slot::<f64>(slot, *value, *default),
    }
}

fn encode_scalar(
    field: &Field,
    value: &Value,
    schema: &Schema,
) -> Result<ScalarSlot, FromJsonError> {
    let base_type = field.type_().base_type();
    let is_enum = field.type_().index() >= 0;
    let default_i = field.default_integer();
    let default_r = field.default_real();

    if is_enum {
        // Enum-backed scalar: accept either JSON string (member
        // name) or JSON number (raw discriminator value). Decoded
        // value gets re-cast to the field's underlying scalar type.
        let enum_def = lookup_enum(field, schema)?;
        let n = decode_enum_value(field, value, 0, &enum_def)?;
        return scalar_from_signed(base_type, n, default_i, field);
    }

    Ok(match base_type {
        BaseType::Bool => {
            let v = value.as_bool().ok_or_else(|| FromJsonError::TypeMismatch {
                field: field.name().to_string(),
                expected: "boolean",
                got: value_kind(value),
            })?;
            ScalarSlot::Bool {
                value: v,
                default: default_i != 0,
            }
        }
        BaseType::Byte => {
            let v = decode_signed_int::<i8>(
                field,
                value,
                0,
                i64::from(i8::MIN),
                i64::from(i8::MAX),
                "i8",
            )?;
            #[allow(
                clippy::cast_possible_truncation,
                reason = "default truncation matches FBB convention"
            )]
            ScalarSlot::I8 {
                value: v,
                default: default_i as i8,
            }
        }
        BaseType::UByte | BaseType::UType => {
            let v = decode_unsigned_int::<u8>(field, value, 0, u64::from(u8::MAX), "u8")?;
            #[allow(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "default truncation matches FBB convention"
            )]
            ScalarSlot::U8 {
                value: v,
                default: default_i as u8,
            }
        }
        BaseType::Short => {
            let v = decode_signed_int::<i16>(
                field,
                value,
                0,
                i64::from(i16::MIN),
                i64::from(i16::MAX),
                "i16",
            )?;
            #[allow(
                clippy::cast_possible_truncation,
                reason = "default truncation matches FBB convention"
            )]
            ScalarSlot::I16 {
                value: v,
                default: default_i as i16,
            }
        }
        BaseType::UShort => {
            let v = decode_unsigned_int::<u16>(field, value, 0, u64::from(u16::MAX), "u16")?;
            #[allow(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "default truncation matches FBB convention"
            )]
            ScalarSlot::U16 {
                value: v,
                default: default_i as u16,
            }
        }
        BaseType::Int => {
            let v = decode_signed_int::<i32>(
                field,
                value,
                0,
                i64::from(i32::MIN),
                i64::from(i32::MAX),
                "i32",
            )?;
            #[allow(
                clippy::cast_possible_truncation,
                reason = "default truncation matches FBB convention"
            )]
            ScalarSlot::I32 {
                value: v,
                default: default_i as i32,
            }
        }
        BaseType::UInt => {
            let v = decode_unsigned_int::<u32>(field, value, 0, u64::from(u32::MAX), "u32")?;
            #[allow(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "default truncation matches FBB convention"
            )]
            ScalarSlot::U32 {
                value: v,
                default: default_i as u32,
            }
        }
        BaseType::Long => {
            let v = decode_signed_int::<i64>(field, value, 0, i64::MIN, i64::MAX, "i64")?;
            ScalarSlot::I64 {
                value: v,
                default: default_i,
            }
        }
        BaseType::ULong => {
            let v = decode_unsigned_int::<u64>(field, value, 0, u64::MAX, "u64")?;
            #[allow(
                clippy::cast_sign_loss,
                reason = "default is schema-declared; FBB casts identically"
            )]
            ScalarSlot::U64 {
                value: v,
                default: default_i as u64,
            }
        }
        BaseType::Float => {
            let v = decode_float(field, value, 0)?;
            #[allow(
                clippy::cast_possible_truncation,
                reason = "Float field default fits in f32"
            )]
            ScalarSlot::F32 {
                value: v as f32,
                default: default_r as f32,
            }
        }
        BaseType::Double => {
            let v = decode_float(field, value, 0)?;
            ScalarSlot::F64 {
                value: v,
                default: default_r,
            }
        }
        other => {
            return Err(FromJsonError::Internal(format!(
                "encode_scalar: unexpected base_type {:?}",
                other.variant_name().unwrap_or("?")
            )));
        }
    })
}

fn scalar_from_signed(
    base_type: BaseType,
    v: i64,
    default_i: i64,
    field: &Field,
) -> Result<ScalarSlot, FromJsonError> {
    let name = field.name();
    macro_rules! r {
        ($variant:ident, $t:ty, $kind:literal) => {{
            let min = <$t>::MIN as i64;
            let max = <$t>::MAX as i64;
            if v < min || v > max {
                return Err(FromJsonError::IntegerOutOfRange {
                    field: name.to_string(),
                    value: i128::from(v),
                    kind: $kind,
                });
            }
            #[allow(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "range-checked above"
            )]
            Ok(ScalarSlot::$variant {
                value: v as $t,
                default: default_i as $t,
            })
        }};
    }
    match base_type {
        BaseType::Byte => r!(I8, i8, "i8"),
        BaseType::UByte | BaseType::UType => r!(U8, u8, "u8"),
        BaseType::Short => r!(I16, i16, "i16"),
        BaseType::UShort => r!(U16, u16, "u16"),
        BaseType::Int => r!(I32, i32, "i32"),
        BaseType::UInt => r!(U32, u32, "u32"),
        BaseType::Long => Ok(ScalarSlot::I64 {
            value: v,
            default: default_i,
        }),
        BaseType::ULong => {
            if v < 0 {
                return Err(FromJsonError::IntegerOutOfRange {
                    field: name.to_string(),
                    value: i128::from(v),
                    kind: "u64",
                });
            }
            #[allow(clippy::cast_sign_loss, reason = "non-negative checked above")]
            Ok(ScalarSlot::U64 {
                value: v as u64,
                default: default_i as u64,
            })
        }
        other => Err(FromJsonError::Internal(format!(
            "scalar_from_signed: not an integer base_type ({:?})",
            other.variant_name().unwrap_or("?")
        ))),
    }
}

fn decode_signed_int<T>(
    field: &Field,
    value: &Value,
    idx: usize,
    min: i64,
    max: i64,
    kind: &'static str,
) -> Result<T, FromJsonError>
where
    T: TryFrom<i64>,
{
    let field_name = if idx > 0 {
        format!("{}[{idx}]", field.name())
    } else {
        field.name().to_string()
    };
    let n = value.as_i64().or_else(|| {
        // serde_json::Value::as_i64 returns None for floats; allow
        // floats that have no fractional part for flatc parity.
        value.as_f64().and_then(|f| {
            if f.fract() == 0.0 && f >= min as f64 && f <= max as f64 {
                Some(f as i64)
            } else {
                None
            }
        })
    });
    let n = match n {
        Some(n) => n,
        None => {
            return Err(FromJsonError::TypeMismatch {
                field: field_name,
                expected: kind,
                got: value_kind(value),
            });
        }
    };
    if n < min || n > max {
        return Err(FromJsonError::IntegerOutOfRange {
            field: field_name,
            value: i128::from(n),
            kind,
        });
    }
    T::try_from(n).map_err(|_| FromJsonError::IntegerOutOfRange {
        field: field_name,
        value: i128::from(n),
        kind,
    })
}

fn decode_unsigned_int<T>(
    field: &Field,
    value: &Value,
    idx: usize,
    max: u64,
    kind: &'static str,
) -> Result<T, FromJsonError>
where
    T: TryFrom<u64>,
{
    let field_name = if idx > 0 {
        format!("{}[{idx}]", field.name())
    } else {
        field.name().to_string()
    };
    let n = value.as_u64();
    let n = match n {
        Some(n) => n,
        None => {
            return Err(FromJsonError::TypeMismatch {
                field: field_name,
                expected: kind,
                got: value_kind(value),
            });
        }
    };
    if n > max {
        return Err(FromJsonError::IntegerOutOfRange {
            field: field_name,
            value: i128::from(n),
            kind,
        });
    }
    T::try_from(n).map_err(|_| FromJsonError::IntegerOutOfRange {
        field: field_name,
        value: i128::from(n),
        kind,
    })
}

fn decode_float(field: &Field, value: &Value, idx: usize) -> Result<f64, FromJsonError> {
    let field_name = if idx > 0 {
        format!("{}[{idx}]", field.name())
    } else {
        field.name().to_string()
    };
    value
        .as_f64()
        .or_else(|| value.as_i64().map(|n| n as f64))
        .or_else(|| value.as_u64().map(|n| n as f64))
        .ok_or_else(|| FromJsonError::TypeMismatch {
            field: field_name,
            expected: "number",
            got: value_kind(value),
        })
}

/// Decode an enum-typed JSON value: either a member-name string or a
/// raw numeric discriminator value. Returns the resolved
/// discriminator as `i64`.
fn decode_enum_value(
    field: &Field,
    value: &Value,
    idx: usize,
    enum_def: &Enum<'_>,
) -> Result<i64, FromJsonError> {
    let field_name = if idx > 0 {
        format!("{}[{idx}]", field.name())
    } else {
        field.name().to_string()
    };
    match value {
        Value::String(s) => {
            let ev = enum_def
                .values()
                .iter()
                .find(|v| v.name() == s.as_str())
                .ok_or_else(|| FromJsonError::UnknownEnumName {
                    field: field_name,
                    enum_name: enum_def.name().to_string(),
                    value: s.clone(),
                })?;
            Ok(ev.value())
        }
        Value::Number(_) => value.as_i64().ok_or_else(|| FromJsonError::TypeMismatch {
            field: field_name,
            expected: "integer enum value",
            got: value_kind(value),
        }),
        _ => Err(FromJsonError::TypeMismatch {
            field: field_name,
            expected: "enum member name or integer",
            got: value_kind(value),
        }),
    }
}

// -- helpers --

fn is_scalar(b: BaseType) -> bool {
    matches!(
        b,
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
    )
}

fn value_kind(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn check_depth(depth: usize, max: usize) -> Result<(), FromJsonError> {
    if depth > max {
        return Err(FromJsonError::NestingTooDeep { max });
    }
    Ok(())
}

fn lookup_enum<'a>(field: &Field, schema: &'a Schema) -> Result<Enum<'a>, FromJsonError> {
    let idx = u_index(field.type_().index(), "enum index")?;
    let enums = schema.enums();
    if idx >= enums.len() {
        return Err(FromJsonError::Internal(format!(
            "enum index {idx} out of range (schema has {} enums)",
            enums.len()
        )));
    }
    Ok(enums.get(idx))
}

fn u_index(i: i32, what: &'static str) -> Result<usize, FromJsonError> {
    if i < 0 {
        return Err(FromJsonError::Internal(format!("{what} is negative ({i})")));
    }
    Ok(i as usize)
}

// ---------------------------------------------------------------------------
// Tests (pure Rust — no `cargo pgrx test` needed)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use flatbuffers::FlatBufferBuilder;
    use flatbuffers_reflection::reflection::{
        Enum, EnumArgs, EnumVal, EnumValArgs, Field as RField, FieldArgs, Object as RObject,
        ObjectArgs, Schema as RSchema, SchemaArgs, Type, TypeArgs, root_as_schema,
    };
    use serde_json::json;

    /// Build a non-struct schema covering the v0.1-supported types:
    ///
    /// ```fbs
    /// enum Color : byte { Red = 0, Green = 1, Blue = 2 }
    /// table Inner { tag:string; }
    /// table T {
    ///   name:string;
    ///   count:int = 7;
    ///   active:bool;
    ///   color:Color;
    ///   tags:[string];
    ///   nums:[int];
    ///   blob:[ubyte];
    ///   inner:Inner;
    ///   children:[Inner];
    /// }
    /// root_type T;
    /// ```
    fn build_t_schema() -> Vec<u8> {
        let mut fbb = FlatBufferBuilder::new();

        // -- types --
        let int_t = Type::create(
            &mut fbb,
            &TypeArgs {
                base_type: BaseType::Int,
                ..Default::default()
            },
        );
        let str_t = Type::create(
            &mut fbb,
            &TypeArgs {
                base_type: BaseType::String,
                ..Default::default()
            },
        );
        let bool_t = Type::create(
            &mut fbb,
            &TypeArgs {
                base_type: BaseType::Bool,
                ..Default::default()
            },
        );

        // enum Color : byte
        let red_n = fbb.create_string("Red");
        let green_n = fbb.create_string("Green");
        let blue_n = fbb.create_string("Blue");
        let byte_t = Type::create(
            &mut fbb,
            &TypeArgs {
                base_type: BaseType::Byte,
                ..Default::default()
            },
        );
        let red = EnumVal::create(
            &mut fbb,
            &EnumValArgs {
                name: Some(red_n),
                value: 0,
                ..Default::default()
            },
        );
        let green = EnumVal::create(
            &mut fbb,
            &EnumValArgs {
                name: Some(green_n),
                value: 1,
                ..Default::default()
            },
        );
        let blue = EnumVal::create(
            &mut fbb,
            &EnumValArgs {
                name: Some(blue_n),
                value: 2,
                ..Default::default()
            },
        );
        let color_values = fbb.create_vector(&[red, green, blue]);
        let color_n = fbb.create_string("Color");
        let color_enum = Enum::create(
            &mut fbb,
            &EnumArgs {
                name: Some(color_n),
                values: Some(color_values),
                underlying_type: Some(byte_t),
                is_union: false,
                ..Default::default()
            },
        );
        let color_t = Type::create(
            &mut fbb,
            &TypeArgs {
                base_type: BaseType::Byte,
                index: 0,
                ..Default::default()
            },
        );

        // table Inner { tag:string }  (object index 0)
        let tag_n = fbb.create_string("tag");
        let tag_f = RField::create(
            &mut fbb,
            &FieldArgs {
                name: Some(tag_n),
                type_: Some(str_t),
                id: 0,
                offset: 4,
                ..Default::default()
            },
        );
        let inner_fields = fbb.create_vector(&[tag_f]);
        let inner_n = fbb.create_string("Inner");
        let inner = RObject::create(
            &mut fbb,
            &ObjectArgs {
                name: Some(inner_n),
                fields: Some(inner_fields),
                ..Default::default()
            },
        );

        // Inner obj type — points at object 0 (Inner)
        let inner_obj_t = Type::create(
            &mut fbb,
            &TypeArgs {
                base_type: BaseType::Obj,
                index: 0,
                ..Default::default()
            },
        );

        // vector types
        let vec_string_t = Type::create(
            &mut fbb,
            &TypeArgs {
                base_type: BaseType::Vector,
                element: BaseType::String,
                ..Default::default()
            },
        );
        let vec_int_t = Type::create(
            &mut fbb,
            &TypeArgs {
                base_type: BaseType::Vector,
                element: BaseType::Int,
                ..Default::default()
            },
        );
        let vec_ubyte_t = Type::create(
            &mut fbb,
            &TypeArgs {
                base_type: BaseType::Vector,
                element: BaseType::UByte,
                ..Default::default()
            },
        );
        let vec_inner_t = Type::create(
            &mut fbb,
            &TypeArgs {
                base_type: BaseType::Vector,
                element: BaseType::Obj,
                index: 0,
                ..Default::default()
            },
        );

        // -- table T --
        macro_rules! f {
            ($name:expr, $type_:expr, $id:expr) => {{
                let n = fbb.create_string($name);
                RField::create(
                    &mut fbb,
                    &FieldArgs {
                        name: Some(n),
                        type_: Some($type_),
                        id: $id,
                        offset: 4 + ($id as u16) * 2,
                        ..Default::default()
                    },
                )
            }};
        }
        let name_f = f!("name", str_t, 0);
        let count_n = fbb.create_string("count");
        let count_f = RField::create(
            &mut fbb,
            &FieldArgs {
                name: Some(count_n),
                type_: Some(int_t),
                id: 1,
                offset: 6,
                default_integer: 7,
                ..Default::default()
            },
        );
        let active_f = f!("active", bool_t, 2);
        let color_f = f!("color", color_t, 3);
        let tags_f = f!("tags", vec_string_t, 4);
        let nums_f = f!("nums", vec_int_t, 5);
        let blob_f = f!("blob", vec_ubyte_t, 6);
        let inner_f = f!("inner", inner_obj_t, 7);
        let children_f = f!("children", vec_inner_t, 8);

        // Alphabetical: active, blob, children, color, count, inner, name, nums, tags.
        let t_fields = fbb.create_vector(&[
            active_f, blob_f, children_f, color_f, count_f, inner_f, name_f, nums_f, tags_f,
        ]);
        let t_n = fbb.create_string("T");
        let t_obj = RObject::create(
            &mut fbb,
            &ObjectArgs {
                name: Some(t_n),
                fields: Some(t_fields),
                ..Default::default()
            },
        );

        // Objects sorted: Inner (0), T (1).
        let objects = fbb.create_vector(&[inner, t_obj]);
        let enums = fbb.create_vector(&[color_enum]);
        let schema = RSchema::create(
            &mut fbb,
            &SchemaArgs {
                objects: Some(objects),
                enums: Some(enums),
                root_table: Some(t_obj),
                ..Default::default()
            },
        );
        fbb.finish(schema, None);
        fbb.finished_data().to_vec()
    }

    #[test]
    fn from_json_round_trips_via_to_json() {
        let bfbs = build_t_schema();
        let schema = root_as_schema(&bfbs).expect("schema parses");
        let input = json!({
            "name": "hello",
            "count": 42,
            "active": true,
            "color": "Blue",
            "tags": ["a", "b"],
            "nums": [10, 20, 30],
            "blob": "3q2+7w==",
            "inner": { "tag": "x" },
            "children": [{ "tag": "c1" }, { "tag": "c2" }],
        });
        let buf = json_to_buf(&input, &schema, 64).expect("from_json ok");
        // Verifier must accept it.
        crate::verify::verify(&buf, &schema, &crate::verify::Bounds::default())
            .expect("buffer verifies");
        // to_json round trip must equal the input.
        let value = crate::to_json::buf_to_json(&buf, &schema).expect("to_json ok");
        assert_eq!(value, input);
    }

    #[test]
    fn from_json_omits_absent_optional_fields_and_uses_defaults() {
        let bfbs = build_t_schema();
        let schema = root_as_schema(&bfbs).expect("schema parses");
        // Empty object: every nullable absent, scalar defaults
        // emitted on read.
        let buf = json_to_buf(&json!({}), &schema, 64).expect("from_json ok");
        crate::verify::verify(&buf, &schema, &crate::verify::Bounds::default())
            .expect("buffer verifies");
        let value = crate::to_json::buf_to_json(&buf, &schema).expect("to_json ok");
        assert_eq!(
            value,
            json!({ "count": 7, "active": false, "color": "Red" })
        );
    }

    #[test]
    fn from_json_rejects_unknown_field() {
        let bfbs = build_t_schema();
        let schema = root_as_schema(&bfbs).expect("schema parses");
        let err = json_to_buf(&json!({ "bogus": 1 }), &schema, 64).unwrap_err();
        assert!(
            matches!(&err, FromJsonError::UnknownField { table, field }
                if table == "T" && field == "bogus"),
            "got {err:?}"
        );
    }

    #[test]
    fn from_json_rejects_type_mismatch() {
        let bfbs = build_t_schema();
        let schema = root_as_schema(&bfbs).expect("schema parses");
        let err = json_to_buf(&json!({ "count": "forty-two" }), &schema, 64).unwrap_err();
        assert!(
            matches!(&err, FromJsonError::TypeMismatch { field, expected, got }
                if field == "count" && *expected == "i32" && *got == "string"),
            "got {err:?}"
        );
    }

    #[test]
    fn from_json_rejects_integer_out_of_range() {
        let bfbs = build_t_schema();
        let schema = root_as_schema(&bfbs).expect("schema parses");
        // color is byte-typed (enum), but accepting numeric value
        // outside i8 range should still fail.
        let err = json_to_buf(&json!({ "color": 999 }), &schema, 64).unwrap_err();
        assert!(
            matches!(&err, FromJsonError::IntegerOutOfRange { field, .. } if field == "color"),
            "got {err:?}"
        );
    }

    #[test]
    fn from_json_rejects_unknown_enum_name() {
        let bfbs = build_t_schema();
        let schema = root_as_schema(&bfbs).expect("schema parses");
        let err = json_to_buf(&json!({ "color": "Magenta" }), &schema, 64).unwrap_err();
        assert!(
            matches!(&err, FromJsonError::UnknownEnumName { field, enum_name, value }
                if field == "color" && enum_name == "Color" && value == "Magenta"),
            "got {err:?}"
        );
    }

    #[test]
    fn from_json_rejects_invalid_base64() {
        let bfbs = build_t_schema();
        let schema = root_as_schema(&bfbs).expect("schema parses");
        let err = json_to_buf(&json!({ "blob": "not valid base64!" }), &schema, 64).unwrap_err();
        assert!(
            matches!(&err, FromJsonError::InvalidBase64 { field, .. } if field == "blob"),
            "got {err:?}"
        );
    }

    #[test]
    fn from_json_rejects_root_not_object() {
        let bfbs = build_t_schema();
        let schema = root_as_schema(&bfbs).expect("schema parses");
        let err = json_to_buf(&json!([1, 2, 3]), &schema, 64).unwrap_err();
        assert!(
            matches!(&err, FromJsonError::RootNotObject { got } if *got == "array"),
            "got {err:?}"
        );
    }

    #[test]
    fn from_json_caps_nesting_at_max_depth() {
        let bfbs = build_t_schema();
        let schema = root_as_schema(&bfbs).expect("schema parses");
        // inner.tag → 1 level deep. With max_depth=0 we error out
        // at the very first descent.
        let err = json_to_buf(&json!({ "inner": { "tag": "x" } }), &schema, 0).unwrap_err();
        assert!(
            matches!(&err, FromJsonError::NestingTooDeep { max } if *max == 0),
            "got {err:?}"
        );
    }

    #[test]
    fn from_json_rejects_struct_field_as_unsupported() {
        // Build a minimal schema with an inline struct field to verify
        // the deferral message points at the struct field by name.
        let mut fbb = FlatBufferBuilder::new();
        let f32_t = Type::create(
            &mut fbb,
            &TypeArgs {
                base_type: BaseType::Float,
                ..Default::default()
            },
        );
        // struct Vec3 { x:float; }  (single-field struct, bytesize 4)
        let xn = fbb.create_string("x");
        let xf = RField::create(
            &mut fbb,
            &FieldArgs {
                name: Some(xn),
                type_: Some(f32_t),
                id: 0,
                offset: 0,
                ..Default::default()
            },
        );
        let vec3_fields = fbb.create_vector(&[xf]);
        let vec3_n = fbb.create_string("Vec3");
        let vec3 = RObject::create(
            &mut fbb,
            &ObjectArgs {
                name: Some(vec3_n),
                fields: Some(vec3_fields),
                is_struct: true,
                bytesize: 4,
                minalign: 4,
                ..Default::default()
            },
        );
        // table P { pos:Vec3; }
        let vec3_obj_t = Type::create(
            &mut fbb,
            &TypeArgs {
                base_type: BaseType::Obj,
                index: 1, // Vec3 (sorts after P)
                ..Default::default()
            },
        );
        let pos_n = fbb.create_string("pos");
        let pos_f = RField::create(
            &mut fbb,
            &FieldArgs {
                name: Some(pos_n),
                type_: Some(vec3_obj_t),
                id: 0,
                offset: 4,
                ..Default::default()
            },
        );
        let p_fields = fbb.create_vector(&[pos_f]);
        let p_n = fbb.create_string("P");
        let p_obj = RObject::create(
            &mut fbb,
            &ObjectArgs {
                name: Some(p_n),
                fields: Some(p_fields),
                ..Default::default()
            },
        );
        // Objects sorted alphabetically: P (0), Vec3 (1).
        let objects = fbb.create_vector(&[p_obj, vec3]);
        let enums = fbb.create_vector::<flatbuffers::ForwardsUOffset<Enum>>(&[]);
        let schema_off = RSchema::create(
            &mut fbb,
            &SchemaArgs {
                objects: Some(objects),
                enums: Some(enums),
                root_table: Some(p_obj),
                ..Default::default()
            },
        );
        fbb.finish(schema_off, None);
        let bfbs = fbb.finished_data().to_vec();

        let schema = root_as_schema(&bfbs).expect("schema parses");
        let err = json_to_buf(&json!({ "pos": { "x": 1.0 } }), &schema, 64).unwrap_err();
        assert!(
            matches!(&err, FromJsonError::Unsupported { field, what }
                if field == "pos" && what.contains("struct")),
            "got {err:?}"
        );
    }
}
