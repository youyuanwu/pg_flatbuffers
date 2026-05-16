//! `flatbuffers_root_type` — diagnostic helper returning the
//! registered root-table name for a schema.

use crate::schema_cache::lookup_schema;
use pgrx::prelude::*;

/// Return the registered root-table name for `schema_name`. Raises
/// `ERROR` when the schema is not registered (same contract as
/// [`crate::schema_cache::lookup_schema`]). Provided as a diagnostic
/// helper so operators can confirm a schema is loaded without dumping
/// the `bfbs` blob.
#[pg_extern(stable, parallel_safe, strict)]
fn flatbuffers_root_type(schema_name: &str) -> String {
    lookup_schema(schema_name).root_table.clone()
}
