//! pgrx integration tests for `functions::*`.
//!
//! Each per-area submodule hosts its own `#[pgrx::pg_schema] mod tests { ... }`
//! block; pgrx-tests dispatches every `#[pg_test]` fn via `SELECT tests.<fn>()`,
//! so all of them coexist in a single SQL schema named `tests`. Shared
//! fixtures live in [`fixtures`] as `pub(super)` items so per-area files can
//! `use super::fixtures::*`.

mod fixtures;

mod from_json;
mod guc;
mod query;
mod query_array;
mod query_multi;
mod root_type;
mod to_json;
mod vector64;
mod verify;
