//! GUC (Grand Unified Configuration) plumbing for the verifier bounds
//! (see `docs/design.md` §10).
//!
//! ## Scope of this slice
//!
//! Three `SUSET` integer GUCs that map 1:1 to [`Bounds`]:
//!
//! | GUC | Default | Range | Maps to |
//! | --- | --- | --- | --- |
//! | `pg_flatbuffers.max_depth` | 64 | 1..=1024 | [`Bounds::max_depth`] |
//! | `pg_flatbuffers.max_tables` | 1_000_000 | 1..=`i32::MAX` | [`Bounds::max_tables`] |
//! | `pg_flatbuffers.max_apparent_size_mb` | 64 | 1..=16384 | [`Bounds::max_apparent_size`] (× 1 MiB) |
//!
//! Plus one `USERSET` bool:
//!
//! | GUC | Default | Maps to |
//! | --- | --- | --- |
//! | `pg_flatbuffers.strict` | `on` | [`current_strict`] — consumed by [`crate::functions`] to decide whether to ERROR or substitute `NULL` on a structural verifier failure (§10 "strict does not relax bounds"). |
//!
//! [`current_bounds`] materialises a [`Bounds`] from the current GUC
//! values for every call to [`crate::query::execute`] /
//! [`crate::verify::verify`] in [`crate::functions`]. The pure-Rust
//! tests inside [`crate::query::executor`] and [`crate::verify`]
//! continue to use [`Bounds::default`] directly — they have no
//! Postgres backend to register GUCs against.
//!
//! ## Why `SUSET` for the bounds, `USERSET` for `strict`?
//!
//! Per §10 "GUC governance is part of the safety boundary: a bound
//! that any session can raise is not a bound." The three bounds gate
//! verifier DoS resistance, so they require superuser to change. The
//! `strict` GUC, by contrast, only affects the *calling session's*
//! own results (turning structural failures into `NULL` so a scan
//! over a mixed-cleanliness column doesn't abort mid-statement). It
//! cannot weaken the verifier or relax a bound exceedance —
//! [`crate::verify::VerifyError::is_bound_exceedance`] is still
//! consulted before substituting `NULL`, so `USERSET` is safe.
//!
//! The `pg_flatbuffers.proto3_defaults` `USERSET` GUC that affects
//! the executor's absent-scalar branch lives in its own future
//! micro-slice.
//!
//! ## Why `i32` and not `usize`?
//!
//! `pg_sys::DefineCustomIntVariable` takes `int` (= `i32`). The cast
//! at [`current_bounds`] is loss-free because every range maximum
//! fits in `i32` by construction, and the GUC subsystem rejects
//! negative values at `SET` time via the registered `min_value`.

use crate::verify::Bounds;
use pgrx::guc::{GucContext, GucFlags, GucRegistry, GucSetting};

// -- Backing storage -------------------------------------------------------
//
// `GucSetting<i32>` is a thread-safe `Cell<i32>` wrapper. Initial values
// must match the [`Bounds::default`] values byte-for-byte so a backend
// that never `SET`s anything sees the documented defaults.

/// Backing storage for `pg_flatbuffers.max_depth`.
static MAX_DEPTH: GucSetting<i32> = GucSetting::<i32>::new(64);

/// Backing storage for `pg_flatbuffers.max_tables`.
static MAX_TABLES: GucSetting<i32> = GucSetting::<i32>::new(1_000_000);

/// Backing storage for `pg_flatbuffers.max_apparent_size_mb`. Stored
/// in MiB; multiplied by `1024 * 1024` at materialisation time to
/// match `flatbuffers::VerifierOptions::max_apparent_size` (bytes).
static MAX_APPARENT_SIZE_MB: GucSetting<i32> = GucSetting::<i32>::new(64);

/// Backing storage for `pg_flatbuffers.strict`. Default `true`
/// (`on`) matches design §10's stated default and the current
/// always-ERROR-on-verifier-failure behaviour of
/// [`crate::functions`].
static STRICT: GucSetting<bool> = GucSetting::<bool>::new(true);

// -- Upper-bound sanity caps -----------------------------------------------
//
// Hard caps documented above. `max_tables`'s cap is `i32::MAX` because
// the only way to exceed a billion verifier-visited tables is a
// pathological DAG, which the verifier is built to reject anyway —
// the cap exists to keep the GUC subsystem from rejecting reasonable
// future raises, not to be a meaningful safety boundary on its own.

