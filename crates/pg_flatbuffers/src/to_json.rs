//! Reflection-driven FlatBuffers → JSON walker (design §8).
//!
//! Entry point: [`buf_to_json`]. Given a verified buffer and the
//! reflected `Schema`, returns a [`serde_json::Value`] matching
//! `flatc --strict-json` conventions for the per-type encoding
//! documented in design §8.
//!
//! Encoding summary (one-to-one with the design §8 table):
//!
//! | FlatBuffers type | JSON shape |
//! | --- | --- |
//! | numeric scalars (`int8`…`int64`, `uint8`…`uint64`, `float`, `double`) | JSON number |
//! | `bool` | JSON boolean |
//! | enum (non-union) | JSON string (member name); JSON number if the value is unknown |
//! | `string` | JSON string (UTF-8) |
//! | `[ubyte]` / `[u8]` | JSON string, base64-encoded (lowercase hex if the field carries the `(hex)` attribute, per flatc) |
//! | `[T]` for other `T` | JSON array |
//! | table / struct | JSON object |
//! | vector of `(key)`-annotated tables | JSON object keyed by the `(key)` field's stringified value |
//! | union | flatc-style sibling field pair: `<name>_type` (string) + `<name>` (value) |
//! | fixed-size array | JSON array |
//!
//! ## Deferred (still rejected with [`ToJsonError::Unsupported`]):
//!
//! - Vector of unions — already rejected at the *schema* level in
//!   [`crate::verify::reject_unsupported_schema_features`].
//! - Vector64 — same.
//!
//! ## Safety contract
//!
//! The caller is responsible for having verified `buf` against
//! `schema` (via [`crate::verify::verify`]) before calling
//! [`buf_to_json`]. All `unsafe` blocks below are sound under that
//! precondition: the verifier guarantees every vtable slot offset,
//! every vector body length, every uoffset_t target, and every
//! string length-prefix lives within `buf`.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use flatbuffers::{ForwardsUOffset, Table, Vector, read_scalar_at};
use flatbuffers_reflection::reflection::{BaseType, Enum, Field, Object, Schema};
use serde_json::{Map, Number, Value};
use thiserror::Error;

/// Errors raised by [`buf_to_json`].
///
/// Verifier failures and schema-feature rejections live in
/// [`crate::verify::VerifyError`] and surface *before* the walker is
/// invoked; this error type covers only the things the walker itself
/// can fail on after a successfully-verified buffer.
#[derive(Debug, Error)]
pub enum ToJsonError {
    /// The `bfbs` schema has no `root_table`. Same shape as
    /// `VerifyError::NoRootTable`, surfaced here because the walker
    /// is the first caller after `verify` returns `Ok(())` and
    /// re-checking is cheap.
    #[error("schema has no root_table (cannot stringify a typeless buffer)")]
    NoRootTable,

    /// A reflection look-up that the verifier should have caught
    /// returned an out-of-range index. Treated as `Internal` rather
    /// than a user-facing error because it indicates a malformed
    /// `.bfbs` slipping past `validate_schema`.
    #[error("internal reflection error: {0}")]
    Internal(String),

    /// A scalar `f32`/`f64` value is `NaN` or `±Infinity` — JSON
    /// has no native representation for these. We surface them as
    /// errors rather than silently emitting JSON strings (which
    /// would break round-trip via `from_json`).
    #[error("field {field:?} has non-finite float value {value:?}; JSON has no representation")]
    NonFiniteFloat { field: String, value: String },
}

/// Top-level entry point: convert `buf` to a [`Value`] under `schema`.
///
/// `buf` MUST have been verified against `schema` (see
/// [`crate::verify::verify`]). The walker performs no bounds checking
/// of its own beyond what the FlatBuffers accessor APIs do internally;
/// passing an unverified buffer is a soundness violation.
pub fn buf_to_json(buf: &[u8], schema: &Schema) -> Result<Value, ToJsonError> {
    let root_object = schema.root_table().ok_or(ToJsonError::NoRootTable)?;
    // SAFETY: see module-level safety contract — caller has verified
    // `buf`, so `flatbuffers::root::<ForwardsUOffset<Table>>` would
    // pass. We use the lower-level `Table` constructor because the
    // executor pattern already does this and the verifier covered it.
    let root_uoffset = unsafe { read_scalar_at::<u32>(buf, 0) } as usize;
    let root_table = unsafe { Table::new(buf, root_uoffset) };
    table_to_json(&root_table, &root_object, schema)
}

/// Render a table as a JSON object. Fields are emitted in the
/// schema's field order (matching flatc's `--strict-json` output).
/// Absent nullable fields are omitted (`flatc` parity); absent
/// scalar fields are emitted with their schema default (also flatc
/// parity — the design's read-side `fill_scalar_defaults = off`
/// knob is a query-time concept that doesn't apply to JSON
/// encoding, where the reader has no other way to learn the
/// schema-declared default).
fn table_to_json(table: &Table, object: &Object, schema: &Schema) -> Result<Value, ToJsonError> {
    let mut map = Map::new();
    let fields = object.fields();
    // `Object.fields()` is sorted alphabetically by name in the
    // reflection schema; for flatc JSON parity we want
    // *schema-declared* (id) order. Collect into a Vec and re-sort
    // by `id`.
    let mut by_id: Vec<_> = (0..fields.len()).map(|i| fields.get(i)).collect();
    by_id.sort_by_key(|f| f.id());

    for field in &by_id {
        let base_type = field.type_().base_type();
        // Unions consume *two* schema fields (the auto-generated
        // `<name>_type` UType discriminator and the union value).
        // We process them at the value-field iteration and skip
        // the discriminator (it gets re-emitted in the
        // flatc-canonical style alongside the value).
        if is_union_discriminator(&fields, field) {
            continue;
        }

        let name = field.name().to_string();
        let is_present = table.vtable().get(field.offset()) != 0;

        // Unions need both the discriminator and the value, so
        // they branch off here regardless of presence.
        if base_type == BaseType::Union {
            emit_union_pair(table, field, schema, &mut map)?;
            continue;
        }

        if !is_present && is_nullable(base_type) {
            // Absent nullable field — omitted from JSON (flatc parity).
            continue;
        }

        let value = match base_type {
            // Scalars + enums (enums report their underlying scalar
            // BaseType + a non-negative `type_().index()`).
            BaseType::Bool
            | BaseType::Byte
            | BaseType::UByte
            | BaseType::Short
            | BaseType::UShort
            | BaseType::Int
            | BaseType::UInt
            | BaseType::Long
            | BaseType::ULong
            | BaseType::Float
            | BaseType::Double
            | BaseType::UType => scalar_to_json(table, field, schema, base_type)?,

            BaseType::String => {
                // SAFETY: caller verified; vtable slot validated.
                let s = unsafe { table.get::<ForwardsUOffset<&str>>(field.offset(), None) };
                match s {
                    Some(s) => Value::String(s.to_string()),
                    // Absent strings short-circuit above via
                    // `is_nullable + !is_present`; required strings
                    // are also guaranteed present by the verifier.
                    // Defensive: emit empty string rather than
                    // panicking on a verifier-tolerated edge case.
                    None => Value::String(String::new()),
                }
            }

            BaseType::Obj => {
                let child_idx = u_index(field.type_().index(), "Obj field")?;
                let child_object = schema.objects().get(child_idx);
                if child_object.is_struct() {
                    let slot = table.vtable().get(field.offset()) as usize;
                    if slot == 0 {
                        // Absent struct field — required structs are
                        // caught by the verifier; optional ones are
                        // omitted (above), but if we get here the
                        // type happens to not be nullable per our
                        // `is_nullable` table, so emit `null` to
                        // avoid silently dropping the field.
                        Value::Null
                    } else {
                        struct_to_json(table.buf(), table.loc() + slot, &child_object, schema)?
                    }
                } else {
                    // SAFETY: caller verified.
                    let sub = unsafe { table.get::<ForwardsUOffset<Table>>(field.offset(), None) };
                    match sub {
                        Some(sub_table) => table_to_json(&sub_table, &child_object, schema)?,
                        None => Value::Null,
                    }
                }
            }

            BaseType::Vector => vector_to_json(table, field, schema)?,

            // Vector64 + vectors-of-unions are rejected at the
            // schema-feature scan, so they never reach this walker.
            // Other variants fall through to a defensive error.
            BaseType::Vector64 => {
                return Err(ToJsonError::Internal(format!(
                    "Vector64 field {name:?} reached to_json — should have been \
                     rejected at verify time"
                )));
            }
            BaseType::Array => {
                // Fixed-size arrays only legally live *inside structs*
                // (flatc enforces this). Reaching them at table-field
                // position would be a malformed `.bfbs`.
                return Err(ToJsonError::Internal(format!(
                    "field {name:?} has BaseType::Array at table-field position \
                     (flatc only emits Array inside structs)"
                )));
            }
            BaseType::Union => unreachable!("handled above"),
            BaseType::None => {
                return Err(ToJsonError::Internal(format!(
                    "field {name:?} has BaseType::None (only valid as a union NONE marker)"
                )));
            }
            _ => {
                return Err(ToJsonError::Internal(format!(
                    "field {name:?} has unknown BaseType ({})",
                    base_type.0
                )));
            }
        };

        map.insert(name, value);
    }

    Ok(Value::Object(map))
}

