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

pub use ast::{FieldRef, MapKey, ParseError, ParseErrorKind, Query, Step};
pub use executor::{execute, ExecuteError};
pub use parser::{parse, parse_with_bounds, DEFAULT_MAX_PATH_DEPTH, DEFAULT_MAX_QUERY_LENGTH};
