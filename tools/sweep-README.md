# sarust sweep harness (T28 baseline)

`tools/sweep.sh` runs every `demos/rosetta` `main.rs` through the full
pipeline and classifies the result. It never modifies `sci` (the corpus is
read-only input); it only writes under the sarust repo (`tools/`) and the
scratch work dir (`/tmp/sarust-sweep-T28/`).

## Pipeline (per file, in order)

1. `sars --edition 2021 <main.rs> --rsc-out out.mir.json --crate-type bin`
   with `LD_LIBRARY_PATH=<sysroot>/lib` (bare run exits 127).
   Timeout 180s.
2. `mir2sa lower out.mir.json --out out.sa`. Timeout 120s.
3. `sa check --no-incremental out.sa` (hermetic: no verdict cache).
   Timeout 300s.

Every stage saves its own exit code; a timeout (exit 124) is a TRAP, never
a silent skip. File order is deterministic (`LC_ALL=C sort`).

## Classification (applied exactly as written)

| class | rule |
|-------|------|
| GREEN | sars ok, mir2sa ok, `sa check` ok, loud count = 0. Fully supported. |
| CHECK_ONLY | sars ok, mir2sa ok, `sa check` ok, loud count > 0. It checks, but unsupported parts are loud stubs. |
| TRAP | mir2sa fails, or `sa check` fails, or sars fails without rustc diagnostics, or any stage times out, or sars produces no mir.json. A driver/pipeline failure. |
| SARS_FAIL | sars exits non-zero AND its log contains rustc diagnostics (`error[` / `error:` / `refusing to emit`). rustc rejects the input; unrelated to the driver. |

`loud` per file is mir2sa's own reported `UNSUPPORTED=N` count (the `wrote
... UNSUPPORTED=N` line). The grep cross-check counts both `// UNSUPPORTED`
lines and `// overflow assert folded` lines over the `.sa`: folded overflow
asserts are counted in mir2sa's tally and marked with their own comment,
since the checked helper traps instead of the assert (see
`rsc/mir2sa/src/lower.rs`, overflow asserts are folded deliberately, not
dropped silently). A mismatch is flagged in the note
(`loud-mismatch mir2sa=X grep=Y`) but does not change the class.
`loud total` is the sum of per-file loud counts over lowered files.

## Usage

```sh
tools/sweep.sh                    # full sweep, resumable
tools/sweep.sh --limit 5          # first 5 files only (testing)
tools/sweep.sh --force            # discard results, re-run all (old results -> .bak)
tools/sweep.sh --summarize-only   # regenerate the summary from existing results
```

Resume: files already present in `tools/sweep-results-T28.tsv` are skipped
unless `--force` is given. Each result line is appended as soon as its file
finishes, so a killed run loses at most the file in flight.

## Outputs

- `tools/sweep-results-T28.tsv`: one line per file:
  `rel<TAB>class<TAB>loud<TAB>sars_rc<TAB>mir2sa_rc<TAB>sa_rc<TAB>note`
- `tools/sweep-baseline-T28.md`: totals, loud total, TRAP and SARS_FAIL
  lists, and the exact binary versions used.
- `/tmp/sarust-sweep-T28/<slug>/`: per-file `out.mir.json`, `out.sa`,
  `sars.log`, `mir2sa.log`, `sa.log` (not committed; kept for debugging
  TRAP rows).

## Environment requirements

- `sars` at `rsc/driver/target/sars`, `mir2sa` at
  `rsc/target/debug/mir2sa`, `sa` via `/content/sa_all/env.sh`.
- `~/.cargo/bin` on PATH (rustc shim) and
  `RUSTUP_TOOLCHAIN=nightly-2026-09-28` for the sysroot lookup.
