# Compatibility, build, packaging, and testing

## Postgres compatibility

Primary target for v0.1: **PG 18 only**. The crate's feature flag is wired in
[`crates/pg_flatbuffers/Cargo.toml`](../../crates/pg_flatbuffers/Cargo.toml):

```toml
[features]
default = ["pg18"]
pg17    = ["pgrx/pg17", "pgrx-tests/pg17"]
pg18    = ["pgrx/pg18", "pgrx-tests/pg18"]
```

PG 17 is a planned target for v0.2 (see [roadmap.md](roadmap.md)) once the
v0.1 surface is stable; the design carries no PG-18-only assumptions, so
adding PG 17 should reduce to flipping the feature flag and a CI matrix row.

Older majors (PG 14–16) are not a goal: they may continue to work incidentally
as long as pgrx supports them and no PG 18-only APIs are used, but the
project does not test against them and bug reports specific to those
versions will be closed as out of scope. PG 13 and earlier are explicitly
unsupported.

## Build and packaging

[`Justfile`](../../Justfile) is the canonical entry point for local
development:

| Recipe | What it does |
| --- | --- |
| `just init` | Provision PG 18 in `$PGRX_HOME` (downloads source, builds with `--enable-cassert`). One-time, ~5–10 min. |
| `just build pg18` | Compile the extension against PG 18. |
| `just test pg18` | Run the pgrx test harness against PG 18 (covers both pure-Rust `#[test]` and `#[pg_test]`). |
| `just run pg18` | Start a psql session on a freshly-built extension. |
| `just package pg18` | Produce an installable tree (`lib/`, `extension/`). |
| `just check` | CI gate: fmt-check + clippy + pgrx tests. |
| `just unit` | Pure-Rust unit tests (no Postgres needed). Fast inner loop. |
| `just doctor` | Diagnose the local pgrx environment. |
| `just nuke-pgrx` | Wipe the local Postgres install. Forces re-`init`. |

The workspace `target/` directory is shared across crates; reproducible
release builds set `CARGO_TARGET_DIR` explicitly.

## Testing strategy

1. **Unit tests in Rust** for the path parser, executor, verifier, and JSON
   walkers. Run with `cargo test -p pg_flatbuffers --lib` or `just unit`.
2. **Reflection-driven tests** against fixtures defined directly in the test
   modules using the `flatbuffers-reflection` builders
   (e.g. [`functions/tests/fixtures.rs`](../../crates/pg_flatbuffers/src/functions/tests/),
   [`query/executor/tests/`](../../crates/pg_flatbuffers/src/query/executor/tests/)).
3. **pgrx integration tests** (`#[pg_test]`) that boot a real Postgres,
   install the extension, register a schema, and exercise the SQL surface
   end-to-end. Run via `just test pg18`.
4. **Privilege regression tests** assert the safety boundary holds: an
   unprivileged role's `INSERT` into `flatbuffers_schemas` fails by default;
   `flatbuffers_validate_schema` rejects a malformed `.bfbs` at INSERT time
   with no row reaching the cache; non-superuser `SET` of any `SUSET` GUC
   fails with `ERROR: permission denied`. Lives in
   [`functions/tests/`](../../crates/pg_flatbuffers/src/functions/tests/).

### Planned testing extensions

- **Generative tests** (`proptest`) that build arbitrary buffers from a
  simple schema and assert `from_json(to_json(b)) == b` (modulo field
  ordering). Tracked in [roadmap.md](roadmap.md).
- **Fuzz target** (`cargo +nightly fuzz`) on the path parser and on the
  verifier+executor combo with a tiny corpus of valid buffers as seed.
