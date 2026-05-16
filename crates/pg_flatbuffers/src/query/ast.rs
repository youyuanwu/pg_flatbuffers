//! AST for the `pg_flatbuffers` query mini-language.
//!
//! See `docs/design/sql-surface.md` (query language) and
//! `docs/design/query-execution.md` (parse). The grammar is small enough
//! that a hand-written recursive-descent parser (`super::parser`) is
//! sufficient.
//!
//! Strings inside the AST are owned (`String`) rather than borrowed
//! from the input. Parsing happens once per executor call and the
//! resulting `Query` outlives the input slice (it gets handed to the
//! schema-cache + executor over an SPI/ereport boundary), so the
//! lifetime simplification is worth the small allocation cost.

use std::fmt;

/// A parsed `[<schema>:]<table>:<path>` query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Query {
    /// Schema name from the `flatbuffers_schemas` catalog. `None`
    /// means the executor should resolve `"default"`.
    pub schema: Option<String>,
    /// Fully-qualified FlatBuffers table name (namespace dot-separated,
    /// e.g. `"myco.orders.Order"`). The parser does not validate that
    /// the named table actually exists in the registered schema; that
    /// is the executor's job.
    pub root: String,
    /// Path steps in left-to-right traversal order. Always non-empty:
    /// the parser rejects an empty path so each `Query` describes at
    /// least one selector.
    pub steps: Vec<Step>,
}

/// One step in a path. See `docs/design/sql-surface.md` for semantics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// `field` or `submessage.field` (one segment per `Step::Field`)
    /// or `#7` for the field-id form.
    Field(FieldRef),
    /// `field[7]` — vector index. Out-of-bounds is the *executor's*
    /// responsibility to short-circuit to NULL; the parser only
    /// rejects negative indices (which can't appear here because the
    /// grammar forbids `-`).
    Index(usize),
    /// `field[*]` — every element of a vector.
    All,
    /// `field[abc]` — lookup in a `(key)`-annotated vector.
    /// The parser only emits the `Text` variant; `MapKey::Int` is
    /// reserved for an executor-side promotion when the keyed field
    /// is integral (see §4.3, "Map-like selectors").
    MapKey(MapKey),
    /// `field|keys` — enumerate the key field of a `(key)`-annotated
    /// vector.
    MapKeys,
    /// `field|type` — yield the **name** of the active variant of a
    /// union field as a string leaf (e.g. `"TableA"`, `"NONE"`).
    /// Complements the auto-generated discriminator field
    /// (`<field>_type`) which exposes the numeric tag instead. Only
    /// valid in tail position immediately after a union-typed Field;
    /// the executor rejects it otherwise.
    UnionType,
    // NB: `Step::UnionMember` from the design doc is intentionally
    // absent here. The parser cannot tell a union-member name from a
    // sub-table field name syntactically (both are dotted
    // identifiers); the executor disambiguates against the schema.
}

/// Field reference within a path segment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldRef {
    /// `field` — resolved by name.
    Name(String),
    /// `#7` — resolved against `reflection::Field::id()` (the value of
    /// the `(id: N)` annotation, NOT the declaration order).
    Id(u16),
}

/// Map-key literal payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MapKey {
    /// `[abc]` — string key, or any `[…]` whose contents are not a
    /// pure unsigned-integer literal.
    Text(String),
    /// `[123]` after the executor has determined the keyed field is
    /// integral. Currently never produced by the parser; kept here so
    /// the executor can construct the variant without a separate
    /// internal type.
    #[allow(
        dead_code,
        reason = "executor pattern-matches on this variant; constructor lands with the future numeric-literal-key parser slice"
    )]
    Int(i64),
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Parse-time error. `position` is a byte offset into the original
/// input string for diagnostics (callers may turn it into a column
/// number for `ereport(ERROR, …, errcontext(…))`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub kind: ParseErrorKind,
    pub position: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseErrorKind {
    /// Input exceeded `max_query_length`.
    QueryTooLong { len: usize, limit: usize },
    /// Path nesting exceeded `max_path_depth`.
    PathTooDeep { depth: usize, limit: usize },
    /// No `:` found, so neither `table` nor `path` could be located.
    MissingPathSeparator,
    /// Either side of a `:` was empty (e.g. `":path"`, `"table:"`,
    /// `"schema::path"`).
    EmptyComponent { what: &'static str },
    /// More than two `:` characters in the input.
    TooManyColons,
    /// An identifier was expected but a different character was found.
    ExpectedIdentifier { found: char },
    /// `#` was not followed by digits, or the digits did not fit in
    /// `u16` (FlatBuffers field ids are 16-bit per `reflection.fbs`).
    InvalidFieldId { reason: &'static str },
    /// A `[` was not closed by a matching `]`.
    UnclosedBracket,
    /// An empty bracket pair `[]` was encountered.
    EmptyBracket,
    /// A `[123…]` literal exceeded `usize`.
    IndexTooLarge,
    /// `|` appeared but was not followed by a recognised keyword
    /// (currently `keys` or `type`).
    UnknownPipeKeyword { found: String },
    /// Unexpected trailing characters after a `|<keyword>` marker.
    TrailingAfterPipe,
    /// A character that is not part of the grammar appeared mid-path.
    UnexpectedChar { found: char },
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use ParseErrorKind::*;
        match &self.kind {
            QueryTooLong { len, limit } => write!(
                f,
                "query string is {len} bytes; exceeds limit of {limit} (see pg_flatbuffers.max_query_length)",
            ),
            PathTooDeep { depth, limit } => write!(
                f,
                "path nesting depth {depth} exceeds limit of {limit} (see pg_flatbuffers.max_path_depth)",
            ),
            MissingPathSeparator => {
                f.write_str("query missing ':' separator between table and path")
            }
            EmptyComponent { what } => write!(f, "empty {what} in query"),
            TooManyColons => {
                f.write_str("query has too many ':' separators (expected at most two)")
            }
            ExpectedIdentifier { found } => write!(f, "expected identifier, found {found:?}"),
            InvalidFieldId { reason } => write!(f, "invalid field id after '#': {reason}"),
            UnclosedBracket => f.write_str("unclosed '[' in path"),
            EmptyBracket => f.write_str("empty '[]' in path"),
            IndexTooLarge => f.write_str("vector index does not fit in usize"),
            UnknownPipeKeyword { found } => write!(f, "unknown '|' keyword: {found:?}"),
            TrailingAfterPipe => f.write_str("trailing characters after '|<keyword>'"),
            UnexpectedChar { found } => write!(f, "unexpected character {found:?}"),
        }
    }
}

impl std::error::Error for ParseError {}
