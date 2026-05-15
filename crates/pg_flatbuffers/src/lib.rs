//! pg_flatbuffers — Postgres extension for querying and converting
//! FlatBuffers payloads stored in `bytea` columns.
//!
//! v0.1 surface (this slice): just `flatbuffers_extension_version()`.
//! Subsequent slices add the schema catalog, query parser/executor,
//! and JSON conversion per `docs/design.md`.

use pgrx::prelude::*;

::pgrx::pg_module_magic!(name, version);

/// Returns the extension version as a packed integer:
/// `MAJOR * 10_000 + MINOR * 100 + PATCH`.
///
/// Sourced from `Cargo.toml` at compile time, so bumping the package
/// version automatically bumps this. Wire-compatible with the
/// `flatbuffers_extension_version()` contract in `docs/design.md` §4.2.
#[pg_extern(immutable, parallel_safe)]
fn flatbuffers_extension_version() -> i32 {
    const MAJOR: i32 = parse_u16(env!("CARGO_PKG_VERSION_MAJOR")) as i32;
    const MINOR: i32 = parse_u16(env!("CARGO_PKG_VERSION_MINOR")) as i32;
    const PATCH: i32 = parse_u16(env!("CARGO_PKG_VERSION_PATCH")) as i32;
    MAJOR * 10_000 + MINOR * 100 + PATCH
}

/// Tiny `const fn` u16 parser so the version is computed at compile
/// time and we don't pay for `str::parse` at every call.
const fn parse_u16(s: &str) -> u16 {
    let bytes = s.as_bytes();
    let mut i = 0;
    let mut acc: u16 = 0;
    while i < bytes.len() {
        let b = bytes[i];
        assert!(b.is_ascii_digit(), "version segment must be numeric");
        acc = acc * 10 + (b - b'0') as u16;
        i += 1;
    }
    acc
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(any(test, feature = "pg_test"))]
#[pg_schema]
mod tests {
    use pgrx::prelude::*;

    #[pg_test]
    fn version_is_packed_int() {
        // v0.0.1 → 0 * 10_000 + 0 * 100 + 1 = 1
        let v = Spi::get_one::<i32>("SELECT flatbuffers_extension_version()")
            .expect("SPI failure")
            .expect("NULL from version()");
        assert_eq!(v, 1);
    }
}

/// `cargo pgrx test` discovery hook.
#[cfg(test)]
pub mod pg_test {
    pub fn setup(_options: Vec<&str>) {}

    #[must_use]
    pub fn postgresql_conf_options() -> Vec<&'static str> {
        vec![]
    }
}
