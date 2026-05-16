//! Test fixtures shared across more than one area file.
//!
//! `TestVec3` is referenced by [`super::struct_`] (where it is the
//! struct under test), [`super::vector_of_struct`] (where it
//! populates a `[Vec3]` element vector) and [`super::array`] (where
//! it is the element type of a fixed-size array). Centralising it
//! here keeps the cross-area dependency explicit.

/// `Vec3` mirror used by sibling test modules' `Push` calls to write
/// the struct inline into a parent table or vector. `repr(C, packed)`
/// matches the FlatBuffers wire layout for structs (no compiler
/// padding).
#[repr(C, packed)]
#[derive(Clone, Copy)]
pub(super) struct TestVec3 {
    pub(super) x: f32,
    pub(super) y: f32,
    pub(super) z: f32,
}

// SAFETY: `TestVec3` is `repr(C, packed)` so its in-memory bytes
// match the on-wire little-endian layout on every supported
// host (x86_64, aarch64). `flatbuffers::Push::size()` defaults
// to `size_of::<Self::Output>()` (= 12) and `alignment()` to
// `align_of::<Self::Output>()` (= 4), which matches the
// `bytesize`/`minalign` we declare in each schema that uses it.
impl flatbuffers::Push for TestVec3 {
    type Output = TestVec3;
    unsafe fn push(&self, dst: &mut [u8], _written_len: usize) {
        // SAFETY: see the impl-level comment above.
        let src = unsafe {
            std::slice::from_raw_parts(
                self as *const Self as *const u8,
                std::mem::size_of::<Self>(),
            )
        };
        dst[..src.len()].copy_from_slice(src);
    }
}
