# Phase 2 minimal corpus

9 single-file Rust cases, one per ROADMAP Phase 2 area: types, bindings,
control, functions, structs, enums, ownership, collections, errors.

Run per file: `sars --edition 2021 --crate-type bin <file> --rsc-out mir.json`
(under `LD_LIBRARY_PATH=<sysroot>/lib`), then `mir2sa lower mir.json --out out.sa`,
then `sa check --no-incremental out.sa`.

Acceptance (2026-10-30, main d8784a9): all 9 pass sa check (TRAP 0).
34 loud markers total, all characterized: 20 overflow folds (by design),
3 f64-literal policy, 4 version-return leftovers, 4 switch-discr leftovers,
1 static-str gap, 2 phi x binop interaction markers.