/// Render a struct at `struct_loc` inside `buf` as a JSON object.
/// Struct fields are inlined fixed-offset (no vtable); each field's
/// byte location is `struct_loc + field.offset()`.
fn struct_to_json(
    buf: &[u8],
    struct_loc: usize,
    object: &Object,
    schema: &Schema,
) -> Result<Value, ToJsonError> {
    let mut map = Map::new();
    let fields = object.fields();
    let mut by_id: Vec<_> = (0..fields.len()).map(|i| fields.get(i)).collect();
    by_id.sort_by_key(|f| f.id());

    for field in &by_id {
        let field_loc = struct_loc + usize::from(field.offset());
        let base_type = field.type_().base_type();
        let name = field.name().to_string();

        let value = match base_type {
            BaseType::Bool
            | BaseType::Byte
            | BaseType::UByte
            | BaseType::Short
            | BaseType::UShort
            | BaseType::Int
            | BaseType::UInt
            | BaseType::Long
            | BaseType::ULong
            | BaseType::Float
            | BaseType::Double
            | BaseType::UType => struct_scalar_to_json(buf, field_loc, field, schema, base_type)?,

            BaseType::Obj => {
                // Inline nested struct.
                let child_idx = u_index(field.type_().index(), "Obj struct field")?;
                let child_object = schema.objects().get(child_idx);
                if !child_object.is_struct() {
                    return Err(ToJsonError::Internal(format!(
                        "struct field {name:?} points at non-struct object {:?}",
                        child_object.name()
                    )));
                }
                struct_to_json(buf, field_loc, &child_object, schema)?
            }

            BaseType::Array => array_to_json(buf, field_loc, field, schema)?,

            other => {
                return Err(ToJsonError::Internal(format!(
                    "struct field {name:?} has illegal type {:?} \
                     (flatc forbids strings/vectors/tables/unions inside structs)",
                    other.variant_name().unwrap_or("?")
                )));
            }
        };
        map.insert(name, value);
    }

    Ok(Value::Object(map))
}

/// Render a fixed-size array at `array_loc` as a JSON array. Element
/// type is `field.type_().element()`; element count is
/// `field.type_().fixed_length()`. Elements are contiguous, stride
/// = scalar size or struct bytesize.
fn array_to_json(
    buf: &[u8],
    array_loc: usize,
    field: &Field,
    schema: &Schema,
) -> Result<Value, ToJsonError> {
    let element_type = field.type_().element();
    let count = usize::from(field.type_().fixed_length());
    let name = field.name().to_string();

    let (elem_size, child_object) = match element_type {
        BaseType::Obj => {
            let child_idx = u_index(field.type_().index(), "Array of Obj")?;
            let child_object = schema.objects().get(child_idx);
            if !child_object.is_struct() {
                return Err(ToJsonError::Internal(format!(
                    "array field {name:?} element points at non-struct object"
                )));
            }
            let size = usize::try_from(child_object.bytesize()).map_err(|_| {
                ToJsonError::Internal(format!("array field {name:?} struct bytesize negative"))
            })?;
            (size, Some(child_object))
        }
        other => (
            scalar_size(other).ok_or_else(|| {
                ToJsonError::Internal(format!(
                    "array field {name:?} has non-scalar element {:?}",
                    other.variant_name().unwrap_or("?")
                ))
            })?,
            None,
        ),
    };

    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let elem_loc = array_loc + i * elem_size;
        let value = match (&child_object, element_type) {
            (Some(child), _) => struct_to_json(buf, elem_loc, child, schema)?,
            (None, _) => raw_scalar_to_json(buf, elem_loc, field, element_type)?,
        };
        out.push(value);
    }
    Ok(Value::Array(out))
}

