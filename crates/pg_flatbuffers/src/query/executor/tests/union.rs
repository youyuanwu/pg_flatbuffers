use super::*;

// -----------------------------------------------------------------
// Union dispatch fixtures + tests (design §4.3, §7.2).
// -----------------------------------------------------------------

/// Build the reflected schema:
///
/// ```fbs
/// table  A   { name:string;          }   // object index 0
/// table  B   { count:int;            }   // object index 1
/// struct S   { x:int;                }   // object index 3 (after Msg)
/// union  U   { A, B, S, string       }   // enum   index 0
/// table  Msg { body:U;               }   // object index 2
///                                          //   body_type:UType @ slot 4
///                                          //   body:Union     @ slot 6
/// root_type Msg;
/// ```
///
/// Object vector is sorted alphabetically: `A` (0), `B` (1),
/// `Msg` (2), `S` (3). Enum vector has a single entry: `U` (0).
/// Within `U`, `EnumVal`s are sorted by `value`: `NONE` (0),
/// `A` (1), `B` (2), `S` (3), `string` (4) — that ordering is
/// what lets `walk_union` use [`flatbuffers::Vector::lookup_by_key`]
/// for the discriminator.
///
/// Variant kinds covered:
///
/// - `A`, `B`: table variants (descent via `walk_table`).
/// - `S`: struct variant (descent via `walk_struct`, with the
///   union value slot holding a forward `uoffset_t` to
///   *out-of-line* struct bytes).
/// - `string`: string variant (no descent — leaf form only).
fn build_union_schema() -> Vec<u8> {
    let mut fbb = FlatBufferBuilder::new();

    // Scalar types.
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

    // table A { name:string; }  -> object index 0
    let a_name_n = fbb.create_string("name");
    let a_name = RField::create(
        &mut fbb,
        &FieldArgs {
            name: Some(a_name_n),
            type_: Some(str_t),
            id: 0,
            offset: 4,
            ..Default::default()
        },
    );
    let a_fields = fbb.create_vector(&[a_name]);
    let a_n = fbb.create_string("A");
    let a = RObject::create(
        &mut fbb,
        &ObjectArgs {
            name: Some(a_n),
            fields: Some(a_fields),
            ..Default::default()
        },
    );

    // table B { count:int; }  -> object index 1
    let b_count_n = fbb.create_string("count");
    let b_count = RField::create(
        &mut fbb,
        &FieldArgs {
            name: Some(b_count_n),
            type_: Some(int_t),
            id: 0,
            offset: 4,
            ..Default::default()
        },
    );
    let b_fields = fbb.create_vector(&[b_count]);
    let b_n = fbb.create_string("B");
    let b = RObject::create(
        &mut fbb,
        &ObjectArgs {
            name: Some(b_n),
            fields: Some(b_fields),
            ..Default::default()
        },
    );

    // struct S { x:int @0; }  -> object index 3 (alphabetically after Msg).
    // bytesize 4, minalign 4 — single i32 field.
    let s_x_n = fbb.create_string("x");
    let s_x = RField::create(
        &mut fbb,
        &FieldArgs {
            name: Some(s_x_n),
            type_: Some(int_t),
            id: 0,
            offset: 0,
            ..Default::default()
        },
    );
    let s_fields = fbb.create_vector(&[s_x]);
    let s_n = fbb.create_string("S");
    let s = RObject::create(
        &mut fbb,
        &ObjectArgs {
            name: Some(s_n),
            fields: Some(s_fields),
            is_struct: true,
            bytesize: 4,
            minalign: 4,
            ..Default::default()
        },
    );

    // enum U { NONE=0, A=1, B=2 } as a union (enum index 0).
    let none_n = fbb.create_string("NONE");
    // The NONE variant has a `union_type` of `BaseType::None` —
    // it never resolves to an object, so we don't need an
    // `index` here, but we *do* emit it to mirror flatc output.
    let none_t = Type::create(
        &mut fbb,
        &TypeArgs {
            base_type: BaseType::None,
            ..Default::default()
        },
    );
    let none_ev = EnumVal::create(
        &mut fbb,
        &EnumValArgs {
            name: Some(none_n),
            value: 0,
            union_type: Some(none_t),
            ..Default::default()
        },
    );

    let a_variant_n = fbb.create_string("A");
    let a_obj_t = Type::create(
        &mut fbb,
        &TypeArgs {
            base_type: BaseType::Obj,
            index: 0, // points at object A
            ..Default::default()
        },
    );
    let a_ev = EnumVal::create(
        &mut fbb,
        &EnumValArgs {
            name: Some(a_variant_n),
            value: 1,
            union_type: Some(a_obj_t),
            ..Default::default()
        },
    );

    let b_variant_n = fbb.create_string("B");
    let b_obj_t = Type::create(
        &mut fbb,
        &TypeArgs {
            base_type: BaseType::Obj,
            index: 1, // points at object B
            ..Default::default()
        },
    );
    let b_ev = EnumVal::create(
        &mut fbb,
        &EnumValArgs {
            name: Some(b_variant_n),
            value: 2,
            union_type: Some(b_obj_t),
            ..Default::default()
        },
    );

    // Struct variant: value=3, union_type points at object S
    // (index 3 — alphabetical order puts S after Msg).
    let s_variant_n = fbb.create_string("S");
    let s_obj_t = Type::create(
        &mut fbb,
        &TypeArgs {
            base_type: BaseType::Obj,
            index: 3, // points at object S
            ..Default::default()
        },
    );
    let s_ev = EnumVal::create(
        &mut fbb,
        &EnumValArgs {
            name: Some(s_variant_n),
            value: 3,
            union_type: Some(s_obj_t),
            ..Default::default()
        },
    );

    // String variant: value=4, union_type has BaseType::String
    // and index = -1 (no enclosing object). Mirrors how flatc
    // emits string variants in `.bfbs`.
    let str_variant_n = fbb.create_string("string");
    let str_t_var = Type::create(
        &mut fbb,
        &TypeArgs {
            base_type: BaseType::String,
            index: -1,
            ..Default::default()
        },
    );
    let str_ev = EnumVal::create(
        &mut fbb,
        &EnumValArgs {
            name: Some(str_variant_n),
            value: 4,
            union_type: Some(str_t_var),
            ..Default::default()
        },
    );

    // EnumVals stored sorted by `value` (already 0/1/2/3/4).
    let u_values = fbb.create_vector(&[none_ev, a_ev, b_ev, s_ev, str_ev]);
    let u_underlying = Type::create(
        &mut fbb,
        &TypeArgs {
            base_type: BaseType::UType,
            index: 0, // self-reference is harmless here
            ..Default::default()
        },
    );
    let u_n = fbb.create_string("U");
    let u_enum = Enum::create(
        &mut fbb,
        &EnumArgs {
            name: Some(u_n),
            values: Some(u_values),
            is_union: true,
            underlying_type: Some(u_underlying),
            ..Default::default()
        },
    );

    // table Msg { body:U; }  -> object index 2
    // body_type:UType  @ slot 4 (id 0)
    // body:Union       @ slot 6 (id 1)
    let body_type_n = fbb.create_string("body_type");
    let body_utype_t = Type::create(
        &mut fbb,
        &TypeArgs {
            base_type: BaseType::UType,
            index: 0, // points at enum U
            ..Default::default()
        },
    );
    let body_type_f = RField::create(
        &mut fbb,
        &FieldArgs {
            name: Some(body_type_n),
            type_: Some(body_utype_t),
            id: 0,
            offset: 4,
            ..Default::default()
        },
    );

    let body_n = fbb.create_string("body");
    let body_union_t = Type::create(
        &mut fbb,
        &TypeArgs {
            base_type: BaseType::Union,
            index: 0, // points at enum U
            ..Default::default()
        },
    );
    let body_f = RField::create(
        &mut fbb,
        &FieldArgs {
            name: Some(body_n),
            type_: Some(body_union_t),
            id: 1,
            offset: 6,
            ..Default::default()
        },
    );

    // Field vector sorted alphabetically: "body" < "body_type".
    let msg_fields = fbb.create_vector(&[body_f, body_type_f]);
    let msg_n = fbb.create_string("Msg");
    let msg = RObject::create(
        &mut fbb,
        &ObjectArgs {
            name: Some(msg_n),
            fields: Some(msg_fields),
            ..Default::default()
        },
    );

    // Object vector sorted: A, B, Msg, S.
    let objects = fbb.create_vector(&[a, b, msg, s]);
    let enums = fbb.create_vector(&[u_enum]);
    let schema = RSchema::create(
        &mut fbb,
        &SchemaArgs {
            objects: Some(objects),
            enums: Some(enums),
            root_table: Some(msg),
            ..Default::default()
        },
    );
    fbb.finish(schema, None);
    fbb.finished_data().to_vec()
}

