//! Private helpers shared across the entry-point submodules:
//!
//! * [`split_schema_and_table`] — parser for the
//!   `[<schema>:]<table>` shape used by [`super::verify::flatbuffers_verify`].
//! * [`should_error_on`] / [`resolve_execute_error`] — the
//!   `strict`-aware classifier that decides whether an
//!   [`ExecuteError`] surfaces as a Postgres `ERROR` or as the
//!   per-shape "no leaves" sentinel. Used by every query-shaped
//!   entry point.
//! * [`current_execute_options`] — materialises an
//!   [`ExecuteOptions`] from the per-session USERSET GUCs on each
//!   call so a `SET` takes effect on the very next invocation.

use super::DEFAULT_SCHEMA;
use crate::guc::{current_fill_scalar_defaults, current_key_lookup_strict};
use crate::query::{ExecuteError, ExecuteOptions};
use pgrx::prelude::*;

/// Parse a `[<schema>:]<table>` shape into its parts. `<table>` may
/// itself contain dots (FlatBuffers namespaces) but never `:`. The
/// query mini-language's full parser is overkill here because there
/// is no path component to handle.
pub(super) fn split_schema_and_table(input: &str) -> Result<(&str, &str), &'static str> {
    if input.is_empty() {
        return Err("table_name is empty");
    }
    match input.split_once(':') {
        None => Ok((DEFAULT_SCHEMA, input)),
        Some((schema, table)) => {
            if schema.is_empty() {
                return Err("schema name is empty (before the ':')");
            }
            if table.is_empty() {
                return Err("table name is empty (after the ':')");
            }
            if table.contains(':') {
                // Catches three-component inputs like
                // `default:Foo:bar` — guard against operators
                // accidentally pasting a full query string.
                return Err("expected `[schema:]table`, found extra `:`");
            }
            Ok((schema, table))
        }
    }
}

/// Classify an [`ExecuteError`] under the current
/// `pg_flatbuffers.strict` setting. Returns `true` when the call
/// site should raise ERROR; `false` when it should substitute the
/// per-shape "no leaves" sentinel.
///
/// Rules (design §10 "strict does not relax bounds"):
///
/// - `strict = on` → always ERROR.
/// - `strict = off` and a verifier *bound* exceedance (depth, table
///   count, apparent size) → still ERROR; `USERSET` cannot weaken a
///   `SUSET`-protected bound.
/// - `strict = off` and a *schema-feature* rejection
///   (`UnsupportedSchemaFeature`, e.g. Vector64) → still ERROR. A
///   schema-level mismatch is a config problem, not a buffer
///   problem; silencing would mask a permanent registration error.
/// - `strict = off` and a verifier *structural* failure (malformed
///   bytes, missing required field, etc.) → no ERROR; substitute.
/// - `strict = off` and any *non-verifier* failure (`FieldNotFound`,
///   `UnsupportedType`, `UnsupportedStep`, `Internal`) → still
///   ERROR. These are *caller* / *schema* problems, not buffer
///   problems, so silencing them would mask bugs.
pub(super) fn should_error_on(strict: bool, e: &ExecuteError) -> bool {
    if strict {
        return true;
    }
    match e {
        ExecuteError::Verify(v) => v.is_bound_exceedance() || v.is_schema_feature_rejection(),
        _ => true,
    }
}

/// Apply [`should_error_on`] and either raise Postgres ERROR with a
/// caller-prefixed message, or return the empty `Vec<Option<String>>`
/// that the call site reshapes into its public no-match sentinel
/// (`NULL` / `text[] = '{}'` / zero rows). [`error!`] diverges, so
/// the function appears to "return" `Vec::new()` only on the
/// substitute branch.
pub(super) fn resolve_execute_error(
    fn_name: &str,
    e: ExecuteError,
    strict: bool,
) -> Vec<Option<String>> {
    if should_error_on(strict, &e) {
        error!("{fn_name}: {e}");
    }
    Vec::new()
}

