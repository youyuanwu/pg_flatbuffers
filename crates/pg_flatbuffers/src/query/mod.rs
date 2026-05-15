//! Query parser + AST for the `[<schema>:]<table>:<path>` mini-language.
//!
//! This module is intentionally pure Rust (no `pgrx` imports) so the
//! parser is unit-testable without spinning up a Postgres backend
//! (`cargo test` is enough; `cargo pgrx test` is not required for
//! anything in this directory).
//!
//! See `docs/design.md` §4.3 (language) and §7.1 (parse).

pub mod ast;
pub mod executor;
pub mod parser;

// The re-exports below are deliberate public-API surface but no
// in-crate consumer reaches into the parent module path yet — every
// internal call site uses the inner-module path (`query::ast::*`,
// `query::executor::*`, `query::parser::*`). The lint will start
// firing again once an external consumer (or a future
// `pg_flatbuffers_macros`-style sibling crate) routes through these.
#[allow(
    unused_imports,
    reason = "public-API surface; no in-crate consumer routes through `query::*` yet"
)]
pub use ast::{FieldRef, MapKey, ParseError, ParseErrorKind, Query, Step};
#[allow(
    unused_imports,
    reason = "public-API surface; no in-crate consumer routes through `query::*` yet"
)]
pub use executor::{execute, ExecuteError};
#[allow(
    unused_imports,
    reason = "public-API surface; bound/length constants will be wired through the deferred GUC plumbing slice (§10)"
)]
pub use parser::{parse, parse_with_bounds, DEFAULT_MAX_PATH_DEPTH, DEFAULT_MAX_QUERY_LENGTH};