/// Pick which variant (and what payload) to write into a `Msg`
/// buffer built by [`build_msg_buf`].
enum UnionVariant<'a> {
    /// Discriminator 0; both `body_type` and `body` slots are
    /// omitted (matches the wire shape flatc emits for a NONE
    /// union).
    None,
    /// Discriminator 1; pushes a `TableA` into `body` with the
    /// given `name`.
    A(&'a str),
    /// Discriminator 2; pushes a `TableB` into `body` with the
    /// given `count`.
    B(i32),
    /// Discriminator 3; pushes a `struct S { x:int }` *out-of-line*
    /// into `body` (the union value slot then holds a forward
    /// `uoffset_t` to the struct bytes — same layout as a
    /// table-typed variant, distinct from a struct *field* which
    /// would be inlined into its parent table's body).
    S(i32),
    /// Discriminator 4; pushes a UTF-8 `string` into `body`. The
    /// union value slot holds a forward `uoffset_t` to a
    /// length-prefixed string body.
    Str(&'a str),
}

/// Wire-format wrapper used to push a `struct S { x:int }` body
/// into the FlatBuffer with the correct alignment and stride. The
/// upstream Rust API only exposes typed `push` for types
/// implementing [`flatbuffers::Push`]; for a reflection-driven
/// struct we provide the impl ourselves rather than going through
/// flatc-generated code.
#[repr(transparent)]
struct WireS(i32);

impl flatbuffers::Push for WireS {
    type Output = WireS;
    // Caller has made `size()` bytes available in `dst`.
    // We write exactly 4 little-endian bytes.
    unsafe fn push(&self, dst: &mut [u8], _written_len: usize) {
        dst[..4].copy_from_slice(&self.0.to_le_bytes());
    }
    fn size() -> usize {
        4
    }
}
/// Build a `Msg` buffer for the schema produced by
/// [`build_union_schema`]. Slot 4 holds the `u8` discriminator
/// and slot 6 holds the union value (a forward `uoffset_t` to the
/// variant's content — a sub-table for `A`/`B`, out-of-line
/// struct bytes for `S`, or a length-prefixed string for `Str`).
fn build_msg_buf(variant: UnionVariant<'_>) -> Vec<u8> {
    let mut fbb = FlatBufferBuilder::new();

    // Helper: write the Msg table with the discriminator at slot
    // 4 and the value slot at 6, pointing at the already-pushed
    // `value_off`. Generic over the value's `WIPOffset<T>` so the
    // same finisher works for table / struct / string variants.
    fn finish<'a, T>(
        mut fbb: FlatBufferBuilder<'a>,
        disc: u8,
        value_off: flatbuffers::WIPOffset<T>,
    ) -> Vec<u8> {
        let t = fbb.start_table();
        fbb.push_slot::<u8>(4, disc, 0);
        fbb.push_slot_always(6, value_off);
        let msg = fbb.end_table(t);
        fbb.finish_minimal(msg);
        fbb.finished_data().to_vec()
    }

    match variant {
        UnionVariant::None => {
            // Both slots omitted; the schema's default `disc = 0`
            // makes the union resolve to NONE on read.
            let t = fbb.start_table();
            let msg = fbb.end_table(t);
            fbb.finish_minimal(msg);
            fbb.finished_data().to_vec()
        }
        UnionVariant::A(name) => {
            let name_off = fbb.create_string(name);
            let t = fbb.start_table();
            fbb.push_slot_always(4, name_off);
            let off = fbb.end_table(t);
            finish(fbb, 1, off)
        }
        UnionVariant::B(count) => {
            let t = fbb.start_table();
            fbb.push_slot::<i32>(4, count, 0);
            let off = fbb.end_table(t);
            finish(fbb, 2, off)
        }
        UnionVariant::S(x) => {
            // Push the struct out-of-line; `fbb.push` aligns to
            // `WireS::size()` and returns a `WIPOffset<WireS>`
            // that the value slot then encodes as a forward
            // uoffset_t.
            let s_off = fbb.push(WireS(x));
            finish(fbb, 3, s_off)
        }
        UnionVariant::Str(s) => {
            let str_off = fbb.create_string(s);
            finish(fbb, 4, str_off)
        }
    }
}

