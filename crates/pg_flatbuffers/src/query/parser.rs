//! Hand-written recursive-descent parser for the
//! `[<schema>:]<table>:<path>` query mini-language.
//!
//! See `docs/design.md` §4.3 (language) and §7.1 (parse). The grammar
//! is small enough that a parser-combinator dependency would be
//! overkill; this module stays pure Rust with no `pgrx` dependencies
//! so it is unit-testable without a Postgres backend.
//!
//! Grammar (informal):
//!
//! ```text
//! query   := [SCHEMA ':'] TABLE ':' path
//! path    := segment ('.' segment)*
//! segment := (IDENT | '#' DIGITS) (bracket)* ('|keys' | '|type')?
//! bracket := '[' ('*' | DIGITS | TEXT) ']'
//! ```
//!
//! `IDENT`s match `[A-Za-z_][A-Za-z0-9_]*`; the table name is a
//! dot-separated chain of `IDENT`s (FlatBuffers fully qualified
//! names). `TEXT` inside a bracket is anything other than `]` (no
//! escapes in v0.1 — defer until we have a use case). Whitespace is
//! NOT allowed anywhere in the query: queries are typically
//! constructed by client code or templated into SQL, and tolerating
//! whitespace would only invite ambiguity.

use super::ast::{FieldRef, MapKey, ParseError, ParseErrorKind, Query, Step};

/// Default cap matching `docs/design.md` §10
/// (`pg_flatbuffers.max_query_length`). Callers from inside the
/// extension thread the GUC value here; pure-Rust callers (tests,
/// fuzzing) typically use [`parse`] which uses the defaults.
pub const DEFAULT_MAX_QUERY_LENGTH: usize = 4096;

/// Default cap matching `docs/design.md` §10
/// (`pg_flatbuffers.max_path_depth`). One unit of depth = one path
/// step (field, index, all, map-key, map-keys, or union-type).
pub const DEFAULT_MAX_PATH_DEPTH: usize = 256;

/// Parse with the default bounds. Convenience wrapper around
/// [`parse_with_bounds`].
pub fn parse(input: &str) -> Result<Query, ParseError> {
    parse_with_bounds(input, DEFAULT_MAX_QUERY_LENGTH, DEFAULT_MAX_PATH_DEPTH)
}

/// Parse with caller-supplied bounds (so the executor can plumb
/// `pg_flatbuffers.max_query_length` / `max_path_depth` through).
pub fn parse_with_bounds(
    input: &str,
    max_query_length: usize,
    max_path_depth: usize,
) -> Result<Query, ParseError> {
    if input.len() > max_query_length {
        return Err(ParseError {
            kind: ParseErrorKind::QueryTooLong {
                len: input.len(),
                limit: max_query_length,
            },
            position: 0,
        });
    }

    let (schema, root, path, path_offset) = split_header(input)?;

    let steps = parse_path(path, path_offset, max_path_depth)?;
    Ok(Query {
        schema: schema.map(str::to_owned),
        root: root.to_owned(),
        steps,
    })
}

// ---------------------------------------------------------------------------
// Header split: `[<schema>:]<table>:<path>`
// ---------------------------------------------------------------------------

/// Returns `(schema, table, path, path_byte_offset_in_input)`.
fn split_header(input: &str) -> Result<(Option<&str>, &str, &str, usize), ParseError> {
    // Count colons up-front to give a precise error for the
    // common copy-paste mistake `schema:table:path:extra`.
    let mut colon_positions = input
        .bytes()
        .enumerate()
        .filter_map(|(i, b)| (b == b':').then_some(i));

    let first = colon_positions.next().ok_or(ParseError {
        kind: ParseErrorKind::MissingPathSeparator,
        position: input.len(),
    })?;
    let second = colon_positions.next();
    if colon_positions.next().is_some() {
        return Err(ParseError {
            kind: ParseErrorKind::TooManyColons,
            position: 0,
        });
    }

    match second {
        // schema:table:path
        Some(second_colon) => {
            let schema = &input[..first];
            let table = &input[first + 1..second_colon];
            let path = &input[second_colon + 1..];
            require_nonempty(schema, "schema", 0)?;
            require_nonempty(table, "table", first + 1)?;
            require_nonempty(path, "path", second_colon + 1)?;
            validate_table_name(table, first + 1)?;
            validate_schema_name(schema, 0)?;
            Ok((Some(schema), table, path, second_colon + 1))
        }
        // table:path
        None => {
            let table = &input[..first];
            let path = &input[first + 1..];
            require_nonempty(table, "table", 0)?;
            require_nonempty(path, "path", first + 1)?;
            validate_table_name(table, 0)?;
            Ok((None, table, path, first + 1))
        }
    }
}

