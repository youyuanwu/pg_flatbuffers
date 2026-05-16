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
//! Plus three `USERSET` bools:
//!
//! | GUC | Default | Maps to |
//! | --- | --- | --- |
//! | `pg_flatbuffers.strict` | `on` | [`current_strict`] — consumed by [`crate::functions`] to decide whether to ERROR or substitute `NULL` on a structural verifier failure (§10 "strict does not relax bounds"). |
//! | `pg_flatbuffers.fill_scalar_defaults` | `on` | [`current_fill_scalar_defaults`] — consumed by [`crate::query::execute_with_options`] to decide whether an absent scalar table field reads as its schema default (`on`, matches the FlatBuffers reader API) or as SQL `NULL` (`off`, presence-aware) (§4.3, §10). |
//! | `pg_flatbuffers.key_lookup_strict` | `on` | [`current_key_lookup_strict`] — consumed by [`crate::query::execute_with_options`] to pick between binary search over `(key)`-sorted vectors (`on`, matches the FlatBuffers reader API's `LookupByKey`) or a linear scan that tolerates unsorted vectors (`off`, correct but O(n)) (§7.2 step 4, §10). |
//! | `pg_flatbuffers.from_json_unknown` | `on` | [`current_from_json_unknown_error`] — consumed by [`crate::from_json`]: `on` (default) raises ERROR on an unknown JSON key for a target table; `off` silently drops it for forward-compat workflows (§8). |
//!
//! [`current_bounds`] materialises a [`Bounds`] from the current GUC
//! values for every call to [`crate::query::execute_with_options`] /
//! [`crate::verify::verify`] in [`crate::functions`]. The pure-Rust
//! tests inside [`crate::query::executor`] and [`crate::verify`]
//! continue to use [`Bounds::default`] directly — they have no
//! Postgres backend to register GUCs against.
//!
//! ## Why `SUSET` for the bounds, `USERSET` for `strict` / `fill_scalar_defaults` / `key_lookup_strict`?
//!
//! Per §10 "GUC governance is part of the safety boundary: a bound
//! that any session can raise is not a bound." The three bounds gate
//! verifier DoS resistance, so they require superuser to change. The
//! three `USERSET` bools, by contrast, only affect the *calling
//! session's* own results:
//!
//! - `strict` turns structural failures into `NULL` so a scan over a
//!   mixed-cleanliness column doesn't abort mid-statement. It cannot
//!   weaken the verifier or relax a bound exceedance —
//!   [`crate::verify::VerifyError::is_bound_exceedance`] is still
//!   consulted before substituting `NULL`.
//! - `fill_scalar_defaults` selects between two equally-valid
//!   interpretations of an absent scalar (schema default vs. SQL
//!   `NULL`) — both are read-side-only and neither weakens any
//!   safety invariant.
//! - `key_lookup_strict` selects between binary search (assumes the
//!   vector is `(key)`-sorted per the FlatBuffers spec) and linear
//!   scan (correct on any vector but O(n)). The verifier does not
//!   yet check sortedness, so `off` is the safe escape hatch for
//!   payloads from a writer that violated the contract; `on` is the
//!   performance default that matches every standard FlatBuffers
//!   `LookupByKey` accessor. Read-side only — cannot weaken any
//!   safety invariant; the worst case under `on` against an
//!   unsorted vector is a silent miss, never a buffer over-read.
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

/// Backing storage for `pg_flatbuffers.fill_scalar_defaults`.
/// Default `true` (`on`) matches the FlatBuffers reader API (and
/// the pre-GUC executor behaviour): an absent scalar table field
/// reads back as its schema default. `false` (`off`) makes absent
/// scalars surface as SQL `NULL` instead, so callers can
/// distinguish "writer set field to 0" from "writer never set
/// field" (§4.3, §10).
static FILL_SCALAR_DEFAULTS: GucSetting<bool> = GucSetting::<bool>::new(true);