/// Test helper: execute against the union schema and return the
/// first leaf.
fn run_union(query_str: &str, buf: &[u8], bfbs: &[u8]) -> Result<Option<String>, ExecuteError> {
    let schema = root_as_schema(bfbs).expect("test schema verifies");
    let query = parse(query_str).expect("test query parses");
    execute_with_options(
        buf,
        &schema,
        &query,
        &Bounds::default(),
        &ExecuteOptions::default(),
    )
    .map(|v| v.into_iter().next().flatten())
}

/// Test helper: execute against the union schema and return the
/// raw error (used by the unsupported-step / not-found tests).
fn run_union_err(query_str: &str, buf: &[u8], bfbs: &[u8]) -> ExecuteError {
    let schema = root_as_schema(bfbs).expect("test schema verifies");
    let query = parse(query_str).expect("test query parses");
    execute_with_options(
        buf,
        &schema,
        &query,
        &Bounds::default(),
        &ExecuteOptions::default(),
    )
    .unwrap_err()
}

#[test]
fn union_descends_into_table_a_variant() {
    let bfbs = build_union_schema();
    let buf = build_msg_buf(UnionVariant::A("hello"));
    // Auto-dispatch through the discriminator: body resolves to
    // TableA, then `.name` reads the string scalar leaf.
    assert_eq!(
        run_union("Msg:body.name", &buf, &bfbs).unwrap(),
        Some("hello".to_string())
    );
}