/// Materialise an [`ExecuteOptions`] from the current per-session
/// USERSET GUC values. Called once per public SQL entry point so a
/// `SET pg_flatbuffers.fill_scalar_defaults = ...` or `SET
/// pg_flatbuffers.key_lookup_strict = ...` takes effect on the very
/// next call in the same session.
pub(super) fn current_execute_options() -> ExecuteOptions {
    ExecuteOptions {
        fill_scalar_defaults: current_fill_scalar_defaults(),
        key_lookup_strict: current_key_lookup_strict(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_no_colon_uses_default_schema() {
        assert_eq!(
            split_schema_and_table("Order").unwrap(),
            ("default", "Order")
        );
    }

    #[test]
    fn split_with_schema() {
        assert_eq!(
            split_schema_and_table("myco:Order").unwrap(),
            ("myco", "Order")
        );
    }

    #[test]
    fn split_namespaced_table_no_schema_prefix() {
        // `myco.orders.Order` is a fully-qualified FB table name
        // (dots, not colons) — no schema prefix.
        assert_eq!(
            split_schema_and_table("myco.orders.Order").unwrap(),
            ("default", "myco.orders.Order"),
        );
    }

    #[test]
    fn split_rejects_empty_input() {
        assert!(split_schema_and_table("").is_err());
    }

    #[test]
    fn split_rejects_empty_schema() {
        assert!(split_schema_and_table(":Order").is_err());
    }

    #[test]
    fn split_rejects_empty_table() {
        assert!(split_schema_and_table("myco:").is_err());
    }

    #[test]
    fn split_rejects_three_components() {
        // Likely a user pasted a full `flatbuffers_query` argument.
        assert!(split_schema_and_table("default:Order:id").is_err());
    }

    // -- should_error_on classifier --

    use crate::query::ExecuteError;
    use crate::verify::VerifyError;

    fn structural_verify_err() -> ExecuteError {
        ExecuteError::Verify(VerifyError::Invalid(
            "Range [0, 4) is out of bounds".to_owned(),
        ))
    }

    fn bound_verify_err() -> ExecuteError {
        ExecuteError::Verify(VerifyError::Invalid("depth 65 exceeds limit 64".to_owned()))
    }

    #[test]
    fn should_error_on_strict_on_structural_is_true() {
        assert!(should_error_on(true, &structural_verify_err()));
    }

    #[test]
    fn should_error_on_strict_on_bound_is_true() {
        assert!(should_error_on(true, &bound_verify_err()));
    }

    #[test]
    fn should_error_on_strict_off_structural_is_false() {
        // strict = off swallows a structural verifier failure into
        // the substitute path.
        assert!(!should_error_on(false, &structural_verify_err()));
    }

    #[test]
    fn should_error_on_strict_off_bound_still_errors() {
        // §10: strict does not relax bounds — depth / tables /
        // apparent size always ERROR even with strict = off.
        assert!(should_error_on(false, &bound_verify_err()));
    }

    #[test]
    fn should_error_on_strict_off_non_verify_still_errors() {
        // Caller / schema errors are not silenced by strict = off.
        let e = ExecuteError::FieldNotFound {
            what: "missing".to_owned(),
            table: "T".to_owned(),
        };
        assert!(should_error_on(false, &e));
    }

    #[test]
    fn should_error_on_schema_feature_rejection_always_errors() {
        // Vector64 (and any future unsupported-schema-feature
        // variant) is a *config* problem. strict = off MUST NOT
        // silence it — otherwise an operator who registered a
        // broken schema would get rows with NULL leaves and no
        // visible signal.
        let e = ExecuteError::Verify(VerifyError::UnsupportedSchemaFeature {
            feature: "Vector64",
            table: "V64".to_owned(),
            field: "items".to_owned(),
        });
        assert!(should_error_on(true, &e));
        assert!(should_error_on(false, &e));
    }
}
