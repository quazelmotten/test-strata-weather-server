//! wx-point HTTP server library (spec §7 + §5 hot-path rules, milestone 4).
//!
//! Layout: `stats` (metrics counters + Prometheus rendering), `timeparse`
//! (ISO-8601Z <-> epoch seconds), `queryapi` (query-string parsing, run
//! selection over the arc-swap snapshot, response JSON rendering), `httpapi`
//! (axum router + handlers + binary startup wiring helpers).
//!
//! The local toolchain's parser rejects canonical shorthands (`&self` sugar,
//! `{field,}` literals), so several style lints are silenced module-wide, same
//! policy as wx-core/wx-ingest.

#[allow(clippy::needless_return)]
#[allow(clippy::redundant_field_names)]
#[allow(clippy::needless_arbitrary_self_type)]
#[allow(clippy::manual_div_ceil)]
#[allow(clippy::unnecessary_cast)]
#[allow(clippy::manual_range_contains)]
#[allow(clippy::assign_op_pattern)]
#[allow(clippy::double_parens)]
#[allow(clippy::explicit_counter_loop)]
#[allow(clippy::len_zero)]
#[allow(clippy::needless_range_loop)]
#[allow(clippy::needless_borrow)]
#[allow(clippy::needless_borrows_for_generic_args)]
#[allow(clippy::ptr_arg)]
#[allow(clippy::manual_find)]
#[allow(clippy::single_match)]
#[allow(clippy::match_bool)]
#[allow(clippy::match_single_binding)]
#[allow(clippy::manual_swap)]
#[allow(clippy::cmp_owned)]
#[allow(clippy::too_many_arguments)]
#[allow(clippy::collapsible_match)]
#[allow(clippy::if_same_then_else)]
#[allow(clippy::unnecessary_to_owned)]
#[allow(clippy::manual_map)]
pub mod stats;

#[allow(clippy::needless_return)]
#[allow(clippy::redundant_field_names)]
#[allow(clippy::needless_arbitrary_self_type)]
#[allow(clippy::manual_div_ceil)]
#[allow(clippy::unnecessary_cast)]
#[allow(clippy::manual_range_contains)]
#[allow(clippy::assign_op_pattern)]
#[allow(clippy::double_parens)]
#[allow(clippy::explicit_counter_loop)]
#[allow(clippy::len_zero)]
#[allow(clippy::needless_range_loop)]
#[allow(clippy::needless_borrow)]
#[allow(clippy::needless_borrows_for_generic_args)]
#[allow(clippy::ptr_arg)]
#[allow(clippy::manual_find)]
#[allow(clippy::single_match)]
#[allow(clippy::match_bool)]
#[allow(clippy::match_single_binding)]
#[allow(clippy::manual_swap)]
#[allow(clippy::cmp_owned)]
#[allow(clippy::too_many_arguments)]
#[allow(clippy::collapsible_match)]
#[allow(clippy::if_same_then_else)]
#[allow(clippy::unnecessary_to_owned)]
#[allow(clippy::manual_map)]
pub mod timeparse;

#[allow(clippy::needless_return)]
#[allow(clippy::redundant_field_names)]
#[allow(clippy::needless_arbitrary_self_type)]
#[allow(clippy::manual_div_ceil)]
#[allow(clippy::unnecessary_cast)]
#[allow(clippy::manual_range_contains)]
#[allow(clippy::assign_op_pattern)]
#[allow(clippy::double_parens)]
#[allow(clippy::explicit_counter_loop)]
#[allow(clippy::len_zero)]
#[allow(clippy::needless_range_loop)]
#[allow(clippy::needless_borrow)]
#[allow(clippy::needless_borrows_for_generic_args)]
#[allow(clippy::ptr_arg)]
#[allow(clippy::manual_find)]
#[allow(clippy::single_match)]
#[allow(clippy::match_bool)]
#[allow(clippy::match_single_binding)]
#[allow(clippy::manual_swap)]
#[allow(clippy::cmp_owned)]
#[allow(clippy::too_many_arguments)]
#[allow(clippy::collapsible_match)]
#[allow(clippy::if_same_then_else)]
#[allow(clippy::unnecessary_to_owned)]
#[allow(clippy::manual_map)]
pub mod queryapi;

#[allow(clippy::needless_return)]
#[allow(clippy::redundant_field_names)]
#[allow(clippy::needless_arbitrary_self_type)]
#[allow(clippy::manual_div_ceil)]
#[allow(clippy::unnecessary_cast)]
#[allow(clippy::manual_range_contains)]
#[allow(clippy::assign_op_pattern)]
#[allow(clippy::double_parens)]
#[allow(clippy::explicit_counter_loop)]
#[allow(clippy::len_zero)]
#[allow(clippy::needless_range_loop)]
#[allow(clippy::needless_borrow)]
#[allow(clippy::needless_borrows_for_generic_args)]
#[allow(clippy::ptr_arg)]
#[allow(clippy::manual_find)]
#[allow(clippy::single_match)]
#[allow(clippy::match_bool)]
#[allow(clippy::match_single_binding)]
#[allow(clippy::manual_swap)]
#[allow(clippy::cmp_owned)]
#[allow(clippy::too_many_arguments)]
#[allow(clippy::collapsible_match)]
#[allow(clippy::if_same_then_else)]
#[allow(clippy::unnecessary_to_owned)]
#[allow(clippy::manual_map)]
pub mod httpapi;

pub use stats::QueryStats;
pub use stats::new_stats;
pub use stats::mono_ns;
pub use stats::realtime_ns;
pub use timeparse::parse_iso_z;
pub use timeparse::format_iso_z;
pub use queryapi::ParsedArgs;
pub use queryapi::parse_query;
pub use queryapi::answer_param;
pub use queryapi::lookup_entries;
pub use queryapi::new_entries;
pub use queryapi::PointEntries;
pub use queryapi::param_at;
pub use queryapi::FailInfo;
pub use queryapi::MAX_INLINE_PARAMS;
pub use queryapi::FailTok;
pub use queryapi::render_point_json;
pub use queryapi::render_error_json;
pub use httpapi::ServerState;
pub use httpapi::build_router;
pub use httpapi::make_response;