#[test]
fn union_descends_into_table_b_variant() {
    let bfbs = build_union_schema();
    let buf = build_msg_buf(UnionVariant::B(42));
    assert_eq!(
        run_union("Msg:body.count", &buf, &bfbs).unwrap(),
        Some("42".to_string())
    );
}

#[test]
fn union_none_variant_yields_none() {
    let bfbs = build_union_schema();
    let buf = build_msg_buf(UnionVariant::None);
    // Discriminator 0 short-circuits to `vec![None]` regardless
    // of what's asked beneath the union; same shape as an
    // absent sub-table.
    assert_eq!(run_union("Msg:body.name", &buf, &bfbs).unwrap(), None);
    assert_eq!(run_union("Msg:body.count", &buf, &bfbs).unwrap(), None);
}

#[test]
fn union_field_not_in_active_variant_errors() {
    let bfbs = build_union_schema();
    // Active variant is A (which has `name` only); ask for B's
    // `count`. Auto-dispatch lands in TableA, where `count`
    // doesn't exist → FieldNotFound.
    let buf = build_msg_buf(UnionVariant::A("hi"));
    let err = run_union_err("Msg:body.count", &buf, &bfbs);
    assert!(
        matches!(&err, ExecuteError::FieldNotFound { what, table }
            if what == "count" && table == "A"),
        "got {err:?}"
    );
}

#[test]
fn union_at_leaf_errors() {
    let bfbs = build_union_schema();
    let buf = build_msg_buf(UnionVariant::A("x"));
    // `Msg:body` with no descent — for a table-typed active
    // variant the union value is not a textual leaf, so the
    // empty-tail leaf path (`read_union_value_leaf`) rejects with
    // a hint to descend via `.field`. (String variants
    // *do* have a leaf form — see
    // `union_string_variant_at_leaf_returns_value`.)
    let err = run_union_err("Msg:body", &buf, &bfbs);
    assert!(
        matches!(&err, ExecuteError::UnsupportedType { field, type_name }
            if field == "body" && type_name.contains("table/struct variant")),
        "got {err:?}"
    );
}

#[test]
fn union_discriminator_field_returns_value() {
    let bfbs = build_union_schema();
    // `body_type` is a UType (u8) scalar; queryable directly.
    // Returns the discriminator number — callers can map it to
    // a name in SQL until the deferred `|type` syntax lands.
    assert_eq!(
        run_union("Msg:body_type", &build_msg_buf(UnionVariant::A("x")), &bfbs).unwrap(),
        Some("1".to_string())
    );
    assert_eq!(
        run_union("Msg:body_type", &build_msg_buf(UnionVariant::B(0)), &bfbs).unwrap(),
        Some("2".to_string())
    );
    // NONE: discriminator slot omitted → schema default = 0.
    assert_eq!(
        run_union("Msg:body_type", &build_msg_buf(UnionVariant::None), &bfbs).unwrap(),
        Some("0".to_string())
    );
}

