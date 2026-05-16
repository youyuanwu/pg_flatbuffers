//! Pure-Rust unit tests for the executor (no `pgrx`; pgrx-backed SQL
//! smoke tests live in `crate::functions::tests`).
//!
//! Helpers (schema/buffer builders, `run_*` wrappers) are grouped by
//! the section they exercise — one file per production-side area
//! (`walk`/`vector`/`map_key`/`struct_`/`union`/`leaf`/`util`):
//!
//! * [`comparator`] — [`super::map_key::compare_actual_to_compiled`] unit
//!   tests (no fixture).
//! * [`table`] — `Order`/`Customer` schema: scalar leaves, nested table
//!   descent, nullability, error variants.
//! * [`vector`] — `Bag` schema with vectors of strings/scalars/tables:
//!   [`Step::Index`], [`Step::All`], [`Step::MapKey`] (with the binary
//!   search ↔ linear scan switch), [`Step::MapKeys`].
//! * [`struct_`] — `Point` schema: inline struct descent.
//! * [`union`] — `Msg` schema: union discriminator dispatch and the
//!   `|type` leaf.
//! * [`vector_of_struct`] — `Bag` (`Vec3`) schema: vector of inline
//!   structs.
//! * [`array`] — `Holder` schema: fixed-size arrays inside structs.
//!
//! Common imports and helpers are re-exported by this `mod.rs` so each
//! area file can `use super::*;` and pull in the full preamble.

pub(super) use super::map_key::{CompiledKey, compare_actual_to_compiled};
pub(super) use super::{ExecuteError, ExecuteOptions, execute_with_options};
pub(super) use crate::query::ast::{MapKey, Query, Step};
pub(super) use crate::query::parse;
pub(super) use crate::verify::Bounds;
pub(super) use flatbuffers::FlatBufferBuilder;
pub(super) use flatbuffers_reflection::reflection::{
    BaseType, Enum, EnumArgs, EnumVal, EnumValArgs, Field as RField, FieldArgs, Object as RObject,
    ObjectArgs, Schema as RSchema, SchemaArgs, Type, TypeArgs, root_as_schema,
};
pub(super) use std::cmp::Ordering;

mod array;
mod common;
mod comparator;
mod struct_;
mod table;
mod union;
mod vector;
mod vector_of_struct;