/// Render a vector field (anything matching `BaseType::Vector`) as a
/// JSON array — except `[ubyte]` / `[u8]`, which is base64-encoded
/// (or lowercase-hex when the field carries the `(hex)` attribute)
/// per design §8.
fn vector_to_json(table: &Table, field: &Field, schema: &Schema) -> Result<Value, ToJsonError> {
    let element_type = field.type_().element();
    let name = field.name().to_string();

    // [ubyte] / [u8]: JSON string, base64 by default, lowercase hex
    // when `(hex)` is set on the field (matches flatc's
    // `(hex)`/`hashed` convention).
    if matches!(element_type, BaseType::UByte | BaseType::UType) {
        // SAFETY: caller verified.
        let v = unsafe { table.get::<ForwardsUOffset<Vector<u8>>>(field.offset(), None) };
        let bytes: &[u8] = match v {
            Some(vec) => vec.bytes(),
            None => return Ok(Value::Array(Vec::new())),
        };
        let encoded = if field_has_attribute(field, "hex") {
            encode_lower_hex(bytes)
        } else {
            BASE64.encode(bytes)
        };
        return Ok(Value::String(encoded));
    }

    // [bool]: vector of booleans.
    if element_type == BaseType::Bool {
        let v = unsafe { table.get::<ForwardsUOffset<Vector<bool>>>(field.offset(), None) };
        return Ok(match v {
            Some(vec) => Value::Array((0..vec.len()).map(|i| Value::Bool(vec.get(i))).collect()),
            None => Value::Array(Vec::new()),
        });
    }

    // [string]
    if element_type == BaseType::String {
        let v = unsafe {
            table.get::<ForwardsUOffset<Vector<ForwardsUOffset<&str>>>>(field.offset(), None)
        };
        return Ok(match v {
            Some(vec) => Value::Array(
                (0..vec.len())
                    .map(|i| Value::String(vec.get(i).to_string()))
                    .collect(),
            ),
            None => Value::Array(Vec::new()),
        });
    }

    // [scalar]
    macro_rules! scalar_vec {
        ($t:ty, $to_json:expr) => {{
            let v = unsafe { table.get::<ForwardsUOffset<Vector<$t>>>(field.offset(), None) };
            return Ok(match v {
                Some(vec) => {
                    let mut out = Vec::with_capacity(vec.len());
                    for i in 0..vec.len() {
                        out.push($to_json(vec.get(i))?);
                    }
                    Value::Array(out)
                }
                None => Value::Array(Vec::new()),
            });
        }};
    }
    match element_type {
        BaseType::Byte => scalar_vec!(i8, |x: i8| Ok::<_, ToJsonError>(Value::Number(x.into()))),
        BaseType::Short => scalar_vec!(i16, |x: i16| Ok::<_, ToJsonError>(Value::Number(x.into()))),
        BaseType::UShort => {
            scalar_vec!(u16, |x: u16| Ok::<_, ToJsonError>(Value::Number(x.into())))
        }
        BaseType::Int => {
            // [int] could be a vector of enum values too — but the
            // enum-name remapping is per-element symmetric with the
            // scalar arm and only kicks in for enum-typed elements
            // (which flatc reports as element=Int with index>=0).
            if field.type_().index() >= 0 {
                return vector_of_int_enum_to_json(table, field, schema);
            }
            scalar_vec!(i32, |x: i32| Ok::<_, ToJsonError>(Value::Number(x.into())))
        }
        BaseType::UInt => scalar_vec!(u32, |x: u32| Ok::<_, ToJsonError>(Value::Number(x.into()))),
        BaseType::Long => scalar_vec!(i64, |x: i64| Ok::<_, ToJsonError>(Value::Number(x.into()))),
        BaseType::ULong => scalar_vec!(u64, |x: u64| Ok::<_, ToJsonError>(Value::Number(x.into()))),
        BaseType::Float => scalar_vec!(f32, |x: f32| float_to_json_value(f64::from(x), &name)),
        BaseType::Double => scalar_vec!(f64, |x: f64| float_to_json_value(x, &name)),
        BaseType::Obj => {
            let child_idx = u_index(field.type_().index(), "Vector of Obj")?;
            let child_object = schema.objects().get(child_idx);
            if child_object.is_struct() {
                // Vector of structs: contiguous inline struct bytes
                // in the vector body, stride = child_object.bytesize().
                vector_of_struct_to_json(table, field, &child_object, schema)
            } else {
                // Vector of tables: each element is a ForwardsUOffset<Table>.
                let v = unsafe {
                    table.get::<ForwardsUOffset<Vector<ForwardsUOffset<Table>>>>(
                        field.offset(),
                        None,
                    )
                };
                let vec = match v {
                    Some(vec) => vec,
                    None => {
                        // Absent vector: object-shape if the child
                        // has a `(key)` field (empty object), array
                        // otherwise — mirrors the populated path.
                        return Ok(if find_keyed_field(&child_object).is_some() {
                            Value::Object(Map::new())
                        } else {
                            Value::Array(Vec::new())
                        });
                    }
                };
                // `(key)`-annotated child → JSON object keyed by the
                // key field's stringified value (design §8 sugar).
                // Iteration is in wire order so consecutive equal
                // keys (malformed vectors that the verifier doesn't
                // yet reject) collapse with last-write-wins; the
                // object form makes that visible rather than hiding
                // it in array indices.
                if let Some(keyed_field) = find_keyed_field(&child_object) {
                    let mut map = Map::new();
                    for i in 0..vec.len() {
                        let elem = vec.get(i);
                        // SAFETY: caller verified.
                        let key = unsafe {
                            flatbuffers_reflection::get_any_field_string(
                                &elem,
                                &keyed_field,
                                schema,
                            )
                        };
                        let body = table_to_json(&elem, &child_object, schema)?;
                        map.insert(key, body);
                    }
                    return Ok(Value::Object(map));
                }
                let mut out = Vec::with_capacity(vec.len());
                for i in 0..vec.len() {
                    out.push(table_to_json(&vec.get(i), &child_object, schema)?);
                }
                Ok(Value::Array(out))
            }
        }
        BaseType::Union => Err(ToJsonError::Internal(format!(
            "field {name:?} is a vector of unions but reached to_json \
             (should have been rejected at verify time)"
        ))),
        other => Err(ToJsonError::Internal(format!(
            "vector field {name:?} has unsupported element type {:?}",
            other.variant_name().unwrap_or("?")
        ))),
    }
}

/// Vector of inline structs: compute the body location via the
/// upstream verifier's pattern (vtable slot → uoffset → body),
/// then iterate `count` structs at stride `bytesize`.
fn vector_of_struct_to_json(
    table: &Table,
    field: &Field,
    child_object: &Object,
    schema: &Schema,
) -> Result<Value, ToJsonError> {
    let slot_offset = table.vtable().get(field.offset()) as usize;
    if slot_offset == 0 {
        return Ok(Value::Array(Vec::new()));
    }
    let buf = table.buf();
    let field_loc = table.loc() + slot_offset;
    // SAFETY: caller verified; `field_loc` + 4 is in bounds.
    let forward_offset = unsafe { read_scalar_at::<u32>(buf, field_loc) } as usize;
    let body_loc = field_loc + forward_offset;
    let count = unsafe { read_scalar_at::<u32>(buf, body_loc) } as usize;
    let bytesize = usize::try_from(child_object.bytesize()).map_err(|_| {
        ToJsonError::Internal(format!(
            "vector field {:?} struct element bytesize negative",
            field.name()
        ))
    })?;
    let mut out = Vec::with_capacity(count);
    let first_elem = body_loc + 4; // skip u32 count word
    for i in 0..count {
        let elem_loc = first_elem + i * bytesize;
        out.push(struct_to_json(buf, elem_loc, child_object, schema)?);
    }
    Ok(Value::Array(out))
}

