use super::*;

// -- comparator unit tests (pure; no fixture) --

#[test]
fn compare_text_keys_lexicographically() {
    let key = CompiledKey::Text("banana");
    assert_eq!(compare_actual_to_compiled("apple", &key), Ordering::Less);
    assert_eq!(compare_actual_to_compiled("banana", &key), Ordering::Equal);
    assert_eq!(
        compare_actual_to_compiled("cherry", &key),
        Ordering::Greater
    );
}

#[test]
fn compare_int_keys_numerically() {
    // Lexicographic comparison would put "10" < "2"; numeric
    // comparison puts 2 < 10. Pin the numeric semantics.
    let key = CompiledKey::Int(10);
    assert_eq!(compare_actual_to_compiled("2", &key), Ordering::Less);
    assert_eq!(compare_actual_to_compiled("10", &key), Ordering::Equal);
    assert_eq!(compare_actual_to_compiled("11", &key), Ordering::Greater);
}

#[test]
fn compare_int_keys_signed() {
    // Negative actuals (signed int fields) parse and compare
    // correctly without underflowing through "-" lexicography.
    let key = CompiledKey::Int(0);
    assert_eq!(compare_actual_to_compiled("-5", &key), Ordering::Less);
    assert_eq!(compare_actual_to_compiled("5", &key), Ordering::Greater);
}

#[test]
fn compare_int_keys_unparseable_actual_is_less() {
    // Documented in `compare_actual_to_compiled`: a
    // non-i64-parseable actual (e.g., a ULong above i64::MAX)
    // sorts as `Less` so the bisect remains deterministic.
    let key = CompiledKey::Int(42);
    assert_eq!(
        compare_actual_to_compiled("99999999999999999999", &key),
        Ordering::Less,
    );
}
