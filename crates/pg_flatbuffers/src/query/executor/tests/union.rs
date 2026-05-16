use super::*;

// -----------------------------------------------------------------
// Union dispatch fixtures + tests (design §4.3, §7.2).
// -----------------------------------------------------------------

/// Build the reflected schema:
///
/// ```fbs
/// table A   { name:string;   }   // object index 0
/// table B   { count:int;     }   // object index 1
/// union U   { A, B }             // enum   index 0
/// table Msg { body:U;        }   // object index 2
///                                 //   body_type:UType @ slot 4
///                                 //   body:Union     @ slot 6
/// root_type Msg;
/// ```
///
/// Object vector is sorted alphabetically: `A` (0), `B` (1),
/// `Msg` (2). Enum vector has a single entry: `U` (0). Within
/// `U`, `EnumVal`s are sorted by `value`: `NONE` (0), `A` (1),
/// `B` (2) — that ordering is what lets `walk_union` use
/// [`flatbuffers::Vector::lookup_by_key`] for the discriminator.
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

    // EnumVals stored sorted by `value` (already 0/1/2).
    let u_values = fbb.create_vector(&[none_ev, a_ev, b_ev]);
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

    // Object vector sorted: A, B, Msg.
    let objects = fbb.create_vector(&[a, b, msg]);
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
}

/// Build a `Msg` buffer for the schema produced by
/// [`build_union_schema`]. Slot 4 holds the `u8` discriminator
/// and slot 6 holds the union value (a sub-table offset).
fn build_msg_buf(variant: UnionVariant<'_>) -> Vec<u8> {
    let mut fbb = FlatBufferBuilder::new();

    // Build the value sub-table first so its offset is known.
    // `flatbuffers::WIPOffset` is generic over the table type;
    // erase to a `usize`-shaped offset before pushing into the
    // union slot to keep both arms type-compatible.
    let (disc, value_off) = match variant {
        UnionVariant::None => (0u8, None),
        UnionVariant::A(name) => {
            let name_off = fbb.create_string(name);
            let t = fbb.start_table();
            fbb.push_slot_always(4, name_off);
            let off = fbb.end_table(t);
            (1, Some(off))
        }
        UnionVariant::B(count) => {
            let t = fbb.start_table();
            fbb.push_slot::<i32>(4, count, 0);
            let off = fbb.end_table(t);
            (2, Some(off))
        }
    };

    let t = fbb.start_table();
    if disc != 0 {
        // Discriminator at slot 4 (default 0 = NONE).
        fbb.push_slot::<u8>(4, disc, 0);
    }
    if let Some(off) = value_off {
        // Union value pointer at slot 6.
        fbb.push_slot_always(6, off);
    }
    let msg = fbb.end_table(t);
    fbb.finish_minimal(msg);
    fbb.finished_data().to_vec()
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
    // `Msg:body` with no descent — the union value is not a
    // textual leaf. Hits the existing `read_leaf` rejection
    // with `type_name: "union"`. Adding the variant-specific
    // "descend with `.field`" hint would require duplicating
    // the dispatch here; not worth a slice on its own.
    let err = run_union_err("Msg:body", &buf, &bfbs);
    assert!(
        matches!(&err, ExecuteError::UnsupportedType { field, type_name }
            if field == "body" && *type_name == "union"),
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