/// Vector of `int`-typed enum values: stringify each element via the
/// enum's reflected EnumVal name. v0.1 only handles `int`-backed
/// enums in vector position; other underlying types fall through
/// the standard scalar vector path (numeric output) without name
/// remapping. (`flatc` permits enum-backed scalar types across all
/// integer widths; expanding here is a mechanical per-width
/// duplication.)
fn vector_of_int_enum_to_json(
    table: &Table,
    field: &Field,
    schema: &Schema,
) -> Result<Value, ToJsonError> {
    let enum_def = lookup_enum(field, schema)?;
    // SAFETY: caller verified.
    let v = unsafe { table.get::<ForwardsUOffset<Vector<i32>>>(field.offset(), None) };
    Ok(match v {
        Some(vec) => Value::Array(
            (0..vec.len())
                .map(|i| enum_value_to_json(i64::from(vec.get(i)), &enum_def))
                .collect(),
        ),
        None => Value::Array(Vec::new()),
    })
}

/// Stringify a scalar/enum field on a table. Enums (non-negative
/// `field.type_().index()`) are emitted as JSON strings via the
/// enum's reflected EnumVal name, with the raw numeric value as
/// fallback for unknown discriminators.
fn scalar_to_json(
    table: &Table,
    field: &Field,
    schema: &Schema,
    base_type: BaseType,
) -> Result<Value, ToJsonError> {
    let name = field.name().to_string();
    // Enum dispatch: if the field's type carries a non-negative
    // index, it points at an enum in `schema.enums()`.
    let is_enum = field.type_().index() >= 0;

    macro_rules! read_int {
        ($t:ty, $default:expr) => {{
            // SAFETY: caller verified; vtable validated.
            unsafe { table.get::<$t>(field.offset(), Some($default)) }.unwrap_or($default)
        }};
    }

    let value: i64 = match base_type {
        BaseType::Bool => {
            let d = field.default_integer() != 0;
            let v = read_int!(bool, d);
            return Ok(Value::Bool(v));
        }
        BaseType::Byte => i64::from(read_int!(i8, field.default_integer() as i8)),
        BaseType::UByte | BaseType::UType => {
            #[allow(clippy::cast_sign_loss, reason = "u8 default fits in i64 unsigned")]
            let d = field.default_integer() as u8;
            i64::from(read_int!(u8, d))
        }
        BaseType::Short => i64::from(read_int!(i16, field.default_integer() as i16)),
        BaseType::UShort => {
            let d = field.default_integer() as u16;
            i64::from(read_int!(u16, d))
        }
        BaseType::Int => i64::from(read_int!(i32, field.default_integer() as i32)),
        BaseType::UInt => {
            let d = field.default_integer() as u32;
            i64::from(read_int!(u32, d))
        }
        BaseType::Long => read_int!(i64, field.default_integer()),
        BaseType::ULong => {
            #[allow(
                clippy::cast_sign_loss,
                reason = "default_integer is the schema-declared default"
            )]
            let d = field.default_integer() as u64;
            let v = read_int!(u64, d);
            // u64 may exceed i64::MAX — handle separately for enum/non-enum.
            if is_enum {
                return Ok(enum_value_to_json(v as i64, &lookup_enum(field, schema)?));
            }
            return Ok(Value::Number(v.into()));
        }
        BaseType::Float => {
            #[allow(
                clippy::cast_possible_truncation,
                reason = "Float field default fits in f32"
            )]
            let d = field.default_real() as f32;
            let v = unsafe { table.get::<f32>(field.offset(), Some(d)) }.unwrap_or(d);
            return float_to_json_value(f64::from(v), &name);
        }
        BaseType::Double => {
            let d = field.default_real();
            let v = unsafe { table.get::<f64>(field.offset(), Some(d)) }.unwrap_or(d);
            return float_to_json_value(v, &name);
        }
        _ => {
            return Err(ToJsonError::Internal(format!(
                "scalar_to_json: unexpected base_type {:?}",
                base_type.variant_name().unwrap_or("?")
            )));
        }
    };

    if is_enum {
        Ok(enum_value_to_json(value, &lookup_enum(field, schema)?))
    } else {
        Ok(Value::Number(value.into()))
    }
}

/// Same as `scalar_to_json` but reading from a struct field (no
/// vtable; fixed byte offset). Enums work identically.
fn struct_scalar_to_json(
    buf: &[u8],
    loc: usize,
    field: &Field,
    schema: &Schema,
    base_type: BaseType,
) -> Result<Value, ToJsonError> {
    let name = field.name().to_string();
    let is_enum = field.type_().index() >= 0;

    macro_rules! read {
        ($t:ty) => {{
            // SAFETY: caller verified; struct bytesize covers this loc.
            unsafe { read_scalar_at::<$t>(buf, loc) }
        }};
    }

    let value: i64 = match base_type {
        BaseType::Bool => return Ok(Value::Bool(read!(u8) != 0)),
        BaseType::Byte => i64::from(read!(i8)),
        BaseType::UByte | BaseType::UType => i64::from(read!(u8)),
        BaseType::Short => i64::from(read!(i16)),
        BaseType::UShort => i64::from(read!(u16)),
        BaseType::Int => i64::from(read!(i32)),
        BaseType::UInt => i64::from(read!(u32)),
        BaseType::Long => read!(i64),
        BaseType::ULong => {
            let v = read!(u64);
            if is_enum {
                return Ok(enum_value_to_json(v as i64, &lookup_enum(field, schema)?));
            }
            return Ok(Value::Number(v.into()));
        }
        BaseType::Float => return float_to_json_value(f64::from(read!(f32)), &name),
        BaseType::Double => return float_to_json_value(read!(f64), &name),
        _ => {
            return Err(ToJsonError::Internal(format!(
                "struct_scalar_to_json: unexpected base_type {:?}",
                base_type.variant_name().unwrap_or("?")
            )));
        }
    };

    if is_enum {
        Ok(enum_value_to_json(value, &lookup_enum(field, schema)?))
    } else {
        Ok(Value::Number(value.into()))
    }
}

/// Generic raw-scalar reader keyed by `BaseType`, used by the array
/// element walker. Doesn't handle enum remapping — fixed-size arrays
/// of enums fall back to numeric (consistent with flatc).
fn raw_scalar_to_json(
    buf: &[u8],
    loc: usize,
    field: &Field,
    base_type: BaseType,
) -> Result<Value, ToJsonError> {
    let name = field.name().to_string();
    macro_rules! read {
        ($t:ty) => {{
            // SAFETY: caller verified; array stride was sized by
            // `array_to_json` using `scalar_size`.
            unsafe { read_scalar_at::<$t>(buf, loc) }
        }};
    }
    Ok(match base_type {
        BaseType::Bool => Value::Bool(read!(u8) != 0),
        BaseType::Byte => Value::Number(i64::from(read!(i8)).into()),
        BaseType::UByte | BaseType::UType => Value::Number(u64::from(read!(u8)).into()),
        BaseType::Short => Value::Number(i64::from(read!(i16)).into()),
        BaseType::UShort => Value::Number(u64::from(read!(u16)).into()),
        BaseType::Int => Value::Number(i64::from(read!(i32)).into()),
        BaseType::UInt => Value::Number(u64::from(read!(u32)).into()),
        BaseType::Long => Value::Number(read!(i64).into()),
        BaseType::ULong => Value::Number(read!(u64).into()),
        BaseType::Float => return float_to_json_value(f64::from(read!(f32)), &name),
        BaseType::Double => return float_to_json_value(read!(f64), &name),
        other => {
            return Err(ToJsonError::Internal(format!(
                "raw_scalar_to_json: unexpected base_type {:?}",
                other.variant_name().unwrap_or("?")
            )));
        }
    })
}