const MAX_DEPTH_CAP: i32 = 1024;
const MAX_APPARENT_SIZE_MB_CAP: i32 = 16 * 1024; // 16 GiB

// -- Registration ----------------------------------------------------------

/// Register all three GUCs with Postgres. Called once per backend
/// from [`crate::_PG_init`].
///
/// Idempotent at the pgrx layer: pgrx wraps
/// `DefineCustomIntVariable`, which Postgres itself treats as
/// idempotent (re-registration with identical parameters is a no-op).
pub fn init() {
    GucRegistry::define_int_guc(
        c"pg_flatbuffers.max_depth",
        c"Maximum FlatBuffers nested-table depth the verifier accepts.",
        c"Verifier DoS bound (design §10). Bypass would permit a \
          stack-exhaustion attack against the verifier itself.",
        &MAX_DEPTH,
        1,
        MAX_DEPTH_CAP,
        GucContext::Suset,
        GucFlags::default(),
    );
    GucRegistry::define_int_guc(
        c"pg_flatbuffers.max_tables",
        c"Maximum number of tables the verifier visits per payload.",
        c"Verifier DoS bound (design §10). Bypass would permit a \
          quadratic-time attack on payloads that share the same \
          sub-table from many parents.",
        &MAX_TABLES,
        1,
        i32::MAX,
        GucContext::Suset,
        GucFlags::default(),
    );
    GucRegistry::define_int_guc(
        c"pg_flatbuffers.max_apparent_size_mb",
        c"Maximum DAG-expanded apparent payload size, in MiB.",
        c"Verifier DoS bound (design §10). Also applied to the \
          from_json build path. Bypass would permit unbounded memory \
          amplification on to_json paths that materialise the expansion.",
        &MAX_APPARENT_SIZE_MB,
        1,
        MAX_APPARENT_SIZE_MB_CAP,
        GucContext::Suset,
        GucFlags::default(),
    );
    GucRegistry::define_bool_guc(
        c"pg_flatbuffers.strict",
        c"When on (default), a verifier failure raises ERROR. When off, structural failures return NULL instead, but bound exceedances still ERROR.",
        c"Per-session knob (design §10). USERSET-safe because it cannot \
          weaken the verifier or relax a bound: \
          VerifyError::is_bound_exceedance() is still consulted before \
          substituting NULL, so the SUSET-protected max_* bounds remain \
          effective even when this GUC is off.",
        &STRICT,
        GucContext::Userset,
        GucFlags::default(),
    );
}

// -- Read path -------------------------------------------------------------

/// Materialise a [`Bounds`] from the current GUC values. Called
/// once per public SQL entry point in [`crate::functions`] so a
/// `SET pg_flatbuffers.max_*` takes effect on the very next call
/// in the same session.
///
/// Each cast is loss-free: the registered `min_value` (1) prevents
/// negative values, and every `max_value` fits in `i32` by
/// construction so `as usize` is a widening conversion.
pub fn current_bounds() -> Bounds {
    Bounds {
        max_depth: MAX_DEPTH.get() as usize,
        max_tables: MAX_TABLES.get() as usize,
        max_apparent_size: (MAX_APPARENT_SIZE_MB.get() as usize) * 1024 * 1024,
    }
}

/// Current value of `pg_flatbuffers.strict`. `true` is the design §10
/// default and means a verifier failure raises ERROR; `false` means
/// a *structural* failure substitutes a NULL leaf / empty result and
/// the scan continues. Bound exceedances are not affected by this
/// GUC — the call site in [`crate::functions`] consults
/// [`crate::verify::VerifyError::is_bound_exceedance`] separately
/// and ERRORs on those regardless of `strict`.
pub fn current_strict() -> bool {
    STRICT.get()
}

// -- Tests -----------------------------------------------------------------

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
mod tests {
    use super::*;
    use pgrx::prelude::*;

    /// At startup, `current_bounds()` must return the documented
    /// defaults from §10 byte-for-byte. Pins the `Bounds::default`
    /// invariant from a backend with the GUCs registered.
    #[pg_test]
    fn pg_guc_defaults_match_design_section_10() {
        let b = current_bounds();
        assert_eq!(b.max_depth, 64);
        assert_eq!(b.max_tables, 1_000_000);
        assert_eq!(b.max_apparent_size, 64 * 1024 * 1024);
    }

