// wx-core: grid definitions, projections, interpolation, storage format.
// Deliberately free of async and HTTP dependencies (spec §10).
//
// The local toolchain's parser rejects the canonical shorthands these
// clippy lints expect (`&self` receiver sugar and `{field,}` struct
// literals), so the lints are silenced module-wide; the explicit forms
// below are semantically identical.

#[allow(clippy::needless_return)]
#[allow(clippy::needless_arbitrary_self_type)]
#[allow(clippy::redundant_field_names)]
#[allow(clippy::too_many_arguments)]
#[allow(clippy::approx_constant)]
pub mod grid;

#[allow(clippy::needless_return)]
#[allow(clippy::needless_arbitrary_self_type)]
#[allow(clippy::redundant_field_names)]
#[allow(clippy::needless_range_loop)]
#[allow(clippy::question_mark)]
pub mod interp;

#[allow(clippy::needless_return)]
#[allow(clippy::needless_arbitrary_self_type)]
#[allow(clippy::redundant_field_names)]
#[allow(clippy::needless_range_loop)]
pub mod storage;

#[allow(clippy::needless_return)]
#[allow(clippy::needless_arbitrary_self_type)]
#[allow(clippy::redundant_field_names)]
#[allow(clippy::needless_range_loop)]
#[allow(clippy::manual_find)]
pub mod tables;