/// Emit the two flatc-canonical union fields into `map`: the
/// `<name>_type` discriminator (as the EnumVal name string) and the
/// `<name>` value (table/struct as JSON object, string as JSON
/// string). NONE produces just `<name>_type = "NONE"` (no value
/// field). Matches `flatc --strict-json` output.
fn emit_union_pair(
    table: &Table,
    value_field: &Field,
    schema: &Schema,
    map: &mut Map<String, Value>,
) -> Result<(), ToJsonError> {
    let name = value_field.name();
    let value_slot = value_field.offset();
    let disc_slot = value_slot.checked_sub(2).ok_or_else(|| {
        ToJsonError::Internal(format!(
            "union value field {name:?} has vtable offset {value_slot}; \
             expected ≥2 to leave room for the discriminator slot"
        ))
    })?;
    // SAFETY: caller verified.
    let disc = unsafe { table.get::<u8>(disc_slot, Some(0)) }.unwrap_or(0);
    let enum_def = lookup_enum(value_field, schema)?;
    let variant = enum_def
        .values()
        .lookup_by_key(disc as i64, |v, k| v.key_compare_with_value(*k))
        .ok_or_else(|| {
            ToJsonError::Internal(format!(
                "union field {name:?} discriminator {disc} not found in enum {:?}",
                enum_def.name()
            ))
        })?;

    // The discriminator field is emitted as `<name>_type` regardless
    // of its actual reflection name (flatc always uses this suffix).
    map.insert(
        format!("{name}_type"),
        Value::String(variant.name().to_string()),
    );

    if disc == 0 {
        return Ok(());
    }

    let variant_type = variant.union_type().ok_or_else(|| {
        ToJsonError::Internal(format!(
            "union variant {:?} has no union_type",
            variant.name()
        ))
    })?;

    let value = match variant_type.base_type() {
        BaseType::Obj => {
            let obj_idx = u_index(variant_type.index(), "union variant Obj")?;
            let variant_object = schema.objects().get(obj_idx);
            if variant_object.is_struct() {
                // Out-of-line struct: resolve uoffset.
                let slot_offset = table.vtable().get(value_slot) as usize;
                if slot_offset == 0 {
                    Value::Null
                } else {
                    let buf = table.buf();
                    let value_loc = table.loc() + slot_offset;
                    let forward_offset = unsafe { read_scalar_at::<u32>(buf, value_loc) } as usize;
                    let struct_loc = value_loc + forward_offset;
                    struct_to_json(buf, struct_loc, &variant_object, schema)?
                }
            } else {
                // SAFETY: caller verified.
                let sub = unsafe { table.get::<ForwardsUOffset<Table>>(value_slot, None) };
                match sub {
                    Some(sub_table) => table_to_json(&sub_table, &variant_object, schema)?,
                    None => Value::Null,
                }
            }
        }
        BaseType::String => {
            // SAFETY: caller verified.
            let s = unsafe { table.get::<ForwardsUOffset<&str>>(value_slot, None) };
            match s {
                Some(s) => Value::String(s.to_string()),
                None => Value::Null,
            }
        }
        other => {
            return Err(ToJsonError::Internal(format!(
                "union variant {:?} has unsupported base type {:?}",
                variant.name(),
                other.variant_name().unwrap_or("?")
            )));
        }
    };

    map.insert(name.to_string(), value);
    Ok(())
}

// -- helpers --

fn is_nullable(b: BaseType) -> bool {
    matches!(
        b,
        BaseType::String
            | BaseType::Obj
            | BaseType::Vector
            | BaseType::Vector64
            | BaseType::Union
            | BaseType::Array
    )
}

/// Returns true if `field` is the auto-generated `<name>_type`
/// discriminator for a sibling union value field. flatc emits these
/// as `BaseType::UType` with the same `name` suffix `_type` and a
/// sibling field whose name minus `_type` matches.
fn is_union_discriminator(
    fields: &flatbuffers::Vector<'_, flatbuffers::ForwardsUOffset<Field<'_>>>,
    field: &Field,
) -> bool {
    if field.type_().base_type() != BaseType::UType {
        return false;
    }
    let n = field.name();
    let Some(value_name) = n.strip_suffix("_type") else {
        return false;
    };
    // The sibling value field must have BaseType::Union.
    for i in 0..fields.len() {
        let f = fields.get(i);
        if f.name() == value_name && f.type_().base_type() == BaseType::Union {
            return true;
        }
    }
    false
}

fn lookup_enum<'a>(field: &Field, schema: &'a Schema) -> Result<Enum<'a>, ToJsonError> {
    let idx = u_index(field.type_().index(), "enum index")?;
    let enums = schema.enums();
    if idx >= enums.len() {
        return Err(ToJsonError::Internal(format!(
            "enum index {idx} out of range (schema has {} enums)",
            enums.len()
        )));
    }
    Ok(enums.get(idx))
}

fn enum_value_to_json(value: i64, enum_def: &Enum<'_>) -> Value {
    match enum_def
        .values()
        .lookup_by_key(value, |v, k| v.key_compare_with_value(*k))
    {
        Some(ev) => Value::String(ev.name().to_string()),
        None => Value::Number(value.into()),
    }
}

fn float_to_json_value(v: f64, field_name: &str) -> Result<Value, ToJsonError> {
    Number::from_f64(v).map(Value::Number).ok_or_else(|| {
        let value = if v.is_nan() {
            "NaN".to_string()
        } else if v.is_sign_positive() {
            "Infinity".to_string()
        } else {
            "-Infinity".to_string()
        };
        ToJsonError::NonFiniteFloat {
            field: field_name.to_string(),
            value,
        }
    })
}

fn u_index(i: i32, what: &'static str) -> Result<usize, ToJsonError> {
    if i < 0 {
        return Err(ToJsonError::Internal(format!("{what} is negative ({i})")));
    }
    Ok(i as usize)
}

