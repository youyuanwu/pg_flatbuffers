# Justfile — task runner for pg_flatbuffers
#
# Run `just` with no args (or `just list`) to see available recipes.
# All `cargo pgrx …` recipes target the in-repo Postgres builds under
# `.pgrx/` (see PGRX_HOME below). No sudo, no system Postgres needed.

set shell := ["bash", "-cu"]
set dotenv-load := true

# Sub-files. `import` flat-merges into the same recipe namespace, so a
# recipe `foo` in `just/x.just` is invoked as `just foo`.
import 'just/doctor.just'

# Pin every cargo-pgrx invocation to the repo-local Postgres install
# created by `just init`. Override by exporting PGRX_HOME before calling
# just (e.g. to share a single ~/.pgrx/ across repos).
export PGRX_HOME := env_var_or_default("PGRX_HOME", justfile_directory() / ".pgrx")

# The extension crate name; used by `cargo pgrx --package`.
EXT := "pg_flatbuffers"

# Postgres major targeted by single-version recipes.
# v0.1 ships PG 18 only (see docs/design.md §11). PG 17 is a v0.2 target.
default_pg := "pg18"

# ---------------------------------------------------------------------------
# Default recipe: list everything.
# ---------------------------------------------------------------------------

default:
    @just --list

# ---------------------------------------------------------------------------
# One-time environment setup
# ---------------------------------------------------------------------------

# Provision PG 18 in $PGRX_HOME (downloads source, builds with
# --enable-cassert). One-time, ~5–10 min, ~1.5 GB.
init:
    @echo "PGRX_HOME=$PGRX_HOME"
    cargo pgrx init --pg18 download

# Provision a single major. Usage: `just init-one pg=pg17`.
init-one pg=default_pg:
    cargo pgrx init --{{pg}} download

# Print the resolved pg_config for each provisioned major.
which-pg:
    @cat "$PGRX_HOME/config.toml" 2>/dev/null || \
        echo "no $PGRX_HOME/config.toml — run `just init` first"

# ---------------------------------------------------------------------------
# Build / run / test
# ---------------------------------------------------------------------------

# Compile the extension against a Postgres major. Usage: `just build pg=pg17`.
build pg=default_pg:
    cargo pgrx package --package {{EXT}} --pg-config "$(just _pg-config {{pg}})"

# Start a psql session on a freshly-built extension. Usage: `just run pg=pg17`.
run pg=default_pg:
    cargo pgrx run {{pg}} --package {{EXT}}

# Run the extension test suite against one Postgres major.
test pg=default_pg:
    cargo pgrx test {{pg}} --package {{EXT}}

# Alias kept for forward-compat with the v0.2 PG 17 + PG 18 matrix.
test-all: test

# Pure-Rust unit tests (no Postgres needed). Fast inner loop.
unit:
    cargo test --workspace --lib --bins

# ---------------------------------------------------------------------------
# Quality gates
# ---------------------------------------------------------------------------

fmt:
    cargo fmt --all

fmt-check:
    cargo fmt --all -- --check

lint:
    cargo clippy --workspace --all-targets -- -D warnings

# CI gate: format check + lint + unit tests + pg18.
check: fmt-check lint unit
    just test pg18

# ---------------------------------------------------------------------------
# Packaging
# ---------------------------------------------------------------------------

# Produce an installable tree for one major. Usage: `just package pg=pg17`.
package pg=default_pg:
    cargo pgrx package --package {{EXT}} --pg-config "$(just _pg-config {{pg}})"

# ---------------------------------------------------------------------------
# Cleanup
# ---------------------------------------------------------------------------

clean:
    cargo clean

# Wipe the local Postgres install. Forces re-`init`. Slow to recover.
nuke-pgrx:
    @echo "Removing $PGRX_HOME"
    rm -rf "$PGRX_HOME"

# ---------------------------------------------------------------------------
# Internal helpers (prefixed `_` so they don't show in `just --list`).
# ---------------------------------------------------------------------------

# Resolve the pg_config path for a given major from $PGRX_HOME/config.toml.
_pg-config pg:
    @awk -F'=' '/^{{pg}}[[:space:]]*=/ {gsub(/[" ]/,"",$2); print $2}' \
        "$PGRX_HOME/config.toml"