    /// `SET` of every SUSET GUC must take effect on the very next
    /// `current_bounds()` call within the same session. Tests run as
    /// superuser, so the SUSET context is satisfied.
    #[pg_test]
    fn pg_guc_set_takes_effect_in_same_session() {
        Spi::run("SET pg_flatbuffers.max_depth = 32").expect("SPI: SET max_depth");
        Spi::run("SET pg_flatbuffers.max_tables = 5000").expect("SPI: SET max_tables");
        Spi::run("SET pg_flatbuffers.max_apparent_size_mb = 8")
            .expect("SPI: SET max_apparent_size_mb");

        let b = current_bounds();
        assert_eq!(b.max_depth, 32);
        assert_eq!(b.max_tables, 5_000);
        assert_eq!(b.max_apparent_size, 8 * 1024 * 1024);
    }

    /// `SHOW` round-trips the registered GUCs as plain integers, so
    /// `psql`-style introspection works without surprise. Pins the
    /// boot-default rendering.
    #[pg_test]
    fn pg_guc_show_returns_defaults_as_text() {
        let v = Spi::get_one::<String>("SHOW pg_flatbuffers.max_depth")
            .expect("SPI failure")
            .expect("NULL from SHOW");
        assert_eq!(v, "64");

        let v = Spi::get_one::<String>("SHOW pg_flatbuffers.max_tables")
            .expect("SPI failure")
            .expect("NULL from SHOW");
        assert_eq!(v, "1000000");

        let v = Spi::get_one::<String>("SHOW pg_flatbuffers.max_apparent_size_mb")
            .expect("SPI failure")
            .expect("NULL from SHOW");
        assert_eq!(v, "64");
    }

    /// Below the registered minimum (1) is a `SET`-time ERROR. Pins
    /// the contract that the GUC subsystem — not [`current_bounds`] —
    /// guards against zero / negative values, so the cast-to-usize
    /// stays loss-free without a runtime check.
    #[pg_test(
        error = "0 is outside the valid range for parameter \"pg_flatbuffers.max_depth\" (1 .. 1024)"
    )]
    fn pg_guc_below_min_errors() {
        Spi::run("SET pg_flatbuffers.max_depth = 0").expect("SPI failure");
    }

    /// Above the registered maximum is a `SET`-time ERROR. Pins the
    /// hard cap that prevents an attempt to widen the depth bound
    /// past its documented sanity limit.
    #[pg_test(
        error = "99999 is outside the valid range for parameter \"pg_flatbuffers.max_depth\" (1 .. 1024)"
    )]
    fn pg_guc_above_max_errors() {
        Spi::run("SET pg_flatbuffers.max_depth = 99999").expect("SPI failure");
    }

    // -- pg_flatbuffers.strict --

    /// Default for `pg_flatbuffers.strict` is `on` (matches design
    /// §10 and the historical always-ERROR-on-verifier-failure
    /// behaviour of [`crate::functions`]). Boot value is read
    /// through [`current_strict`] to also cover the accessor.
    #[pg_test]
    fn pg_guc_strict_default_is_on() {
        assert!(current_strict());
        let v = Spi::get_one::<String>("SHOW pg_flatbuffers.strict")
            .expect("SPI failure")
            .expect("NULL from SHOW");
        assert_eq!(v, "on");
    }

    /// `USERSET` means an unprivileged session can `SET` it. Tests
    /// run as superuser, but the GUC's `GucContext::Userset` is
    /// equally accessible to superuser, so the assertion still
    /// holds. Cross-role enforcement (a non-superuser can `SET` a
    /// `USERSET` but not a `SUSET`) is a regression-test concern
    /// for §13 once we have a role-aware test harness.
    #[pg_test]
    fn pg_guc_strict_set_takes_effect_in_same_session() {
        Spi::run("SET pg_flatbuffers.strict = off").expect("SPI: SET strict");
        assert!(!current_strict());
        Spi::run("SET pg_flatbuffers.strict = on").expect("SPI: SET strict back on");
        assert!(current_strict());
    }
}