/// Returns true if `field` has an attribute whose key matches
/// `name` (case-sensitive; flatc attribute keys are stored verbatim
/// in the `.bfbs`). The attribute's value (if any) is ignored —
/// flatc's flag-style attributes like `(hex)`, `(required)`, and
/// `(key)` carry no value, and the reflection encoding stores them
/// as a `KeyValue` with the key set and an empty/absent value.
fn field_has_attribute(field: &Field, name: &str) -> bool {
    let Some(attrs) = field.attributes() else {
        return false;
    };
    (0..attrs.len()).any(|i| attrs.get(i).key() == name)
}

/// Look up the `(key)`-annotated field on a child table, used to
/// drive the design §8 "vector of `(key)`-annotated tables → JSON
/// object" sugar. FlatBuffers / flatc enforce at most one `(key)`
/// field per table at schema-compile time; we still walk all
/// fields rather than relying on that invariant. Returns `None`
/// for plain tables (the caller falls back to the JSON-array
/// rendering).
fn find_keyed_field<'a>(child_object: &'a Object) -> Option<Field<'a>> {
    let fields = child_object.fields();
    for i in 0..fields.len() {
        let f = fields.get(i);
        if f.key() {
            return Some(f);
        }
    }
    None
}

/// Encode `bytes` as a lowercase hexadecimal string with no
/// separators or `0x` prefix — flatc's `(hex)` convention for
/// `[ubyte]` fields.
fn encode_lower_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

