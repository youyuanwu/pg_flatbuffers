//! Postgres-compatible float stringification.
//!
//! Floats are routed through Postgres's own `float4out` / `float8out`
//! via [`pg_sys::DirectFunctionCall1Coll`], so query leaves match
//! `SELECT col::text` from a `real` / `double precision` column
//! byte-for-byte (design §7.2: *"we will reuse Postgres's
//! `float4out`/`float8out` via pgrx's built-in conversions, not
//! Rust's default `Display`"*). Notable consequences vs. Rust's
//! `f32::Display` / `f64::Display`:
//!
//! - `Infinity` / `-Infinity` / `NaN` (PG) instead of `inf` / `-inf`
//!   / `NaN` (Rust).
//! - Scientific notation switch for very large / very small
//!   magnitudes (PG) vs. always-fixed (Rust).
//! - Honors the per-session `extra_float_digits` GUC (default `1`
//!   since PG12 = shortest round-trip Ryu).
//!
//! Integer and bool scalars are left on Rust's `Display`: their
//! output is pure ASCII-decimal and matches Postgres's `int*out` /
//! `boolout` formatters byte-for-byte (except `boolout` emits
//! `t`/`f`, which we deliberately *don't* mirror because every other
//! leaf path here renders bool as `0`/`1` for consistency with
//! `flatbuffers_reflection::get_any_field_string`).
//!
//! # Two compile-time variants
//!
//! The crate has to build cleanly into two very different kinds of
//! binary:
//!
//! 1. **cdylib** — both the production extension and the cdylib
//!    that `cargo pgrx test` loads into a real Postgres backend.
//!    Cdylib targets are *not* compiled with `cfg(test)`, allow
//!    undefined symbols at link time, and run inside a live
//!    Postgres process where `pg_sys::float4out` is resolvable.
//! 2. **Test binaries** — both `cargo test --lib` (driven by
//!    `just unit`) and the pgrx-tests shim that `cargo pgrx test`
//!    builds alongside the cdylib. Both are normal Rust
//!    executables (`cfg(test)`) that the linker requires to be
//!    fully resolved, and neither has a Postgres backend behind
//!    them: the pgrx-tests shim drives the cdylib via SPI rather
//!    than calling lib code directly, so it never needs to run
//!    `format_float4` itself.
//!
//! `cfg(test)` cleanly separates the two: the real PG-calling
//! implementation compiles into the cdylib (`not(test)`), and a
//! `Display`-based fallback compiles into any test binary
//! (`test`). The fallback matches PG output byte-for-byte on the
//! finite normal values executor unit tests assert on. Tests that
//! need PG-specific formatting (`Infinity`, large-magnitude
//! scientific notation, …) must be `#[pg_test]` so they execute
//! inside the cdylib's real formatter — see
//! `pg_query_float_formatting_matches_postgres` in
//! `functions::tests::query`.

#[cfg(not(test))]
mod imp {
    use pgrx::pg_sys;
    use std::ffi::{CStr, c_char};

    // Bare C declarations for the two output functions. We have to
    // re-declare these (rather than reaching for `pg_sys::float4out`)
    // because pgrx-pg-sys wraps every extern fn with a
    // `#[pg_guard]`-generated Rust shim — its type is a Rust-ABI
    // `unsafe fn`, not the `unsafe extern "C-unwind" fn` item that
    // `pg_sys::PGFunction` requires for the fmgr fn-pointer slot.
    // `DirectFunctionCall1Coll` and `pfree` themselves we *do* call
    // through the pgrx wrappers below — the wrapper signature is
    // irrelevant there because we're calling them directly, not
    // storing their address.
    unsafe extern "C-unwind" {
        unsafe fn float4out(fcinfo: pg_sys::FunctionCallInfo) -> pg_sys::Datum;
        unsafe fn float8out(fcinfo: pg_sys::FunctionCallInfo) -> pg_sys::Datum;
    }

    /// Format an `f32` exactly as Postgres's `float4out` would.
    pub(in crate::query::executor) fn format_float4(v: f32) -> String {
        // SAFETY: `Float4GetDatum` packs the IEEE-754 bit pattern
        // of an `f32` into the low 32 bits of a pass-by-value Datum.
        // `float4out` consumes that Datum and returns a palloc'd
        // NUL-terminated cstring; we copy into an owned `String`
        // and `pfree` the original.
        unsafe {
            let arg = pg_sys::Datum::from(u64::from(v.to_bits()));
            let out = pg_sys::DirectFunctionCall1Coll(Some(float4out), pg_sys::InvalidOid, arg);
            cstring_datum_into_string(out)
        }
    }

    /// Format an `f64` exactly as Postgres's `float8out` would.
    pub(in crate::query::executor) fn format_float8(v: f64) -> String {
        // SAFETY: same shape as [`format_float4`]. USE_FLOAT8_BYVAL
        // is the default on every 64-bit platform pg_flatbuffers
        // supports, so `Float8GetDatum` is the IEEE-754 bit pattern
        // packed into a pass-by-value Datum.
        unsafe {
            let arg = pg_sys::Datum::from(v.to_bits());
            let out = pg_sys::DirectFunctionCall1Coll(Some(float8out), pg_sys::InvalidOid, arg);
            cstring_datum_into_string(out)
        }
    }

    /// Copy a `cstring` Datum returned by a Postgres output function
    /// into an owned `String` and `pfree` the original allocation.
    ///
    /// # Safety
    ///
    /// `datum` must hold a non-NULL pointer to a NUL-terminated
    /// palloc'd `cstring`, as produced by every Postgres `*out`
    /// function.
    unsafe fn cstring_datum_into_string(datum: pg_sys::Datum) -> String {
        let cstr_ptr = datum.cast_mut_ptr::<c_char>();
        // SAFETY: per caller contract — non-NULL, NUL-terminated.
        let s = unsafe { CStr::from_ptr(cstr_ptr) }
            .to_string_lossy()
            .into_owned();
        // SAFETY: same allocation; transfers ownership back to PG.
        unsafe { pg_sys::pfree(cstr_ptr.cast()) };
        s
    }
}

pub(super) use imp::{format_float4, format_float8};

#[cfg(test)]
mod imp {
    /// Pure-Rust fallback for any test binary (`cargo test --lib` or
    /// the pgrx-tests shim). Matches Postgres's `float4out`
    /// byte-for-byte on the finite normal values executor unit
    /// tests assert on (Rust's Ryu shortest-decimal agrees with
    /// Postgres for that range). The pgrx-tests shim never reaches
    /// this code at runtime — `#[pg_test]` bodies execute inside
    /// the cdylib via SPI, which uses the `not(test)` impl.
    pub(in crate::query::executor) fn format_float4(v: f32) -> String {
        v.to_string()
    }

    /// Pure-Rust fallback for any test binary. See
    /// [`format_float4`].
    pub(in crate::query::executor) fn format_float8(v: f64) -> String {
        v.to_string()
    }
}
