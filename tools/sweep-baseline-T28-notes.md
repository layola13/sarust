# T28 baseline sweep — analyst notes (hand-written, not harness output)

- Baseline: 332 files, GREEN 139 / CHECK_ONLY 117 / TRAP 63 / SARS_FAIL 13,
  loud 516 (mir2sa UNSUPPORTED tally). Harness: tools/sweep.sh; results:
  tools/sweep-results-T28.tsv; summary: tools/sweep-baseline-T28.md.
  sarust HEAD at sweep time: 8935c4e8; sa 0.1.8; rustc
  nightly-2026-09-28 (1.101.0-nightly d080e7dff).
- Pipeline: sars --edition 2021 --crate-type bin -> mir2sa lower ->
  sa check --no-incremental. Classification rules: tools/sweep-README.md.
- loud markers: of the 516 counted louds, 137 are folded overflow asserts
  (hence 379 `// UNSUPPORTED` comments). Folded overflow asserts are
  deliberate (rsc/mir2sa/src/lower.rs: the checked helper traps instead of
  the assert; re-checking the flag would be nonsense) and marked with their
  own `// overflow assert folded` comment — not silently dropped. After
  counting both marker kinds, the re-run shows 0 loud-mismatch rows.
- SARS_FAIL 13 = old 10 (133, 134, 135, 136, 140, 161, 166, 168, 169, 309)
  + 3: 101_custom_drop, 104_if_let_chains (rustc: let chains need Rust
  2024; the harness forces --edition 2021, so these are edition artifacts,
  not driver bugs — likely to lower under --edition 2024) and
  195_build_script_codegen (needs OUT_DIR/GENERATED from a cargo build
  script; not compilable standalone). All verified driver-unrelated from
  sars.log diagnostics.
- TRAP families (from check-fail notes): UseAfterMove (UAM),
  PhiStateConflict (phi), BorrowConflict, MemoryLeak — the known T28 gap
  families and the re-walk targets (phi -> UAM -> phi2a -> ... per ROADMAP).
- Comparison caution: do not compare these counts directly to the old
  container's final numbers (GREEN 147 / CHECK_ONLY 170 / TRAP 5); that run
  included the phi/UAM/borrow_end/drop_glue/reb clusters, while this is the
  pre-rewalk T28 baseline, and the classification rules differ slightly
  (this baseline requires loud=0 for GREEN).
