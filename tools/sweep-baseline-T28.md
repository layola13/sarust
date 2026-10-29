# sarust T28 baseline sweep

- date (UTC): 2026-10-29T02:21:03Z
- sarust HEAD: 8935c4e8df651b14b343426977f02cf81ae463fa
- sa: sa 0.1.8
- rustc: rustc 1.101.0-nightly (d080e7dff 2026-09-27)
- sars: /content/sa_all/sarust/rsc/driver/target/sars
- mir2sa: /content/sa_all/sarust/rsc/target/debug/mir2sa
- corpus: /content/sa_all/sci/demos/rosetta (main.rs only)
- pipeline: sars --edition 2021 --crate-type bin -> mir2sa lower -> sa check --no-incremental

| class | count |
|-------|-------|
| GREEN | 139 |
| CHECK_ONLY | 117 |
| TRAP | 63 |
| SARS_FAIL | 13 |
| TOTAL | 332 |

- loud total (sum of mir2sa UNSUPPORTED over lowered files): 516

## TRAP files

- 03_if_else/main.rs (check-fail: error[PhiStateConflict]: incoming control-flow states do not agree)
- 04_loop/main.rs (check-fail: error[PhiStateConflict]: incoming control-flow states do not agree)
- 05_struct/main.rs (check-fail: error[UseAfterMove]: moved value is no longer usable)
- 06_enum_and_match/main.rs (check-fail: error[UseAfterMove]: moved value is no longer usable)
- 09_async_await/main.rs (check-fail: error[UseAfterMove]: moved value is no longer usable)
- 102_raii_guard/main.rs (check-fail: error[BorrowConflict]: borrow rules reject this access)
- 103_labeled_break/main.rs (check-fail: error[PhiStateConflict]: incoming control-flow states do not agree)
- 105_let_else/main.rs (check-fail: error[PhiStateConflict]: incoming control-flow states do not agree)
- 108_atomic_spin_lock/main.rs (check-fail: error[PhiStateConflict]: incoming control-flow states do not agree)
- 10_generics_monomorph/main.rs (check-fail: error[UseAfterMove]: moved value is no longer usable)
- 118_global_mutable_state/main.rs (check-fail: error[UseAfterMove]: moved value is no longer usable)
- 11_tuples/main.rs (check-fail: error[UseAfterMove]: moved value is no longer usable)
- 126_mpmc_channel/main.rs (check-fail: error[UseAfterMove]: moved value is no longer usable)
- 129_seqlock_optimistic/main.rs (check-fail: error[MemoryLeak]: live registers remain at function exit)
- 12_destructuring/main.rs (check-fail: error[UseAfterMove]: moved value is no longer usable)
- 130_park_unpark_thread/main.rs (check-fail: error[UseAfterMove]: moved value is no longer usable)
- 137_io_uring_submission/main.rs (check-fail: error[UseAfterMove]: moved value is no longer usable)
- 149_packed_repr/main.rs (check-fail: error[UseAfterMove]: moved value is no longer usable)
- 150_c_repr_alignment/main.rs (check-fail: error[UseAfterMove]: moved value is no longer usable)
- 16_methods/main.rs (check-fail: error[UseAfterMove]: moved value is no longer usable)
- 182_mmap_memory_mapping/main.rs (check-fail: error[MemoryLeak]: live registers remain at function exit)
- 183_signal_handling_setup/main.rs (check-fail: error[UseAfterMove]: moved value is no longer usable)
- 186_sqlite_c_api_binding/main.rs (check-fail: error[UseAfterMove]: moved value is no longer usable)
- 187_opengl_context_swap/main.rs (check-fail: error[UseAfterMove]: moved value is no longer usable)
- 188_websocket_frame_parse/main.rs (check-fail: error[UseAfterMove]: moved value is no longer usable)
- 189_protobuf_varint_decode/main.rs (check-fail: error[PhiStateConflict]: incoming control-flow states do not agree)
- 190_base64_encode_simd/main.rs (check-fail: error[UseAfterMove]: moved value is no longer usable)
- 192_proc_macro_derive_ast/main.rs (check-fail: error[UseAfterMove]: moved value is no longer usable)
- 193_attribute_macro_rewrite/main.rs (check-fail: error[UseAfterMove]: moved value is no longer usable)
- 197_profile_guided_opt/main.rs (check-fail: error[PhiStateConflict]: incoming control-flow states do not agree)
- 199_address_sanitizer_asan/main.rs (check-fail: error[UseAfterMove]: moved value is no longer usable)
- 22_break_continue/main.rs (check-fail: error[UseAfterMove]: moved value is no longer usable)
- 23_nested_loops/main.rs (check-fail: error[PhiStateConflict]: incoming control-flow states do not agree)
- 25_fibonacci/main.rs (check-fail: error[PhiStateConflict]: incoming control-flow states do not agree)
- 28_borrow_chains/main.rs (check-fail: error[UseAfterMove]: moved value is no longer usable)
- 303_match_guard_macro/main.rs (check-fail: error[UseAfterMove]: moved value is no longer usable)
- 304_while_let_macro/main.rs (check-fail: error[PhiStateConflict]: incoming control-flow states do not agree)
- 305_range_pattern_macro/main.rs (check-fail: error[UseAfterMove]: moved value is no longer usable)
- 307_at_binding_macro/main.rs (check-fail: error[UseAfterMove]: moved value is no longer usable)
- 308_rest_pattern_macro/main.rs (check-fail: error[PhiStateConflict]: incoming control-flow states do not agree)
- 30_manual_guard_branch/main.rs (check-fail: error[PhiStateConflict]: incoming control-flow states do not agree)
- 310_generator_yield_macro/main.rs (check-fail: error[PhiStateConflict]: incoming control-flow states do not agree)
- 313_matches_macro/main.rs (check-fail: error[PhiStateConflict]: incoming control-flow states do not agree)
- 314_tail_call_become/main.rs (check-fail: error[PhiStateConflict]: incoming control-flow states do not agree)
- 317_for_each_iter_macro/main.rs (check-fail: error[PhiStateConflict]: incoming control-flow states do not agree)
- 318_vec_literal_macro/main.rs (check-fail: error[UseAfterMove]: moved value is no longer usable)
- 31_trait_static_dispatch/main.rs (check-fail: error[UseAfterMove]: moved value is no longer usable)
- 321_operator_overload_macro/main.rs (check-fail: error[UseAfterMove]: moved value is no longer usable)
- 323_index_trait_macro/main.rs (check-fail: error[UseAfterMove]: moved value is no longer usable)
- 331_rc_shared_ownership/main.rs (check-fail: error[UseAfterMove]: moved value is no longer usable)
- 333_weak_cyclic_reference/main.rs (check-fail: error[PhiStateConflict]: incoming control-flow states do not agree)
- 36_tuple_struct/main.rs (check-fail: error[UseAfterMove]: moved value is no longer usable)
- 39_generic_enum_i32/main.rs (check-fail: error[PhiStateConflict]: incoming control-flow states do not agree)
- 45_config_merge/main.rs (check-fail: error[UseAfterMove]: moved value is no longer usable)
- 47_tuple_swap/main.rs (check-fail: error[UseAfterMove]: moved value is no longer usable)
- 48_generic_pair/main.rs (check-fail: error[UseAfterMove]: moved value is no longer usable)
- 52_queue_rotate/main.rs (check-fail: error[UseAfterMove]: moved value is no longer usable)
- 54_mem_fill/main.rs (check-fail: error[UseAfterMove]: moved value is no longer usable)
- 55_builder_pattern/main.rs (check-fail: error[UseAfterMove]: moved value is no longer usable)
- 57_event_loop/main.rs (check-fail: error[PhiStateConflict]: incoming control-flow states do not agree)
- 62_channel_pingpong/main.rs (check-fail: error[UseAfterMove]: moved value is no longer usable)
- 65_job_scheduler/main.rs (check-fail: error[PhiStateConflict]: incoming control-flow states do not agree)
- 72_graph_walk/main.rs (check-fail: error[UseAfterMove]: moved value is no longer usable)

## SARS_FAIL files (rustc rejects the input; unrelated to the driver)

- 101_custom_drop/main.rs (rustc-reject)
- 104_if_let_chains/main.rs (rustc-reject)
- 133_select_macro_race/main.rs (rustc-reject)
- 134_join_all_futures/main.rs (rustc-reject)
- 135_async_streams/main.rs (rustc-reject)
- 136_executor_task_queue/main.rs (rustc-reject)
- 140_yield_now_suspend/main.rs (rustc-reject)
- 161_generic_associated_types/main.rs (rustc-reject)
- 166_specialization_fallback/main.rs (rustc-reject)
- 168_type_alias_impl_trait/main.rs (rustc-reject)
- 169_negative_impls/main.rs (rustc-reject)
- 195_build_script_codegen/main.rs (rustc-reject)
- 309_try_block_macro/main.rs (rustc-reject)

Classification rules: see tools/sweep-README.md. Per-file results:
tools/sweep-results-T28.tsv. Per-file work outputs (mir.json, .sa,
stage logs) are under /tmp/sarust-sweep-T28/ and are not committed.