fn scalar_size(b: BaseType) -> Option<usize> {
    Some(match b {
        BaseType::Bool | BaseType::Byte | BaseType::UByte | BaseType::UType => 1,
        BaseType::Short | BaseType::UShort => 2,
        BaseType::Int | BaseType::UInt | BaseType::Float => 4,
        BaseType::Long | BaseType::ULong | BaseType::Double => 8,
        _ => return None,
    })
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

    /// Build the reflection schema for:
    ///
    /// ```fbs
    /// enum Color : byte { Red = 0, Green = 1, Blue = 2 }
    /// struct Vec3 { x:float; y:float; z:float; }    // bytesize 12
    /// table T {
    ///   name:string;
    ///   count:int = 7;
    ///   active:bool;
    ///   color:Color;
    ///   pos:Vec3;
    ///   tags:[string];
    ///   nums:[int];
    ///   blob:[ubyte];
    ///   ratio:double;
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
        let f32_t = Type::create(
            &mut fbb,
            &TypeArgs {
                base_type: BaseType::Float,
                ..Default::default()
            },
        );
        let f64_t = Type::create(
            &mut fbb,
            &TypeArgs {
                base_type: BaseType::Double,
                ..Default::default()
            },
        );

        // -- enum Color : byte --
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
                index: 0, // enum index
                ..Default::default()
            },
        );

        // -- struct Vec3 { x:float @0; y:float @4; z:float @8; } --
        // Object index 1 (Vec3 sorts after T).
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
        let yn = fbb.create_string("y");
        let yf = RField::create(
            &mut fbb,
            &FieldArgs {
                name: Some(yn),
                type_: Some(f32_t),
                id: 1,
                offset: 4,
                ..Default::default()
            },
        );
        let zn = fbb.create_string("z");
        let zf = RField::create(
            &mut fbb,
            &FieldArgs {
                name: Some(zn),
                type_: Some(f32_t),
                id: 2,
                offset: 8,
                ..Default::default()
            },
        );
        let vec3_fields = fbb.create_vector(&[xf, yf, zf]);
        let vec3_n = fbb.create_string("Vec3");
        let vec3 = RObject::create(
            &mut fbb,
            &ObjectArgs {
                name: Some(vec3_n),
                fields: Some(vec3_fields),
                is_struct: true,
                bytesize: 12,
                minalign: 4,
                ..Default::default()
            },
        );
        let vec3_obj_t = Type::create(
            &mut fbb,
            &TypeArgs {
                base_type: BaseType::Obj,
                index: 1, // Vec3 object index
                ..Default::default()
            },
        );

        // -- [string] / [int] / [ubyte] vector types --
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

        // -- table T --
        // Field IDs determine emission order in JSON. Use sequential
        // ids that match the schema declaration above.
        // Slot offsets are 4 + 2*id (each field gets a 2-byte vtable slot,
        // starting at slot 4 = first field after vtable_size + table_size).
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
        // count:int = 7 — declared default.
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
        let pos_f = f!("pos", vec3_obj_t, 4);
        let tags_f = f!("tags", vec_string_t, 5);
        let nums_f = f!("nums", vec_int_t, 6);
        let blob_f = f!("blob", vec_ubyte_t, 7);
        let ratio_f = f!("ratio", f64_t, 8);

        // Reflection wants fields sorted alphabetically by name.
        // WIPOffsets are opaque so we can't sort by reading the
        // underlying name back — hard-code the alphabetical order
        // (active, blob, color, count, name, nums, pos, ratio, tags).
        let t_fields = fbb.create_vector(&[
            active_f, blob_f, color_f, count_f, name_f, nums_f, pos_f, ratio_f, tags_f,
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

        // Object vector sorted: T (0), Vec3 (1).
        let objects = fbb.create_vector(&[t_obj, vec3]);
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

    /// Build a buffer matching `build_t_schema` populated with
    /// reference values. Slot offsets match the field id pattern
    /// above (offset = 4 + id*2).
    fn build_t_buf() -> Vec<u8> {
        let mut fbb = FlatBufferBuilder::new();

        // Strings/vectors must be built before the table.
        let name_off = fbb.create_string("hello");
        let tag1 = fbb.create_string("alpha");
        let tag2 = fbb.create_string("beta");
        let tags_off = fbb.create_vector(&[tag1, tag2]);
        let nums_off = fbb.create_vector::<i32>(&[10, 20, 30]);
        let blob_off = fbb.create_vector::<u8>(&[0xDE, 0xAD, 0xBE, 0xEF]);

        let t = fbb.start_table();
        fbb.push_slot_always(4, name_off); // name (id 0)
        fbb.push_slot::<i32>(6, 42, 7); // count (id 1; default 7)
        fbb.push_slot::<bool>(8, true, false); // active (id 2)
        fbb.push_slot::<i8>(10, 2, 0); // color = Blue (id 3)
        // Struct pos: write inline via `push_slot_always` of a Push impl.
        // Use raw bytes; alignment 4.
        {
            // Build the struct out-of-line, then push the slot as
            // inline bytes? Structs in TABLE fields are inlined, so
            // we use `push_slot_always` with a struct-Push impl.
            #[repr(C, packed)]
            struct WireVec3 {
                x: f32,
                y: f32,
                z: f32,
            }
            impl flatbuffers::Push for WireVec3 {
                type Output = WireVec3;
                unsafe fn push(&self, dst: &mut [u8], _: usize) {
                    dst[..4].copy_from_slice(&self.x.to_le_bytes());
                    dst[4..8].copy_from_slice(&self.y.to_le_bytes());
                    dst[8..12].copy_from_slice(&self.z.to_le_bytes());
                }
                fn size() -> usize {
                    12
                }
                fn alignment() -> flatbuffers::PushAlignment {
                    flatbuffers::PushAlignment::new(4)
                }
            }
            fbb.push_slot_always(
                12,
                WireVec3 {
                    x: 1.0,
                    y: 2.0,
                    z: 3.0,
                },
            );
        }
        fbb.push_slot_always(14, tags_off); // tags (id 5)
        fbb.push_slot_always(16, nums_off); // nums (id 6)
        fbb.push_slot_always(18, blob_off); // blob (id 7)
        fbb.push_slot::<f64>(20, 2.5, 0.0); // ratio (id 8)
        let root = fbb.end_table(t);
        fbb.finish_minimal(root);
        fbb.finished_data().to_vec()
    }

    #[test]
    fn to_json_emits_all_field_kinds() {
        let bfbs = build_t_schema();
        let buf = build_t_buf();
        let schema = root_as_schema(&bfbs).expect("schema parses");

        // Sanity: verifier accepts the buffer.
        crate::verify::verify(&buf, &schema, &crate::verify::Bounds::default())
            .expect("buffer verifies");

        let value = buf_to_json(&buf, &schema).expect("to_json ok");
        assert_eq!(
            value,
            json!({
                "name": "hello",
                "count": 42,
                "active": true,
                "color": "Blue",
                "pos": { "x": 1.0, "y": 2.0, "z": 3.0 },
                "tags": ["alpha", "beta"],
                "nums": [10, 20, 30],
                "blob": "3q2+7w==",  // base64 of DEADBEEF
                "ratio": 2.5,
            })
        );
    }

    #[test]
    fn to_json_omits_absent_optional_fields_and_keeps_defaults_for_scalars() {
        let bfbs = build_t_schema();
        let schema = root_as_schema(&bfbs).expect("schema parses");
        // Empty table — every nullable field is absent; scalar
        // defaults still emit.
        let mut fbb = FlatBufferBuilder::new();
        let t = fbb.start_table();
        let root = fbb.end_table(t);
        fbb.finish_minimal(root);
        let buf = fbb.finished_data().to_vec();
        let value = buf_to_json(&buf, &schema).expect("to_json ok");
        // Nullable (name, pos, tags, nums, blob) are omitted.
        // Scalar defaults (count = 7, active = false, color = Red,
        // ratio = 0) are emitted.
        assert_eq!(
            value,
            json!({
                "count": 7,
                "active": false,
                "color": "Red",
                "ratio": 0.0,
            })
        );
    }

    #[test]
    fn float_to_json_rejects_non_finite() {
        let err = float_to_json_value(f64::INFINITY, "ratio").unwrap_err();
        assert!(
            matches!(&err, ToJsonError::NonFiniteFloat { field, value }
                if field == "ratio" && value == "Infinity"),
            "got {err:?}"
        );
        let err = float_to_json_value(f64::NAN, "ratio").unwrap_err();
        assert!(
            matches!(&err, ToJsonError::NonFiniteFloat { field, value }
                if field == "ratio" && value == "NaN"),
            "got {err:?}"
        );
        let err = float_to_json_value(f64::NEG_INFINITY, "ratio").unwrap_err();
        assert!(
            matches!(&err, ToJsonError::NonFiniteFloat { field, value }
                if field == "ratio" && value == "-Infinity"),
            "got {err:?}"
        );
    }

    /// Build a minimal `table H { blob:[ubyte] (hex); }` reflection
    /// schema. The `(hex)` attribute on `blob` is wired as a
    /// `KeyValue { key: "hex" }` entry on the field's
    /// `attributes` slot — matches what `flatc -b --schema` emits
    /// for an `.fbs` containing `blob:[ubyte] (hex);`.
    fn build_hex_blob_schema() -> Vec<u8> {
        let mut fbb = FlatBufferBuilder::new();
        let ubyte_t = Type::create(
            &mut fbb,
            &TypeArgs {
                base_type: BaseType::UByte,
                ..Default::default()
            },
        );
        let vec_ubyte_t = Type::create(
            &mut fbb,
            &TypeArgs {
                base_type: BaseType::Vector,
                element: BaseType::UByte,
                index: -1,
                ..Default::default()
            },
        );
        let _ = ubyte_t;
        // attributes: [ KeyValue { key: "hex" } ]
        let hex_key = fbb.create_string("hex");
        let hex_kv = flatbuffers_reflection::reflection::KeyValue::create(
            &mut fbb,
            &flatbuffers_reflection::reflection::KeyValueArgs {
                key: Some(hex_key),
                value: None,
            },
        );
        let attrs = fbb.create_vector(&[hex_kv]);
        let blob_n = fbb.create_string("blob");
        let blob_f = RField::create(
            &mut fbb,
            &FieldArgs {
                name: Some(blob_n),
                type_: Some(vec_ubyte_t),
                id: 0,
                offset: 4,
                attributes: Some(attrs),
                ..Default::default()
            },
        );
        let h_fields = fbb.create_vector(&[blob_f]);
        let h_n = fbb.create_string("H");
        let h_obj = RObject::create(
            &mut fbb,
            &ObjectArgs {
                name: Some(h_n),
                fields: Some(h_fields),
                ..Default::default()
            },
        );
        let objects = fbb.create_vector(&[h_obj]);
        let enums: flatbuffers::WIPOffset<
            flatbuffers::Vector<'_, flatbuffers::ForwardsUOffset<Enum<'_>>>,
        > = fbb.create_vector::<flatbuffers::ForwardsUOffset<Enum<'_>>>(&[]);
        let schema = RSchema::create(
            &mut fbb,
            &SchemaArgs {
                objects: Some(objects),
                enums: Some(enums),
                root_table: Some(h_obj),
                ..Default::default()
            },
        );
        fbb.finish(schema, None);
        fbb.finished_data().to_vec()
    }

    /// A `[ubyte]` field carrying the `(hex)` attribute renders as
    /// a lowercase hex string (no `0x` prefix, no separators) —
    /// matches `flatc --strict-json` output for `(hex)`-annotated
    /// fields.
    #[test]
    fn to_json_renders_hex_attribute_as_lowercase_hex_string() {
        let bfbs = build_hex_blob_schema();
        let schema = root_as_schema(&bfbs).expect("schema parses");

        // Build a buffer with blob = [0xDE, 0xAD, 0xBE, 0xEF].
        let mut fbb = FlatBufferBuilder::new();
        let blob_off = fbb.create_vector::<u8>(&[0xDE, 0xAD, 0xBE, 0xEF]);
        let t = fbb.start_table();
        fbb.push_slot_always(4, blob_off);
        let root = fbb.end_table(t);
        fbb.finish_minimal(root);
        let buf = fbb.finished_data().to_vec();

        crate::verify::verify(&buf, &schema, &crate::verify::Bounds::default())
            .expect("buffer verifies");
        let value = buf_to_json(&buf, &schema).expect("to_json ok");
        assert_eq!(value, json!({ "blob": "deadbeef" }));
    }

    /// `field_has_attribute` is the lookup that drives `(hex)`
    /// dispatch (and is reusable for future flag-style attribute
    /// honoring). Pins case-sensitive match + absent-attributes
    /// short-circuit.
    #[test]
    fn field_has_attribute_lookup() {
        let bfbs = build_hex_blob_schema();
        let schema = root_as_schema(&bfbs).expect("schema parses");
        let blob = schema.root_table().unwrap().fields().get(0);
        assert!(field_has_attribute(&blob, "hex"));
        assert!(!field_has_attribute(&blob, "Hex")); // case-sensitive
        assert!(!field_has_attribute(&blob, "key"));
    }

    /// Lowercase-hex encoder is the symmetric inverse of
    /// [`crate::from_json::decode_hex_string`]; round-tripping
    /// arbitrary bytes through it must produce the same bytes.
    #[test]
    fn encode_lower_hex_zero_padded() {
        assert_eq!(encode_lower_hex(&[]), "");
        assert_eq!(encode_lower_hex(&[0x00]), "00");
        assert_eq!(encode_lower_hex(&[0x0f, 0xf0]), "0ff0");
        assert_eq!(encode_lower_hex(&[0xde, 0xad, 0xbe, 0xef]), "deadbeef");
    }

    /// Build a `table Bag { items: [Item]; }` reflection schema
    /// where `table Item { sku: string (key); qty: int; }`. Used
    /// for the `(key)`-vector → JSON object sugar tests in both
    /// directions.
    fn build_keyed_bag_schema() -> Vec<u8> {
        let mut fbb = FlatBufferBuilder::new();
        let str_t = Type::create(
            &mut fbb,
            &TypeArgs {
                base_type: BaseType::String,
                ..Default::default()
            },
        );
        let int_t = Type::create(
            &mut fbb,
            &TypeArgs {
                base_type: BaseType::Int,
                ..Default::default()
            },
        );

        // Item.sku: string (key)
        let sku_n = fbb.create_string("sku");
        let sku_f = RField::create(
            &mut fbb,
            &FieldArgs {
                name: Some(sku_n),
                type_: Some(str_t),
                id: 0,
                offset: 4,
                key: true,
                ..Default::default()
            },
        );
        // Item.qty: int
        let qty_n = fbb.create_string("qty");
        let qty_f = RField::create(
            &mut fbb,
            &FieldArgs {
                name: Some(qty_n),
                type_: Some(int_t),
                id: 1,
                offset: 6,
                ..Default::default()
            },
        );
        // Fields alphabetical: qty < sku.
        let item_fields = fbb.create_vector(&[qty_f, sku_f]);
        let item_n = fbb.create_string("Item");
        let item = RObject::create(
            &mut fbb,
            &ObjectArgs {
                name: Some(item_n),
                fields: Some(item_fields),
                ..Default::default()
            },
        );

        // Bag.items: [Item] (object index 1 once sorted: Bag, Item)
        let vec_item_t = Type::create(
            &mut fbb,
            &TypeArgs {
                base_type: BaseType::Vector,
                element: BaseType::Obj,
                index: 1,
                ..Default::default()
            },
        );
        let items_n = fbb.create_string("items");
        let items_f = RField::create(
            &mut fbb,
            &FieldArgs {
                name: Some(items_n),
                type_: Some(vec_item_t),
                id: 0,
                offset: 4,
                ..Default::default()
            },
        );
        let bag_fields = fbb.create_vector(&[items_f]);
        let bag_n = fbb.create_string("Bag");
        let bag = RObject::create(
            &mut fbb,
            &ObjectArgs {
                name: Some(bag_n),
                fields: Some(bag_fields),
                ..Default::default()
            },
        );
        let objects = fbb.create_vector(&[bag, item]);
        let enums: flatbuffers::WIPOffset<
            flatbuffers::Vector<'_, flatbuffers::ForwardsUOffset<Enum<'_>>>,
        > = fbb.create_vector::<flatbuffers::ForwardsUOffset<Enum<'_>>>(&[]);
        let schema = RSchema::create(
            &mut fbb,
            &SchemaArgs {
                objects: Some(objects),
                enums: Some(enums),
                root_table: Some(bag),
                ..Default::default()
            },
        );
        fbb.finish(schema, None);
        fbb.finished_data().to_vec()
    }

    /// Build a `Bag` populated with two `Item` entries, in
    /// sku-sorted wire order. The reader-side (key) bisect is the
    /// usual consumer of this shape.
    fn build_keyed_bag_buf() -> Vec<u8> {
        let mut fbb = FlatBufferBuilder::new();
        // Sorted by sku: "apple" < "banana".
        let sku0 = fbb.create_string("apple");
        let it0 = {
            let t = fbb.start_table();
            fbb.push_slot_always(4, sku0);
            fbb.push_slot::<i32>(6, 1, 0);
            fbb.end_table(t)
        };
        let sku1 = fbb.create_string("banana");
        let it1 = {
            let t = fbb.start_table();
            fbb.push_slot_always(4, sku1);
            fbb.push_slot::<i32>(6, 2, 0);
            fbb.end_table(t)
        };
        let items = fbb.create_vector(&[it0, it1]);
        let bag = {
            let t = fbb.start_table();
            fbb.push_slot_always(4, items);
            fbb.end_table(t)
        };
        fbb.finish_minimal(bag);
        fbb.finished_data().to_vec()
    }

    /// A `(key)`-annotated vector of tables renders as a JSON
    /// *object* keyed by the key field's stringified value, with
    /// each entry's body rendered as the rest of the table —
    /// matches design §8.
    #[test]
    fn to_json_keyed_vector_emits_object_sugar() {
        let bfbs = build_keyed_bag_schema();
        let buf = build_keyed_bag_buf();
        let schema = root_as_schema(&bfbs).expect("schema parses");
        crate::verify::verify(&buf, &schema, &crate::verify::Bounds::default())
            .expect("buffer verifies");
        let value = buf_to_json(&buf, &schema).expect("to_json ok");
        // Note: each entry still carries its `sku` so the JSON is
        // unambiguous when decoded by a consumer that doesn't know
        // about the sugar.
        assert_eq!(
            value,
            json!({
                "items": {
                    "apple":  { "sku": "apple",  "qty": 1 },
                    "banana": { "sku": "banana", "qty": 2 },
                }
            })
        );
    }

    /// An absent `(key)`-annotated vector field is omitted from
    /// the parent JSON object entirely (vectors are nullable —
    /// same behavior as a plain `[T]` vector).
    #[test]
    fn to_json_keyed_vector_absent_omits_field() {
        let bfbs = build_keyed_bag_schema();
        let schema = root_as_schema(&bfbs).expect("schema parses");
        let mut fbb = FlatBufferBuilder::new();
        let bag = {
            let t = fbb.start_table();
            fbb.end_table(t)
        };
        fbb.finish_minimal(bag);
        let buf = fbb.finished_data().to_vec();
        let value = buf_to_json(&buf, &schema).expect("to_json ok");
        assert_eq!(value, json!({}));
    }
}
