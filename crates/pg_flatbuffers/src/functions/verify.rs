//! `flatbuffers_verify` — boolean entry point suitable for `CHECK`
//! constraints.

use super::util::split_schema_and_table;
use crate::guc::current_bounds;
use crate::schema_cache::lookup_schema;
use crate::verify::verify;
use pgrx::prelude::*;

/// Verify that `buf` parses as `table_name` under the current bounds.
///
/// `table_name` accepts the same `[<schema>:]<table>` shape as the
/// query mini-language minus the path; `:` and a path component is a
/// hard error (use [`flatbuffers_query`] for path traversal).
///
/// Suitable for `CHECK` constraints — never raises on a *buffer*
/// problem, only on caller-error or schema-config problems
/// (malformed `table_name`, unknown schema, schema uses a v0.1-
/// unsupported feature such as `Vector64`):
///
/// | Situation                                | Result   |
/// | ---------------------------------------- | -------- |
/// | `buf` is empty                           | `false`  |
/// | `table_name` is empty / malformed        | `ERROR`  |
/// | Schema not registered                    | `ERROR`  |
/// | Schema's registered root table ≠ `table` | `false`  |
/// | Schema uses unsupported feature (Vector64) | `ERROR` |
/// | Verifier rejects `buf`                   | `false`  |
/// | Verifier accepts `buf`                   | `true`   |
///
/// Note that bound exceedances (depth / table-count / apparent-size)
/// are reported as `false` rather than `ERROR` here, because the
/// boolean contract is what `CHECK` needs. Operators who want hard
/// failures on bound exceedances should use [`flatbuffers_query`]
/// inside the constraint instead. Schema-feature rejections are the
/// only exception to the boolean contract: they always ERROR,
/// because a permanently-broken schema would otherwise manifest as a
/// `CHECK` that silently rejects every row.
#[pg_extern(stable, parallel_safe, strict)]
fn flatbuffers_verify(table_name: &str, buf: &[u8]) -> bool {
    if buf.is_empty() {
        return false;
    }

    let (schema_name, expected_root) = split_schema_and_table(table_name).unwrap_or_else(|msg| {
        error!("flatbuffers_verify: invalid table_name {table_name:?}: {msg}")
    });

    let cached = lookup_schema(schema_name);
    if cached.root_table != expected_root {
        return false;
    }

    match verify(buf, &cached.schema(), &current_bounds()) {
        Ok(()) => true,
        // Schema-level rejection (e.g. Vector64) is a *config*
        // problem, not a buffer problem — surface it loudly so the
        // operator notices before users discover it through a
        // mysterious CHECK constraint that never passes. This is the
        // only escape hatch from the boolean contract; every other
        // verifier failure (bound exceedance, structural malformation)
        // still goes through to `false`.
        Err(e) if e.is_schema_feature_rejection() => {
            error!("flatbuffers_verify: {e}")
        }
        Err(_) => false,
    }
}