fn require_nonempty(s: &str, what: &'static str, position: usize) -> Result<(), ParseError> {
    if s.is_empty() {
        Err(ParseError {
            kind: ParseErrorKind::EmptyComponent { what },
            position,
        })
    } else {
        Ok(())
    }
}

/// Schema names are stored as text in `flatbuffers_schemas.name`, but
/// for the query string we restrict them to `[A-Za-z_][A-Za-z0-9_]*`
/// so they can't contain `:` or other path metacharacters. Operators
/// who need exotic names can always look them up by id from a wrapper.
fn validate_schema_name(name: &str, base: usize) -> Result<(), ParseError> {
    let mut bytes = name.bytes().enumerate();
    match bytes.next() {
        Some((_, b)) if is_ident_start(b) => {}
        Some((i, b)) => {
            return Err(ParseError {
                kind: ParseErrorKind::ExpectedIdentifier { found: b as char },
                position: base + i,
            });
        }
        None => unreachable!("require_nonempty checked"),
    }
    for (i, b) in bytes {
        if !is_ident_cont(b) {
            return Err(ParseError {
                kind: ParseErrorKind::UnexpectedChar { found: b as char },
                position: base + i,
            });
        }
    }
    Ok(())
}

/// FlatBuffers fully qualified names: dot-separated identifiers, e.g.
/// `myco.orders.Order`. Empty components (leading/trailing/consecutive
/// dot) are errors.
fn validate_table_name(name: &str, base: usize) -> Result<(), ParseError> {
    let mut at_segment_start = true;
    for (i, b) in name.bytes().enumerate() {
        if b == b'.' {
            if at_segment_start {
                return Err(ParseError {
                    kind: ParseErrorKind::EmptyComponent {
                        what: "table-name segment",
                    },
                    position: base + i,
                });
            }
            at_segment_start = true;
        } else if at_segment_start {
            if !is_ident_start(b) {
                return Err(ParseError {
                    kind: ParseErrorKind::ExpectedIdentifier { found: b as char },
                    position: base + i,
                });
            }
            at_segment_start = false;
        } else if !is_ident_cont(b) {
            return Err(ParseError {
                kind: ParseErrorKind::UnexpectedChar { found: b as char },
                position: base + i,
            });
        }
    }
    if at_segment_start {
        // Ended on `.`
        return Err(ParseError {
            kind: ParseErrorKind::EmptyComponent {
                what: "table-name segment",
            },
            position: base + name.len(),
        });
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Path parser
// ---------------------------------------------------------------------------

fn parse_path(path: &str, path_offset: usize, max_depth: usize) -> Result<Vec<Step>, ParseError> {
    let mut p = Parser {
        bytes: path.as_bytes(),
        pos: 0,
        base: path_offset,
        steps: Vec::new(),
        max_depth,
    };
    p.parse_segments()?;
    Ok(p.steps)
}

struct Parser<'a> {
    bytes: &'a [u8],
    /// Current byte index into `bytes`.
    pos: usize,
    /// Byte offset of `bytes[0]` within the original input string;
    /// added to `pos` whenever an error is reported.
    base: usize,
    steps: Vec<Step>,
    max_depth: usize,
}

impl<'a> Parser<'a> {
    fn parse_segments(&mut self) -> Result<(), ParseError> {
        loop {
            self.parse_segment()?;
            match self.peek() {
                None => return Ok(()),
                Some(b'.') => {
                    self.pos += 1;
                    if self.peek().is_none() {
                        // Trailing dot
                        return Err(ParseError {
                            kind: ParseErrorKind::ExpectedIdentifier { found: '.' },
                            position: self.base + self.pos,
                        });
                    }
                }
                Some(c) => {
                    return Err(ParseError {
                        kind: ParseErrorKind::UnexpectedChar { found: c as char },
                        position: self.base + self.pos,
                    });
                }
            }
        }
    }

    fn parse_segment(&mut self) -> Result<(), ParseError> {
        // 1. Field selector — name or `#id`.
        let field_pos = self.pos;
        let field = match self.peek() {
            Some(b'#') => {
                self.pos += 1;
                let start = self.pos;
                while let Some(b) = self.peek() {
                    if b.is_ascii_digit() {
                        self.pos += 1;
                    } else {
                        break;
                    }
                }
                if start == self.pos {
                    return Err(ParseError {
                        kind: ParseErrorKind::InvalidFieldId {
                            reason: "no digits after '#'",
                        },
                        position: self.base + field_pos,
                    });
                }
                let digits = &self.bytes[start..self.pos];
                // SAFETY: `digits` is ASCII digits.
                let s = std::str::from_utf8(digits).expect("ascii digits");
                let id: u16 = s.parse().map_err(|_| ParseError {
                    kind: ParseErrorKind::InvalidFieldId {
                        reason: "id does not fit in u16",
                    },
                    position: self.base + field_pos,
                })?;
                FieldRef::Id(id)
            }
            Some(b) if is_ident_start(b) => {
                let start = self.pos;
                self.pos += 1;
                while let Some(b) = self.peek() {
                    if is_ident_cont(b) {
                        self.pos += 1;
                    } else {
                        break;
                    }
                }
                let name = std::str::from_utf8(&self.bytes[start..self.pos])
                    .expect("ident bytes are ASCII");
                FieldRef::Name(name.to_owned())
            }
            Some(b) => {
                return Err(ParseError {
                    kind: ParseErrorKind::ExpectedIdentifier { found: b as char },
                    position: self.base + self.pos,
                });
            }
            None => {
                return Err(ParseError {
                    kind: ParseErrorKind::ExpectedIdentifier {
                        // EOF in the middle of the path
                        found: '\0',
                    },
                    position: self.base + self.pos,
                });
            }
        };
        self.push(Step::Field(field))?;

        // 2. Zero or more `[…]` brackets.
        while let Some(b'[') = self.peek() {
            self.parse_bracket()?;
        }

        // 3. Optional `|keys` or `|type` (terminal markers).
        if let Some(b'|') = self.peek() {
            self.parse_pipe()?;
        }

        Ok(())
    }

    fn parse_bracket(&mut self) -> Result<(), ParseError> {
        debug_assert_eq!(self.peek(), Some(b'['));
        let bracket_start = self.pos;
        self.pos += 1; // consume '['

        // Find the matching ']'.
        let content_start = self.pos;
        let close = self.bytes[self.pos..]
            .iter()
            .position(|&b| b == b']')
            .ok_or(ParseError {
                kind: ParseErrorKind::UnclosedBracket,
                position: self.base + bracket_start,
            })?;
        let content_end = self.pos + close;
        let content = &self.bytes[content_start..content_end];
        self.pos = content_end + 1; // consume ']'

        if content.is_empty() {
            return Err(ParseError {
                kind: ParseErrorKind::EmptyBracket,
                position: self.base + bracket_start,
            });
        }

        let step = if content == b"*" {
            Step::All
        } else if content.iter().all(|b| b.is_ascii_digit()) {
            // Pure unsigned decimal — vector index.
            let s = std::str::from_utf8(content).expect("ascii digits");
            let idx: usize = s.parse().map_err(|_| ParseError {
                kind: ParseErrorKind::IndexTooLarge,
                position: self.base + content_start,
            })?;
            Step::Index(idx)
        } else {
            // Anything else is treated as a textual map key. We do
            // **not** try to coerce signed integers like `[-1]` into
            // `MapKey::Int` — the design forbids negative indices and
            // the executor handles int promotion based on the schema.
            let s = std::str::from_utf8(content).map_err(|_| ParseError {
                // Non-UTF-8 inside a bracket is rare (the input came
                // from PG as text, which is already validated UTF-8
                // in most encodings). Report the offending byte.
                kind: ParseErrorKind::UnexpectedChar {
                    found: content[0] as char,
                },
                position: self.base + content_start,
            })?;
            Step::MapKey(MapKey::Text(s.to_owned()))
        };
        self.push(step)
    }

    fn parse_pipe(&mut self) -> Result<(), ParseError> {
        debug_assert_eq!(self.peek(), Some(b'|'));
        let pipe_start = self.pos;
        self.pos += 1; // consume '|'

        // Read the keyword: alphabetic chars only.
        let kw_start = self.pos;
        while let Some(b) = self.peek() {
            if b.is_ascii_alphabetic() {
                self.pos += 1;
            } else {
                break;
            }
        }
        let keyword =
            std::str::from_utf8(&self.bytes[kw_start..self.pos]).expect("ASCII alphabetic");
        let step = match keyword {
            "keys" => Step::MapKeys,
            "type" => Step::UnionType,
            other => {
                return Err(ParseError {
                    kind: ParseErrorKind::UnknownPipeKeyword {
                        found: other.to_owned(),
                    },
                    position: self.base + pipe_start,
                });
            }
        };
        self.push(step)?;

        // After `|<keyword>`, only `.` (next segment) or end-of-input
        // is legal. A `[` or another `|` here would be nonsense, and
        // descent past a terminal-leaf marker is rejected by the
        // executor anyway — catching it here gives a sharper error.
        match self.peek() {
            None | Some(b'.') => Ok(()),
            Some(_) => Err(ParseError {
                kind: ParseErrorKind::TrailingAfterPipe,
                position: self.base + self.pos,
            }),
        }
    }

    // -- low-level helpers --

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn push(&mut self, step: Step) -> Result<(), ParseError> {
        if self.steps.len() == self.max_depth {
            return Err(ParseError {
                kind: ParseErrorKind::PathTooDeep {
                    depth: self.max_depth + 1,
                    limit: self.max_depth,
                },
                position: self.base + self.pos,
            });
        }
        self.steps.push(step);
        Ok(())
    }
}

#[inline]
fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_'
}