#[test]
fn union_with_index_step_errors() {
    let bfbs = build_union_schema();
    let buf = build_msg_buf(UnionVariant::A("x"));
    // `[0]` makes no sense on a union (it's not a vector).
    // Falls through walk_union → walk_table on TableA, which
    // rejects Step::Index at the `head` match.
    let err = run_union_err("Msg:body[0]", &buf, &bfbs);
    assert!(
        matches!(&err, ExecuteError::UnsupportedStep { what } if *what == "[index]"),
        "got {err:?}"
    );
}

#[test]
fn union_with_keys_step_errors() {
    let bfbs = build_union_schema();
    let buf = build_msg_buf(UnionVariant::A("x"));
    // `|keys` on a union: same dispatch path as `[0]`.
    let err = run_union_err("Msg:body|keys", &buf, &bfbs);
    assert!(
        matches!(&err, ExecuteError::UnsupportedStep { what } if *what == "|keys"),
        "got {err:?}"
    );
}

// -- `|type` (Step::UnionType) tests --

#[test]
fn union_type_leaf_returns_variant_name_a() {
    let bfbs = build_union_schema();
    let buf = build_msg_buf(UnionVariant::A("hello"));
    // `body|type` reads the discriminator and yields the
    // EnumVal name (the symbolic variant name) — string leaf.
    assert_eq!(
        run_union("Msg:body|type", &buf, &bfbs).unwrap(),
        Some("A".to_string())
    );
}

#[test]
fn union_type_leaf_returns_variant_name_b() {
    let bfbs = build_union_schema();
    let buf = build_msg_buf(UnionVariant::B(99));
    assert_eq!(
        run_union("Msg:body|type", &buf, &bfbs).unwrap(),
        Some("B".to_string())
    );
}

#[test]
fn union_type_leaf_for_none_returns_none_name() {
    let bfbs = build_union_schema();
    let buf = build_msg_buf(UnionVariant::None);
    // Discriminator absent → 0 → NONE EnumVal. Returns its
    // *name* (not SQL NULL) so the row stays filterable in SQL.
    // Symmetric with `body_type` returning "0" for absent.
    assert_eq!(
        run_union("Msg:body|type", &buf, &bfbs).unwrap(),
        Some("NONE".to_string())
    );
}

#[test]
fn union_type_on_non_union_field_errors() {
    let bfbs = build_union_schema();
    let buf = build_msg_buf(UnionVariant::A("x"));
    // `body_type` is a UType scalar, not a union. `|type` is
    // only meaningful on `BaseType::Union` fields.
    let err = run_union_err("Msg:body_type|type", &buf, &bfbs);
    assert!(
        matches!(&err, ExecuteError::UnsupportedStep { what }
            if what.starts_with("|type") && what.contains("only valid on union")),
        "got {err:?}"
    );
}

#[test]
fn union_type_with_descent_after_errors() {
    let bfbs = build_union_schema();
    let buf = build_msg_buf(UnionVariant::A("x"));
    // `|type` is a terminal leaf — descending past it is a
    // type-shape error. Parser allows the syntactic shape;
    // executor rejects it (mirrors the `|keys.foo` policy).
    let err = run_union_err("Msg:body|type.x", &buf, &bfbs);
    assert!(
        matches!(&err, ExecuteError::UnsupportedStep { what }
            if what.starts_with("|type") && what.contains("terminal")),
        "got {err:?}"
    );
}

#[test]
fn union_type_at_root_errors() {
    // `Msg:|type` would mean "type of the root" — but the root
    // isn't a union. The parser produces a Step::UnionType as
    // the first step; walk_table's head match rejects it.
    let bfbs = build_union_schema();
    let buf = build_msg_buf(UnionVariant::A("x"));
    // The parser rejects an empty identifier before `|`; we
    // simulate the executor-side path by constructing the AST
    // directly. (`Msg:|type` parses as `EmptyComponent` /
    // `ExpectedIdentifier`, never reaching the executor.)
    let schema = root_as_schema(&bfbs).expect("test schema verifies");
    let query = Query {
        schema: None,
        root: "Msg".to_string(),
        steps: vec![Step::UnionType],
    };
    let err = execute_with_options(
        &buf,
        &schema,
        &query,
        &Bounds::default(),
        &ExecuteOptions::default(),
    )
    .unwrap_err();
    assert!(
        matches!(&err, ExecuteError::UnsupportedStep { what } if *what == "|type"),
        "got {err:?}"
    );
}

