use super::*;

// ---------------------------------------------------------------
// Vector fixtures + tests
// ---------------------------------------------------------------

/// Build a vector-bearing schema, kept separate from
/// `build_schema()` so the two fixtures evolve independently:
///
/// ```text
/// table Item {
///   sku: string;          // id 0, vtable offset 4
/// }
/// table Bag {
///   flags: [bool];        // id 0, vtable offset 4
///   items: [Item];        // id 1, vtable offset 6
///   nums:  [int];         // id 2, vtable offset 8
///   tags:  [string];      // id 3, vtable offset 10
/// }
/// root_type Bag;
/// ```
///
/// Field vectors are sorted alphabetically (flags, items, nums,
/// tags). Object vector is sorted (Bag < Item).
fn build_vec_schema() -> Vec<u8> {
    let mut fbb = FlatBufferBuilder::new();

    let str_t = Type::create(
        &mut fbb,
        &TypeArgs {
            base_type: BaseType::String,
            ..Default::default()
        },
    );

    // -- Item.sku: string (single-field table) --
    // Marked `(key)` so the `Step::MapKey` tests can do
    // `Bag:items[abc].sku` lookups against this fixture. The
    // existing `[i]` and `[*]` tests don't observe the `key`
    // flag, so this annotation is non-breaking for them.
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
    let item_fields = fbb.create_vector(&[sku_f]);
    let item_n = fbb.create_string("Item");
    let item = RObject::create(
        &mut fbb,
        &ObjectArgs {
            name: Some(item_n),
            fields: Some(item_fields),
            ..Default::default()
        },
    );

    // -- Vector element types --
    // Object index 1 = Item (Bag is 0, Item is 1 once sorted).
    let vec_bool_t = Type::create(
        &mut fbb,
        &TypeArgs {
            base_type: BaseType::Vector,
            element: BaseType::Bool,
            ..Default::default()
        },
    );
    let vec_item_t = Type::create(
        &mut fbb,
        &TypeArgs {
            base_type: BaseType::Vector,
            element: BaseType::Obj,
            index: 1,
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
    let vec_str_t = Type::create(
        &mut fbb,
        &TypeArgs {
            base_type: BaseType::Vector,
            element: BaseType::String,
            ..Default::default()
        },
    );

    // -- Bag fields (sorted: flags, items, nums, tags) --
    let flags_n = fbb.create_string("flags");
    let flags_f = RField::create(
        &mut fbb,
        &FieldArgs {
            name: Some(flags_n),
            type_: Some(vec_bool_t),
            id: 0,
            offset: 4,
            ..Default::default()
        },
    );
    let items_n = fbb.create_string("items");
    let items_f = RField::create(
        &mut fbb,
        &FieldArgs {
            name: Some(items_n),
            type_: Some(vec_item_t),
            id: 1,
            offset: 6,
            ..Default::default()
        },
    );
    let nums_n = fbb.create_string("nums");
    let nums_f = RField::create(
        &mut fbb,
        &FieldArgs {
            name: Some(nums_n),
            type_: Some(vec_int_t),
            id: 2,
            offset: 8,
            ..Default::default()
        },
    );
    let tags_n = fbb.create_string("tags");
    let tags_f = RField::create(
        &mut fbb,
        &FieldArgs {
            name: Some(tags_n),
            type_: Some(vec_str_t),
            id: 3,
            offset: 10,
            ..Default::default()
        },
    );
    let bag_fields = fbb.create_vector(&[flags_f, items_f, nums_f, tags_f]);
    let bag_n = fbb.create_string("Bag");
    let bag = RObject::create(
        &mut fbb,
        &ObjectArgs {
            name: Some(bag_n),
            fields: Some(bag_fields),
            ..Default::default()
        },
    );

    // Objects sorted by name: Bag (0), Item (1).
    let objects = fbb.create_vector(&[bag, item]);
    let enums = fbb.create_vector::<flatbuffers::ForwardsUOffset<Enum>>(&[]);
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

/// Build a `Bag` buffer. Each argument may be `None` to elide the
/// vector slot entirely (covers the absent-vector path); an
/// empty slice produces a present zero-length vector.
fn build_bag(
    items: Option<&[&str]>,
    tags: Option<&[&str]>,
    nums: Option<&[i32]>,
    flags: Option<&[bool]>,
) -> Vec<u8> {
    let mut fbb = FlatBufferBuilder::new();

    // Build vectors first so their offsets are known before we
    // open the Bag table.
    let items_off = items.map(|skus| {
        // Each Item is its own table; build them, collect
        // offsets, then create_vector over them.
        let item_offs: Vec<_> = skus
            .iter()
            .map(|sku| {
                let sku_off = fbb.create_string(sku);
                let t = fbb.start_table();
                fbb.push_slot_always(4, sku_off);
                fbb.end_table(t)
            })
            .collect();
        fbb.create_vector(&item_offs)
    });
    let tags_off = tags.map(|ts| {
        let tag_offs: Vec<_> = ts.iter().map(|t| fbb.create_string(t)).collect();
        fbb.create_vector(&tag_offs)
    });
    let nums_off = nums.map(|ns| fbb.create_vector(ns));
    let flags_off = flags.map(|fs| fbb.create_vector(fs));

    let t = fbb.start_table();
    if let Some(off) = flags_off {
        fbb.push_slot_always(4, off);
    }
    if let Some(off) = items_off {
        fbb.push_slot_always(6, off);
    }
    if let Some(off) = nums_off {
        fbb.push_slot_always(8, off);
    }
    if let Some(off) = tags_off {
        fbb.push_slot_always(10, off);
    }
    let bag = fbb.end_table(t);
    fbb.finish_minimal(bag);
    fbb.finished_data().to_vec()
}

/// the first leaf. The fanout (`Step::All`) tests use
/// [`run_vec_all`] instead to inspect the whole vec.
fn run_vec(query_str: &str, buf: &[u8], bfbs: &[u8]) -> Result<Option<String>, ExecuteError> {
    run_vec_all(query_str, buf, bfbs).map(|v| v.into_iter().next().flatten())
}

/// Like [`run_vec`] but takes a custom [`ExecuteOptions`] so
/// tests can pin the `key_lookup_strict` mode (binary search
/// vs. linear scan) explicitly.
fn run_vec_with(
    query_str: &str,
    buf: &[u8],
    bfbs: &[u8],
    options: &ExecuteOptions,
) -> Result<Option<String>, ExecuteError> {
    let schema = root_as_schema(bfbs).expect("test schema verifies");
    let query = parse(query_str).expect("test query parses");
    execute_with_options(buf, &schema, &query, &Bounds::default(), options)
        .map(|v| v.into_iter().next().flatten())
}

fn run_vec_all(
    query_str: &str,
    buf: &[u8],
    bfbs: &[u8],
) -> Result<Vec<Option<String>>, ExecuteError> {
    let schema = root_as_schema(bfbs).expect("test schema verifies");
    let query = parse(query_str).expect("test query parses");
    execute_with_options(
        buf,
        &schema,
        &query,
        &Bounds::default(),
        &ExecuteOptions::default(),
    )
}

// -- vector of strings --

#[test]
fn vector_string_index_in_range() {
    let bfbs = build_vec_schema();
    let buf = build_bag(None, Some(&["red", "green", "blue"]), None, None);
    let v = run_vec("Bag:tags[0]", &buf, &bfbs).unwrap();
    assert_eq!(v.as_deref(), Some("red"));
    let v = run_vec("Bag:tags[2]", &buf, &bfbs).unwrap();
    assert_eq!(v.as_deref(), Some("blue"));
}

#[test]
fn vector_string_index_out_of_bounds_returns_none() {
    let bfbs = build_vec_schema();
    let buf = build_bag(None, Some(&["red"]), None, None);
    let v = run_vec("Bag:tags[99]", &buf, &bfbs).unwrap();
    // Per design §4.3, OOB indices short-circuit to NULL —
    // explicitly distinct from `ERROR`.
    assert!(v.is_none());
}

#[test]
fn vector_string_empty_index_returns_none() {
    let bfbs = build_vec_schema();
    // Present-but-empty vector: vtable slot points to a Vector
    // with length 0. Index 0 is OOB.
    let buf = build_bag(None, Some(&[]), None, None);
    let v = run_vec("Bag:tags[0]", &buf, &bfbs).unwrap();
    assert!(v.is_none());
}

#[test]
fn vector_absent_vtable_slot_returns_none() {
    let bfbs = build_vec_schema();
    let buf = build_bag(None, None, None, None); // no slots set
    let v = run_vec("Bag:tags[0]", &buf, &bfbs).unwrap();
    assert!(v.is_none());
}

// -- vector of scalars --

#[test]
fn vector_int_index_in_range() {
    let bfbs = build_vec_schema();
    let buf = build_bag(None, None, Some(&[10, 20, 30]), None);
    let v = run_vec("Bag:nums[1]", &buf, &bfbs).unwrap();
    assert_eq!(v.as_deref(), Some("20"));
}

#[test]
fn vector_bool_renders_as_zero_or_one() {
    let bfbs = build_vec_schema();
    let buf = build_bag(None, None, None, Some(&[true, false]));
    // Bool elements stringify via `u8::Display` → "0" / "1",
    // matching the way scalar bool *fields* render through the
    // upstream `get_any_field_string` path.
    assert_eq!(
        run_vec("Bag:flags[0]", &buf, &bfbs).unwrap().as_deref(),
        Some("1")
    );
    assert_eq!(
        run_vec("Bag:flags[1]", &buf, &bfbs).unwrap().as_deref(),
        Some("0")
    );
}

// -- vector of tables --

#[test]
fn vector_of_tables_descend_then_string_leaf() {
    let bfbs = build_vec_schema();
    let buf = build_bag(Some(&["ABC", "DEF", "GHI"]), None, None, None);
    let v = run_vec("Bag:items[1].sku", &buf, &bfbs).unwrap();
    assert_eq!(v.as_deref(), Some("DEF"));
}

#[test]
fn vector_of_tables_oob_then_field_returns_none() {
    let bfbs = build_vec_schema();
    let buf = build_bag(Some(&["ABC"]), None, None, None);
    let v = run_vec("Bag:items[5].sku", &buf, &bfbs).unwrap();
    assert!(v.is_none());
}

// -- error paths --

#[test]
fn bare_vector_at_leaf_errors_with_unsupported_type() {
    let bfbs = build_vec_schema();
    let buf = build_bag(None, Some(&["x"]), None, None);
    // `Bag:tags` (no `[i]`) has no v0.1 textual form.
    let err = run_vec("Bag:tags", &buf, &bfbs).unwrap_err();
    assert!(
        matches!(&err, ExecuteError::UnsupportedType { field, .. } if field == "tags"),
        "got {err:?}"
    );
}

#[test]
fn vector_of_tables_element_at_leaf_errors_with_unsupported_type() {
    let bfbs = build_vec_schema();
    let buf = build_bag(Some(&["ABC"]), None, None, None);
    // `Bag:items[0]` would yield a sub-table value; can't
    // stringify.
    let err = run_vec("Bag:items[0]", &buf, &bfbs).unwrap_err();
    assert!(
        matches!(&err, ExecuteError::UnsupportedType { field, .. } if field == "items"),
        "got {err:?}"
    );
}

#[test]
fn descend_through_scalar_vector_errors_with_unsupported_type() {
    let bfbs = build_vec_schema();
    let buf = build_bag(None, None, Some(&[1, 2, 3]), None);
    // `nums[0].foo` — can't descend into a scalar element.
    let err = run_vec("Bag:nums[0].foo", &buf, &bfbs).unwrap_err();
    assert!(
        matches!(&err, ExecuteError::UnsupportedType { field, .. } if field == "nums"),
        "got {err:?}"
    );
}

// -- Step::All fanout --

#[test]
fn vector_all_strings() {
    let bfbs = build_vec_schema();
    let buf = build_bag(None, Some(&["red", "green", "blue"]), None, None);
    let v = run_vec_all("Bag:tags[*]", &buf, &bfbs).expect("ok");
    assert_eq!(
        v,
        vec![
            Some("red".to_owned()),
            Some("green".to_owned()),
            Some("blue".to_owned()),
        ],
    );
}

#[test]
fn vector_all_strings_empty_vector() {
    let bfbs = build_vec_schema();
    // tags is present but empty.
    let buf = build_bag(None, Some(&[]), None, None);
    let v = run_vec_all("Bag:tags[*]", &buf, &bfbs).expect("ok");
    assert!(v.is_empty(), "got {v:?}");
}

#[test]
fn vector_all_strings_absent_vector() {
    let bfbs = build_vec_schema();
    // tags is absent (vtable slot 0).
    let buf = build_bag(None, None, None, None);
    let v = run_vec_all("Bag:tags[*]", &buf, &bfbs).expect("ok");
    assert!(v.is_empty(), "got {v:?}");
}

#[test]
fn vector_all_scalar_ints() {
    let bfbs = build_vec_schema();
    let buf = build_bag(None, None, Some(&[10, 20, 30]), None);
    let v = run_vec_all("Bag:nums[*]", &buf, &bfbs).expect("ok");
    assert_eq!(
        v,
        vec![
            Some("10".to_owned()),
            Some("20".to_owned()),
            Some("30".to_owned()),
        ],
    );
}

#[test]
fn vector_all_scalar_bools() {
    let bfbs = build_vec_schema();
    // bool elements stringify as "1" / "0" to match
    // `read_vector_element` (and the upstream
    // `get_any_field_string` form for scalar bool fields).
    let buf = build_bag(None, None, None, Some(&[true, false, true]));
    let v = run_vec_all("Bag:flags[*]", &buf, &bfbs).expect("ok");
    assert_eq!(
        v,
        vec![
            Some("1".to_owned()),
            Some("0".to_owned()),
            Some("1".to_owned()),
        ],
    );
}

#[test]
fn vector_all_table_field_descent() {
    let bfbs = build_vec_schema();
    // Three items with skus "a", "b", "c".
    let buf = build_bag(Some(&["a", "b", "c"]), None, None, None);
    let v = run_vec_all("Bag:items[*].sku", &buf, &bfbs).expect("ok");
    assert_eq!(
        v,
        vec![
            Some("a".to_owned()),
            Some("b".to_owned()),
            Some("c".to_owned()),
        ],
    );
}

#[test]
fn vector_all_table_field_absent_intermediate() {
    let bfbs = build_vec_schema();
    // build_bag's items always set sku, so use empty-string sku
    // to exercise the "present but empty" path; absent-sku
    // requires a custom builder. The fanout still emits one
    // entry per element regardless.
    let buf = build_bag(Some(&["x", "", "y"]), None, None, None);
    let v = run_vec_all("Bag:items[*].sku", &buf, &bfbs).expect("ok");
    assert_eq!(
        v,
        vec![
            Some("x".to_owned()),
            Some("".to_owned()),
            Some("y".to_owned()),
        ],
    );
}

#[test]
fn vector_all_table_with_no_descent_errors() {
    let bfbs = build_vec_schema();
    let buf = build_bag(Some(&["a"]), None, None, None);
    // `Bag:items[*]` lands at a sub-table value with no textual
    // form — same rationale as `Bag:items[0]`.
    let err = run_vec_all("Bag:items[*]", &buf, &bfbs).unwrap_err();
    assert!(
        matches!(&err, ExecuteError::UnsupportedType { field, .. } if field == "items"),
        "got {err:?}"
    );
}

#[test]
fn vector_all_scalar_with_descent_errors() {
    let bfbs = build_vec_schema();
    let buf = build_bag(None, None, Some(&[1, 2, 3]), None);
    // `Bag:nums[*].foo` — can't descend into a scalar element.
    let err = run_vec_all("Bag:nums[*].foo", &buf, &bfbs).unwrap_err();
    assert!(
        matches!(&err, ExecuteError::UnsupportedType { field, .. } if field == "nums"),
        "got {err:?}"
    );
}

// -- Step::MapKey --

#[test]
fn vector_map_key_hit() {
    let bfbs = build_vec_schema();
    let buf = build_bag(Some(&["a", "b", "c"]), None, None, None);
    // Linear scan finds the element whose `(key)`-annotated
    // `sku` field equals "b", then descends with `.sku` to
    // stringify it.
    let v = run_vec("Bag:items[b].sku", &buf, &bfbs).expect("ok");
    assert_eq!(v.as_deref(), Some("b"));
}

#[test]
fn vector_map_key_first_hit_in_wire_order() {
    let bfbs = build_vec_schema();
    // Two entries with sku = "dup": the linear-scan path
    // (`key_lookup_strict = off`) returns the first in wire
    // order. Binary search under the default
    // `key_lookup_strict = on` mode can return *either* match
    // depending on the bisect path, so we pin the wire-order
    // tiebreaker against the off-path explicitly.
    let buf = build_bag(Some(&["dup", "x", "dup"]), None, None, None);
    let v = run_vec_with(
        "Bag:items[dup].sku",
        &buf,
        &bfbs,
        &ExecuteOptions {
            key_lookup_strict: false,
            ..ExecuteOptions::default()
        },
    )
    .expect("ok");
    assert_eq!(v.as_deref(), Some("dup"));
}

#[test]
fn vector_map_key_miss_returns_none() {
    let bfbs = build_vec_schema();
    let buf = build_bag(Some(&["a", "b"]), None, None, None);
    let v = run_vec("Bag:items[zzz].sku", &buf, &bfbs).expect("ok");
    assert!(v.is_none(), "got {v:?}");
}

#[test]
fn vector_map_key_empty_vector_returns_none() {
    let bfbs = build_vec_schema();
    let buf = build_bag(Some(&[]), None, None, None);
    let v = run_vec("Bag:items[abc].sku", &buf, &bfbs).expect("ok");
    assert!(v.is_none(), "got {v:?}");
}

#[test]
fn vector_map_key_absent_vector_returns_none() {
    let bfbs = build_vec_schema();
    // items field elided entirely.
    let buf = build_bag(None, None, None, None);
    let v = run_vec("Bag:items[abc].sku", &buf, &bfbs).expect("ok");
    assert!(v.is_none(), "got {v:?}");
}

// -- Step::MapKey: binary search vs. linear scan --

/// Binary search (the `key_lookup_strict = on` default) finds an
/// interior key on a properly sorted vector. With 5 elements
/// the bisect visits ~3 of them, vs. ~3 on average for linear
/// scan; the perf-win story shows up at larger n, but this
/// fixture is enough to pin correctness.
#[test]
fn vector_map_key_binary_search_interior_hit() {
    let bfbs = build_vec_schema();
    let buf = build_bag(Some(&["a", "b", "c", "d", "e"]), None, None, None);
    let v = run_vec("Bag:items[c].sku", &buf, &bfbs).expect("ok");
    assert_eq!(v.as_deref(), Some("c"));
}

/// Binary search must hit both endpoints (lo-mid and hi-mid
/// boundaries). The interior-hit test alone doesn't exercise
/// either edge.
#[test]
fn vector_map_key_binary_search_first_and_last_hit() {
    let bfbs = build_vec_schema();
    let buf = build_bag(Some(&["a", "b", "c", "d", "e"]), None, None, None);
    let first = run_vec("Bag:items[a].sku", &buf, &bfbs).expect("ok");
    assert_eq!(first.as_deref(), Some("a"));
    let last = run_vec("Bag:items[e].sku", &buf, &bfbs).expect("ok");
    assert_eq!(last.as_deref(), Some("e"));
}

/// Binary search reports miss on a key sandwiched between two
/// present keys (the classic "split goes both ways" case where
/// a naive bisect would loop forever if it didn't tighten the
/// half-open range correctly).
#[test]
fn vector_map_key_binary_search_interior_miss() {
    let bfbs = build_vec_schema();
    // "bb" sorts between "b" and "c"; not present.
    let buf = build_bag(Some(&["a", "b", "c", "d"]), None, None, None);
    let v = run_vec("Bag:items[bb].sku", &buf, &bfbs).expect("ok");
    assert!(v.is_none(), "got {v:?}");
}

/// Binary search misses below the first element (lo advances
/// past hi at lo=0,hi=0 immediately).
#[test]
fn vector_map_key_binary_search_below_first_miss() {
    let bfbs = build_vec_schema();
    let buf = build_bag(Some(&["m", "n", "o"]), None, None, None);
    let v = run_vec("Bag:items[a].sku", &buf, &bfbs).expect("ok");
    assert!(v.is_none(), "got {v:?}");
}

/// Binary search misses above the last element.
#[test]
fn vector_map_key_binary_search_above_last_miss() {
    let bfbs = build_vec_schema();
    let buf = build_bag(Some(&["a", "b", "c"]), None, None, None);
    let v = run_vec("Bag:items[z].sku", &buf, &bfbs).expect("ok");
    assert!(v.is_none(), "got {v:?}");
}

/// `key_lookup_strict = on` against an unsorted vector silently
/// misses keys the bisect can't reach. This is the documented
/// worst case from the GUC's doc text — fail-fast for the
/// operator is escalate to `key_lookup_strict = off`. Pin it so
/// a regression that accidentally fell back to linear scan
/// under strict-on would surface.
#[test]
fn vector_map_key_binary_search_unsorted_silent_miss() {
    let bfbs = build_vec_schema();
    // "a" sorts before "b" / "c", but the vector is in reverse
    // order. Bisect on len=3 visits mid=1 first; sees "b" vs.
    // target "a" → Less → moves to hi=1; then mid=0, sees "c"
    // vs. "a" → Greater → hi=0; loop ends; no match.
    let buf = build_bag(Some(&["c", "b", "a"]), None, None, None);
    let v = run_vec("Bag:items[a].sku", &buf, &bfbs).expect("ok");
    assert!(
        v.is_none(),
        "binary search on unsorted vec must silently miss; got {v:?}",
    );
}

/// Same fixture as the strict-on silent-miss test, but with
/// `key_lookup_strict = off`: linear scan finds the element
/// regardless of order. Pins the escape-hatch contract.
#[test]
fn vector_map_key_linear_scan_finds_unsorted_match() {
    let bfbs = build_vec_schema();
    let buf = build_bag(Some(&["c", "b", "a"]), None, None, None);
    let v = run_vec_with(
        "Bag:items[a].sku",
        &buf,
        &bfbs,
        &ExecuteOptions {
            key_lookup_strict: false,
            ..ExecuteOptions::default()
        },
    )
    .expect("ok");
    assert_eq!(v.as_deref(), Some("a"));
}

/// Both modes agree on miss when the key genuinely isn't
/// present, regardless of sortedness. Belt-and-suspenders for
/// the "off mode doesn't silently invent matches" property.
#[test]
fn vector_map_key_both_modes_agree_on_absent_key() {
    let bfbs = build_vec_schema();
    let buf = build_bag(Some(&["a", "b", "c"]), None, None, None);
    let strict = run_vec("Bag:items[zzz].sku", &buf, &bfbs).expect("ok");
    let loose = run_vec_with(
        "Bag:items[zzz].sku",
        &buf,
        &bfbs,
        &ExecuteOptions {
            key_lookup_strict: false,
            ..ExecuteOptions::default()
        },
    )
    .expect("ok");
    assert!(
        strict.is_none() && loose.is_none(),
        "{strict:?} vs {loose:?}"
    );
}

#[test]
fn vector_map_key_against_scalar_vector_errors() {
    let bfbs = build_vec_schema();
    // `tags` is a vector of strings, not a vector of tables;
    // map-key lookup isn't defined for it.
    let buf = build_bag(None, Some(&["a", "b"]), None, None);
    let err = run_vec("Bag:tags[a]", &buf, &bfbs).unwrap_err();
    assert!(
        matches!(&err, ExecuteError::UnsupportedType { field, .. } if field == "tags"),
        "got {err:?}"
    );
}

#[test]
fn vector_map_key_no_descent_errors() {
    let bfbs = build_vec_schema();
    // `Bag:items[abc]` lands at a sub-table value with no v0.1
    // textual form — same rationale as `Bag:items[0]`.
    let buf = build_bag(Some(&["abc"]), None, None, None);
    let err = run_vec("Bag:items[abc]", &buf, &bfbs).unwrap_err();
    assert!(
        matches!(&err, ExecuteError::UnsupportedType { field, .. } if field == "items"),
        "got {err:?}"
    );
}

// -- Step::MapKeys --

#[test]
fn vector_map_keys_fans_out_keys_in_wire_order() {
    let bfbs = build_vec_schema();
    let buf = build_bag(Some(&["a", "b", "c"]), None, None, None);
    let v = run_vec_all("Bag:items|keys", &buf, &bfbs).expect("ok");
    assert_eq!(
        v,
        vec![
            Some("a".to_owned()),
            Some("b".to_owned()),
            Some("c".to_owned()),
        ],
    );
}

#[test]
fn vector_map_keys_preserves_duplicates() {
    let bfbs = build_vec_schema();
    // Linear / wire-order fanout: duplicates are NOT collapsed
    // (the §10 `key_lookup_strict = off` fallback semantics that
    // this slice ships unconditionally).
    let buf = build_bag(Some(&["dup", "x", "dup"]), None, None, None);
    let v = run_vec_all("Bag:items|keys", &buf, &bfbs).expect("ok");
    assert_eq!(
        v,
        vec![
            Some("dup".to_owned()),
            Some("x".to_owned()),
            Some("dup".to_owned()),
        ],
    );
}

#[test]
fn vector_map_keys_empty_vector_returns_empty_vec() {
    let bfbs = build_vec_schema();
    let buf = build_bag(Some(&[]), None, None, None);
    let v = run_vec_all("Bag:items|keys", &buf, &bfbs).expect("ok");
    assert!(v.is_empty(), "got {v:?}");
}

#[test]
fn vector_map_keys_absent_vector_returns_empty_vec() {
    let bfbs = build_vec_schema();
    let buf = build_bag(None, None, None, None);
    let v = run_vec_all("Bag:items|keys", &buf, &bfbs).expect("ok");
    assert!(v.is_empty(), "got {v:?}");
}

#[test]
fn vector_map_keys_against_scalar_vector_errors() {
    let bfbs = build_vec_schema();
    // `tags` is a vector of strings; `|keys` only works over
    // vectors of `(key)`-annotated tables.
    let buf = build_bag(None, Some(&["a", "b"]), None, None);
    let err = run_vec_all("Bag:tags|keys", &buf, &bfbs).unwrap_err();
    assert!(
        matches!(&err, ExecuteError::UnsupportedType { field, .. } if field == "tags"),
        "got {err:?}"
    );
}

#[test]
fn vector_map_keys_with_trailing_step_errors() {
    let bfbs = build_vec_schema();
    // Parser allows `items|keys.foo`; executor rejects because
    // the keys are themselves the leaves.
    let buf = build_bag(Some(&["a"]), None, None, None);
    let err = run_vec_all("Bag:items|keys.foo", &buf, &bfbs).unwrap_err();
    assert!(
        matches!(&err, ExecuteError::UnsupportedType { field, .. } if field == "items"),
        "got {err:?}"
    );
}