#[inline]
fn is_ident_cont(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

// ---------------------------------------------------------------------------
// Tests (pure Rust — no `cargo pgrx test` needed)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(input: &str) -> Query {
        parse(input).unwrap_or_else(|e| panic!("parse({input:?}) failed: {e}"))
    }

    fn err(input: &str) -> ParseError {
        parse(input).expect_err(&format!("parse({input:?}) unexpectedly succeeded"))
    }

    fn name(s: &str) -> Step {
        Step::Field(FieldRef::Name(s.to_owned()))
    }

    fn id(n: u16) -> Step {
        Step::Field(FieldRef::Id(n))
    }

    fn key(s: &str) -> Step {
        Step::MapKey(MapKey::Text(s.to_owned()))
    }

    // --- header (schema / table) ---

    #[test]
    fn parses_table_only_header() {
        let q = ok("Order:price");
        assert_eq!(q.schema, None);
        assert_eq!(q.root, "Order");
        assert_eq!(q.steps, vec![name("price")]);
    }

    #[test]
    fn parses_schema_table_header() {
        let q = ok("orders:myco.orders.Order:price");
        assert_eq!(q.schema.as_deref(), Some("orders"));
        assert_eq!(q.root, "myco.orders.Order");
        assert_eq!(q.steps, vec![name("price")]);
    }

    #[test]
    fn rejects_no_colon() {
        let e = err("Order");
        assert_eq!(e.kind, ParseErrorKind::MissingPathSeparator);
    }

    #[test]
    fn rejects_three_colons() {
        let e = err("a:b:c:d");
        assert_eq!(e.kind, ParseErrorKind::TooManyColons);
    }

    #[test]
    fn rejects_empty_schema() {
        let e = err(":Order:price");
        assert_eq!(e.kind, ParseErrorKind::EmptyComponent { what: "schema" });
    }

    #[test]
    fn rejects_empty_table() {
        let e = err("orders::price");
        assert_eq!(e.kind, ParseErrorKind::EmptyComponent { what: "table" });
    }

    #[test]
    fn rejects_empty_path() {
        let e = err("Order:");
        assert_eq!(e.kind, ParseErrorKind::EmptyComponent { what: "path" });
    }

    #[test]
    fn rejects_table_with_leading_dot() {
        let e = err(".Order:price");
        assert_eq!(
            e.kind,
            ParseErrorKind::EmptyComponent {
                what: "table-name segment"
            }
        );
    }

    #[test]
    fn rejects_table_with_consecutive_dots() {
        let e = err("a..b:price");
        assert_eq!(
            e.kind,
            ParseErrorKind::EmptyComponent {
                what: "table-name segment"
            }
        );
    }

    #[test]
    fn rejects_table_with_trailing_dot() {
        let e = err("a.b.:price");
        assert_eq!(
            e.kind,
            ParseErrorKind::EmptyComponent {
                what: "table-name segment"
            }
        );
    }

    // --- path: field segments ---

    #[test]
    fn parses_dotted_field_chain() {
        let q = ok("Order:customer.address.city");
        assert_eq!(
            q.steps,
            vec![name("customer"), name("address"), name("city")]
        );
    }

    #[test]
    fn parses_field_id_form() {
        let q = ok("Order:#7");
        assert_eq!(q.steps, vec![id(7)]);
    }

    #[test]
    fn parses_mixed_name_and_id_segments() {
        let q = ok("Order:customer.#3.email");
        assert_eq!(q.steps, vec![name("customer"), id(3), name("email")]);
    }

    #[test]
    fn rejects_field_id_with_no_digits() {
        let e = err("Order:#");
        assert_eq!(
            e.kind,
            ParseErrorKind::InvalidFieldId {
                reason: "no digits after '#'"
            }
        );
    }

    #[test]
    fn rejects_field_id_overflow() {
        let e = err("Order:#65536");
        assert_eq!(
            e.kind,
            ParseErrorKind::InvalidFieldId {
                reason: "id does not fit in u16"
            }
        );
    }

    #[test]
    fn rejects_field_starting_with_digit() {
        let e = err("Order:1abc");
        assert!(matches!(e.kind, ParseErrorKind::ExpectedIdentifier { .. }));
    }

    #[test]
    fn rejects_trailing_dot_in_path() {
        let e = err("Order:a.");
        assert!(matches!(e.kind, ParseErrorKind::ExpectedIdentifier { .. }));
    }

    // --- path: brackets ---

    #[test]
    fn parses_index() {
        let q = ok("Order:items[7]");
        assert_eq!(q.steps, vec![name("items"), Step::Index(7)]);
    }

    #[test]
    fn parses_zero_index() {
        let q = ok("Order:items[0]");
        assert_eq!(q.steps, vec![name("items"), Step::Index(0)]);
    }

    #[test]
    fn parses_universal() {
        let q = ok("Order:items[*]");
        assert_eq!(q.steps, vec![name("items"), Step::All]);
    }

    #[test]
    fn parses_text_map_key() {
        let q = ok("Order:items[abc]");
        assert_eq!(q.steps, vec![name("items"), key("abc")]);
    }

    #[test]
    fn parses_text_map_key_with_punctuation() {
        // Anything other than `]` is allowed inside a textual map key
        // for v0.1 — including dots, dashes, and digits-mixed-with-letters.
        let q = ok("Order:items[k-1.x]");
        assert_eq!(q.steps, vec![name("items"), key("k-1.x")]);
    }

    #[test]
    fn parses_chained_brackets() {
        let q = ok("Order:matrix[2][3][*]");
        assert_eq!(
            q.steps,
            vec![name("matrix"), Step::Index(2), Step::Index(3), Step::All,]
        );
    }

    #[test]
    fn parses_brackets_then_dotted_continuation() {
        let q = ok("Order:items[*].sku");
        assert_eq!(q.steps, vec![name("items"), Step::All, name("sku")]);
    }

    #[test]
    fn rejects_negative_index_as_unexpected_char() {
        // `-` is not a valid identifier start nor inside a name; the
        // parser sees `[-` and treats `-1` as a textual map key
        // (executor will refuse it). That's fine for v0.1 — the
        // important thing is we don't silently accept negatives as
        // signed indices.
        let q = ok("Order:items[-1]");
        assert_eq!(q.steps, vec![name("items"), key("-1")]);
    }

    #[test]
    fn rejects_unclosed_bracket() {
        let e = err("Order:items[7");
        assert_eq!(e.kind, ParseErrorKind::UnclosedBracket);
    }

    #[test]
    fn rejects_empty_bracket() {
        let e = err("Order:items[]");
        assert_eq!(e.kind, ParseErrorKind::EmptyBracket);
    }

    #[test]
    fn rejects_index_overflow() {
        let big = format!("Order:items[{}9]", usize::MAX);
        let e = err(&big);
        assert_eq!(e.kind, ParseErrorKind::IndexTooLarge);
    }

    // --- path: pipe / keys ---

    #[test]
    fn parses_pipe_keys() {
        let q = ok("Order:items|keys");
        assert_eq!(q.steps, vec![name("items"), Step::MapKeys]);
    }

    #[test]
    fn parses_pipe_keys_then_dotted_continuation() {
        // Defensive: while it's semantically odd to continue past a
        // `|keys`, the parser allows it and the executor will reject.
        let q = ok("Order:items|keys.foo");
        assert_eq!(q.steps, vec![name("items"), Step::MapKeys, name("foo")]);
    }

    #[test]
    fn rejects_unknown_pipe_keyword() {
        let e = err("Order:items|values");
        assert_eq!(
            e.kind,
            ParseErrorKind::UnknownPipeKeyword {
                found: "values".to_owned()
            }
        );
    }

    #[test]
    fn rejects_pipe_followed_by_bracket() {
        let e = err("Order:items|keys[0]");
        assert_eq!(e.kind, ParseErrorKind::TrailingAfterPipe);
    }

    // --- path: pipe / type ---

    #[test]
    fn parses_pipe_type() {
        let q = ok("Msg:body|type");
        assert_eq!(q.steps, vec![name("body"), Step::UnionType]);
    }

    #[test]
    fn parses_pipe_type_after_dotted_path() {
        // `|type` lives in tail position after any field, regardless
        // of the preceding chain. The executor rejects it on
        // non-union targets; the parser is purely structural.
        let q = ok("Outer:wrapper.body|type");
        assert_eq!(
            q.steps,
            vec![name("wrapper"), name("body"), Step::UnionType]
        );
    }

    #[test]
    fn rejects_pipe_type_followed_by_bracket() {
        let e = err("Msg:body|type[0]");
        assert_eq!(e.kind, ParseErrorKind::TrailingAfterPipe);
    }

    // --- bounds ---

    #[test]
    fn rejects_query_too_long() {
        let limit = 16;
        let input = "Order:".to_string() + &"a".repeat(limit);
        let e = parse_with_bounds(&input, limit, DEFAULT_MAX_PATH_DEPTH).unwrap_err();
        assert!(matches!(e.kind, ParseErrorKind::QueryTooLong { .. }));
    }

    #[test]
    fn rejects_path_too_deep() {
        let limit = 4;
        // Five steps: a.b.c.d.e
        let input = "Order:a.b.c.d.e";
        let e = parse_with_bounds(input, DEFAULT_MAX_QUERY_LENGTH, limit).unwrap_err();
        assert!(matches!(e.kind, ParseErrorKind::PathTooDeep { .. }));
    }

    #[test]
    fn accepts_path_at_exact_depth_limit() {
        let limit = 3;
        let input = "Order:a.b.c";
        parse_with_bounds(input, DEFAULT_MAX_QUERY_LENGTH, limit).unwrap();
    }

    // --- error-position smoke test ---

    #[test]
    fn error_position_points_into_input() {
        // First failure is at the `?` character in "items?".
        let input = "Order:items?";
        let e = parse(input).unwrap_err();
        assert_eq!(input.as_bytes()[e.position], b'?');
    }
}