// -- Non-table union variants: string + struct (design §4.3) --

#[test]
fn union_string_variant_at_leaf_returns_value() {
    let bfbs = build_union_schema();
    let buf = build_msg_buf(UnionVariant::Str("hello world"));
    // Empty-tail path: `Msg:body` for a string-typed variant
    // returns the UTF-8 content as the leaf value (no descent
    // is possible — strings carry no field children). Routed
    // through `read_union_value_leaf`.
    assert_eq!(
        run_union("Msg:body", &buf, &bfbs).unwrap(),
        Some("hello world".to_string())
    );
}

#[test]
fn union_string_variant_with_descent_after_errors() {
    let bfbs = build_union_schema();
    let buf = build_msg_buf(UnionVariant::Str("x"));
    // `.x` after a string-typed variant is a type-shape error
    // — strings have no sub-fields. `walk_union`'s String arm
    // emits the rejection.
    let err = run_union_err("Msg:body.x", &buf, &bfbs);
    assert!(
        matches!(&err, ExecuteError::UnsupportedType { field, type_name }
            if field == "body" && type_name.contains("string union variant")),
        "got {err:?}"
    );
}

#[test]
fn union_string_variant_type_leaf_returns_variant_name() {
    let bfbs = build_union_schema();
    let buf = build_msg_buf(UnionVariant::Str("ignored"));
    // `|type` on a string-typed variant still returns the
    // EnumVal name from the reflected enum (in this schema:
    // `"string"`, matching how `flatc` names string variants).
    assert_eq!(
        run_union("Msg:body|type", &buf, &bfbs).unwrap(),
        Some("string".to_string())
    );
}

#[test]
fn union_string_discriminator_field_returns_value() {
    let bfbs = build_union_schema();
    // `body_type` is a UType scalar; for the string variant
    // (discriminator = 4) it returns "4".
    assert_eq!(
        run_union(
            "Msg:body_type",
            &build_msg_buf(UnionVariant::Str("anything")),
            &bfbs
        )
        .unwrap(),
        Some("4".to_string())
    );
}

#[test]
fn union_struct_variant_descends_to_inline_field() {
    let bfbs = build_union_schema();
    let buf = build_msg_buf(UnionVariant::S(123));
    // `Msg:body.x` resolves the union to struct `S`, then
    // descends into the inline `x:int` field via `walk_struct`.
    assert_eq!(
        run_union("Msg:body.x", &buf, &bfbs).unwrap(),
        Some("123".to_string())
    );
}

#[test]
fn union_struct_variant_at_leaf_errors() {
    let bfbs = build_union_schema();
    let buf = build_msg_buf(UnionVariant::S(0));
    // Empty-tail `Msg:body` for a struct-typed variant: like
    // the table variant, structs have no v0.1 textual leaf
    // form. Rejected with the table/struct hint by
    // `read_union_value_leaf`.
    let err = run_union_err("Msg:body", &buf, &bfbs);
    assert!(
        matches!(&err, ExecuteError::UnsupportedType { field, type_name }
            if field == "body" && type_name.contains("table/struct variant")),
        "got {err:?}"
    );
}

#[test]
fn union_struct_variant_type_leaf_returns_variant_name() {
    let bfbs = build_union_schema();
    let buf = build_msg_buf(UnionVariant::S(7));
    assert_eq!(
        run_union("Msg:body|type", &buf, &bfbs).unwrap(),
        Some("S".to_string())
    );
}

#[test]
fn union_struct_variant_unknown_field_errors() {
    let bfbs = build_union_schema();
    let buf = build_msg_buf(UnionVariant::S(0));
    // Auto-dispatch lands in struct `S` which has only field
    // `x`; asking for `count` (a table-B field) bottoms out in
    // `walk_struct`'s find-field path.
    let err = run_union_err("Msg:body.count", &buf, &bfbs);
    assert!(
        matches!(&err, ExecuteError::FieldNotFound { what, table }
            if what == "count" && table == "S"),
        "got {err:?}"
    );
}