/// Backing storage for `pg_flatbuffers.key_lookup_strict`. Default
/// `true` (`on`) means `field[key]` lookups bisect a
/// `(key)`-annotated vector under the FlatBuffers contract that the
/// vector is key-sorted (matches the upstream `LookupByKey`
/// accessor). `false` (`off`) falls back to a linear scan that is
/// correct on any vector but O(n). Read-side only; cannot weaken
/// any safety invariant (§7.2 step 4, §10).
static KEY_LOOKUP_STRICT: GucSetting<bool> = GucSetting::<bool>::new(true);

/// Backing storage for `pg_flatbuffers.from_json_unknown`.
/// Default `true` (`on`) means an unknown JSON key for a target
/// table raises `ERROR`; `false` (`off`) silently drops it for
/// forward-compat workflows where producers add fields ahead of
/// consumers (§8). Write-side only; cannot relax any verifier
/// bound (the resulting buffer is still verified before any
/// downstream use).
static FROM_JSON_UNKNOWN_ERROR: GucSetting<bool> = GucSetting::<bool>::new(true);

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
    GucRegistry::define_bool_guc(
        c"pg_flatbuffers.fill_scalar_defaults",
        c"When on (default), an absent scalar table field reads back as its schema default. When off, it surfaces as SQL NULL instead.",
        c"Per-session knob (design §4.3, §10). USERSET-safe: read-side \
          only, affects neither the verifier nor any DoS bound. Selects \
          between two equally-valid interpretations of an absent scalar \
          — schema default (matches the FlatBuffers reader API) vs. \
          SQL NULL (presence-aware).",
        &FILL_SCALAR_DEFAULTS,
        GucContext::Userset,
        GucFlags::default(),
    );
    GucRegistry::define_bool_guc(
        c"pg_flatbuffers.key_lookup_strict",
        c"When on (default), field[key] lookups bisect the vector under the FlatBuffers (key)-sorted contract. When off, fall back to a linear scan.",
        c"Per-session knob (design §7.2 step 4, §10). USERSET-safe: \
          read-side only, neither weakens the verifier nor relaxes any \
          DoS bound. The verifier does not yet check vector \
          sortedness, so the on-mode worst case against a writer that \
          violated the (key)-sorted contract is a silent miss, never a \
          buffer over-read. Off-mode trades O(log n) for O(n) and is \
          correct on any vector.",
        &KEY_LOOKUP_STRICT,
        GucContext::Userset,
        GucFlags::default(),
    );
    GucRegistry::define_bool_guc(
        c"pg_flatbuffers.from_json_unknown",
        c"When on (default), an unknown JSON key for a target table raises ERROR. When off, unknown keys are silently dropped.",
        c"Per-session knob (design §8). USERSET-safe: write-side only, \
          neither weakens the verifier nor relaxes any DoS bound. The \
          off-mode 'ignore' semantic is useful for forward-compat \
          workflows where producers add fields ahead of consumers; \
          the resulting buffer is still verified before any downstream \
          use.",
        &FROM_JSON_UNKNOWN_ERROR,
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

/// Current value of `pg_flatbuffers.fill_scalar_defaults`. `true`
/// (the default) means an absent scalar table field reads back as
/// its schema default (FlatBuffers reader API parity, §4.3); `false`
/// makes it surface as SQL `NULL` so callers can distinguish
/// presence from default. Consumed by [`crate::functions`] when
/// building the [`crate::query::ExecuteOptions`] for each call.
pub fn current_fill_scalar_defaults() -> bool {
    FILL_SCALAR_DEFAULTS.get()
}

/// Current value of `pg_flatbuffers.key_lookup_strict`. `true` (the
/// default) means `field[key]` lookups bisect the vector assuming
/// the FlatBuffers `(key)`-sorted contract. `false` falls back to a
/// linear scan. Consumed by [`crate::functions`] when building the
/// [`crate::query::ExecuteOptions`] for each call.
pub fn current_key_lookup_strict() -> bool {
    KEY_LOOKUP_STRICT.get()
}

/// Current value of `pg_flatbuffers.from_json_unknown`. `true` (the
/// default) means [`crate::from_json::json_to_buf`] raises
/// [`crate::from_json::FromJsonError::UnknownField`] on an unknown
/// JSON key; `false` silently drops the key. Consumed by
/// [`crate::functions::flatbuffers_from_json`] when building the
/// per-call [`crate::from_json::BuildOptions`].
pub fn current_from_json_unknown_error() -> bool {
    FROM_JSON_UNKNOWN_ERROR.get()
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

    // -- pg_flatbuffers.fill_scalar_defaults --

    /// Default for `pg_flatbuffers.fill_scalar_defaults` is `on`
    /// (matches the FlatBuffers reader API and the pre-GUC executor
    /// behaviour). Boot value is read through
    /// [`current_fill_scalar_defaults`] to also cover the accessor.
    #[pg_test]
    fn pg_guc_fill_scalar_defaults_default_is_on() {
        assert!(current_fill_scalar_defaults());
        let v = Spi::get_one::<String>("SHOW pg_flatbuffers.fill_scalar_defaults")
            .expect("SPI failure")
            .expect("NULL from SHOW");
        assert_eq!(v, "on");
    }

    /// `USERSET` toggling round-trips: `off` flips the accessor, `on`
    /// restores it. Pins the GUC subsystem actually mutating the
    /// `GucSetting<bool>` backing storage on `SET`.
    #[pg_test]
    fn pg_guc_fill_scalar_defaults_set_takes_effect_in_same_session() {
        Spi::run("SET pg_flatbuffers.fill_scalar_defaults = off")
            .expect("SPI: SET fill_scalar_defaults");
        assert!(!current_fill_scalar_defaults());
        Spi::run("SET pg_flatbuffers.fill_scalar_defaults = on")
            .expect("SPI: SET fill_scalar_defaults back on");
        assert!(current_fill_scalar_defaults());
    }

    // -- pg_flatbuffers.key_lookup_strict --

    /// Default for `pg_flatbuffers.key_lookup_strict` is `on`
    /// (binary search; matches the FlatBuffers reader API's
    /// `LookupByKey`). Boot value is read through
    /// [`current_key_lookup_strict`] to also cover the accessor.
    #[pg_test]
    fn pg_guc_key_lookup_strict_default_is_on() {
        assert!(current_key_lookup_strict());
        let v = Spi::get_one::<String>("SHOW pg_flatbuffers.key_lookup_strict")
            .expect("SPI failure")
            .expect("NULL from SHOW");
        assert_eq!(v, "on");
    }

    /// `USERSET` toggling round-trips.
    #[pg_test]
    fn pg_guc_key_lookup_strict_set_takes_effect_in_same_session() {
        Spi::run("SET pg_flatbuffers.key_lookup_strict = off").expect("SPI: SET key_lookup_strict");
        assert!(!current_key_lookup_strict());
        Spi::run("SET pg_flatbuffers.key_lookup_strict = on")
            .expect("SPI: SET key_lookup_strict back on");
        assert!(current_key_lookup_strict());
    }

    // -- pg_flatbuffers.from_json_unknown --

    /// Default for `pg_flatbuffers.from_json_unknown` is `on`
    /// (matches design §8: unknown JSON keys raise ERROR by
    /// default; the off mode is the opt-in forward-compat escape
    /// hatch).
    #[pg_test]
    fn pg_guc_from_json_unknown_default_is_on() {
        assert!(current_from_json_unknown_error());
        let v = Spi::get_one::<String>("SHOW pg_flatbuffers.from_json_unknown")
            .expect("SPI failure")
            .expect("NULL from SHOW");
        assert_eq!(v, "on");
    }

    /// `USERSET` toggling round-trips.
    #[pg_test]
    fn pg_guc_from_json_unknown_set_takes_effect_in_same_session() {
        Spi::run("SET pg_flatbuffers.from_json_unknown = off").expect("SPI: SET from_json_unknown");
        assert!(!current_from_json_unknown_error());
        Spi::run("SET pg_flatbuffers.from_json_unknown = on")
            .expect("SPI: SET from_json_unknown back on");
        assert!(current_from_json_unknown_error());
    }
}
