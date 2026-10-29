# Stream 8: module tree + multi-crate build orchestration

## What this is

- **Module tree**: one `sars` run on a crate root already covers the
  whole `mod` tree — rustc resolves `mod`/`pub mod` files, `#[path]`,
  and nested modules (`util::inner` -> `src/util/inner.rs`). No
  hand-rolled mod parser: the official compiler is the parser.
- **Multi-crate linking**: `mir2sa link` merges one mir.json per crate
  (dependency order, bin last) into a single `.sa`, resolving
  same-name functions across modules and crates.
- **Orchestration**: `tools/build-project.sh` drives the per-crate
  stages (rustc rlib for deps -> sars -> link -> sa check).

## Pipeline

```text
crate root.rs --(rustc: resolve mod tree, typeck, borrowck)--> MIR
  --(sars: after_analysis)--> <crate>.mir.json   (functions carry `path`
       = crate-relative def path, file carries top-level `crate` name)
  --(mir2sa link, all crates in dep order)--> linked.sa
  --(sa check --no-incremental)--> verdict
```

Dependency crates are also built once with plain rustc to an rlib,
because sars emits mir.json, not an rlib, and a dependent crate needs
a real rlib on `--extern` for typechecking. The rlib uses the same
pinned nightly (`nightly-2026-09-28`) as sars.

## Naming / link rules (Jev jev-1.14.0, qualify-on-collision, p=0.86)

- A function keeps its short name unless that name is defined more
  than once in the linked set. Non-colliding output is byte-identical
  to plain `lower` output; single-crate `lower` is untouched (this is
  what makes the 332-file sweep a valid zero-regression gate).
- Colliding definitions are renamed from their identity
  (`crate::path`): `mylib::a::helper` -> `mylib__a__helper` (`__` so it
  can never equal a plain `mylib_a` short name). Residual collisions
  (a plain `a__b` fn next to `a::b`) get deterministic `__2`, `__3`
  suffixes in link order, with a stderr warning.
- Call resolution order for a `Term::Call` with `func_raw`: exact
  identity match, then same-file bare-path match (this disambiguates
  two crates sharing a module layout), then globally unique bare-path
  match. This also fixes the cross-crate short-name mismatch: sars
  emits a Debug-fallback name (`mylib_triple`) for extern-crate calls
  while the lib defines plain `triple`; link rewrites the call to the
  definition, so the bin no longer calls an `@extern` stub while the
  lib body sits unused (silently wrong before this change).
- Entry point: the definition with path `main` in a crate-named file
  keeps the bare name `main`. Two entry mains (two bins in one link)
  is a loud link error (exit 1, `duplicate entry-point main`), never a
  silent pick. A module fn literally named `main` (`m::main`) is
  qualified like any other collision.

## Interface: stream 7 build plan (implemented, not just assumed)

`tools/build-project.sh --plan plan.json` consumes stream 7's JSON
build plan directly (schema_version 1, `docs/cargo-plan.md` on branch
stream/cargo-toml; `mir2sa cargo <manifest-dir> --out plan.json`).
`tools/plan-adapter.py` translates it into build entries; no Cargo.toml
parser was duplicated on this side — the JSON file is the seam.

- Order: plan `crates` are dependency-first post-order, root last;
  entries are built left to right, every earlier rlib `--extern`'d
  into later crates.
- Gap display (mandatory): the adapter prints `unsupported` /
  `warnings` / `excluded_target_deps` before any build step. A partial
  plan may still build (the planner exits 0 with explicit gaps); an
  actually-used missing dependency then fails loudly at sars
  typechecking (unresolved import -> rustc error), never silently.
- Targets: every lib becomes an rlib entry; only the root crate's bins
  are link targets (non-root bins are skipped with a printed note; a
  non-root crate with no lib is a loud error). Root bin selection:
  `--bin <target name>` picks one; no `--bin` with a single root bin
  uses it; zero root bins means lib-only link; several root bins
  without `--bin` is a loud error.
- `cfg_args` are spliced per crate into both the rustc rlib build and
  the sars invocation (sars forwards non-`--rsc-out` args to rustc).
  Per-crate `edition` comes from the plan; `--edition` with `--plan`
  is rejected rather than silently overriding.
- `edge_kind` is advisory only (see plan-adapter.py): every lib is
  built and `--extern`'d, because a crate reached via both a normal
  and a build edge must not be misclassified by first-visit edge.
- `--crate-name` values are sanitized (`-` -> `_`), matching Cargo's
  normalization, identically for the rustc and sars builds of a crate.
- The explicit `--crate` interface is kept as a fallback and for
  non-Cargo layouts; `--plan` and `--crate` are mutually exclusive.

## Interface assumptions about stream 7 (residual)

- Bin target `name` fields are used as given (autobin `src/main.rs`
  -> package name, per stream 7's code); `--bin` matches that name.
- Fixtures under `examples/module-tree/` and
  `examples/multi-crate/` are real Cargo layouts (package + workspace
  + path dependency) so stream 7's planner can parse them unchanged
  later; the committed plan-mode test hand-builds an equivalent plan
  JSON at test time (absolute paths are machine-specific, so no sample
  plan file is committed).

## Commands

```sh
# Multi-file mod tree (same fn name in two modules + nested mod)
tools/build-project.sh --crate modtree_demo=examples/module-tree/src/main.rs \
  --bin modtree_demo --out-dir /tmp/mt --out /tmp/mt/linked.sa

# Multi-crate path dependency (lib + bin, cross-crate name collision)
tools/build-project.sh --crate mylib=examples/multi-crate/mylib/src/lib.rs \
  --crate mybin=examples/multi-crate/mybin/src/main.rs \
  --bin mybin --out-dir /tmp/mc --out /tmp/mc/linked.sa

# Same, via a stream 7 build plan (gap sections print before building)
mir2sa cargo examples/multi-crate/mybin --out /tmp/plan.json  # stream 7
tools/build-project.sh --plan /tmp/plan.json --out-dir /tmp/mc2 --out /tmp/mc2/linked.sa

# Both fixtures + negative test (duplicate entry main rejected)
tools/test-module-tree.sh

# Direct link (single input also works: intra-crate collisions only)
rsc/target/debug/mir2sa link a.mir.json b.mir.json --out out.sa

# mir2sa unit tests (includes link::link_tests)
CARGO_BUILD_JOBS=2 cargo test -p mir2sa   # (run from rsc/)
```

## Loud limits (never silently swallowed)

- `Rvalue::Call` carries no `func_raw` (it only arises from the
  `parse` path, never from sars output) and is left unchanged.
- `func_raw` is truncated to 100 chars by sars; a 100-char raw path
  resolves only on a unique prefix match, otherwise warns and leaves
  the call unchanged.
- Path-less inputs (old mir.json, `parse` output) fall back to
  `link<file-index>__<name>` qualification and warn; entry-main
  detection is unavailable for them.
- A merged .sa's mir.json is terminal: re-linking it is unsupported
  (its `crate` is unset and its calls are already rewritten).
- crates.io / git dependencies are out of scope: a crate needing one
  fails loudly at the rustc/sars stage. `#[cfg(feature)]` gating
  belongs to stream 7's feature handling, not to link.
- Two-arg `println!("{} {}", a, b)` fails `sa check` even single-file
  (pre-existing mir2sa limitation, verified on a plain file); the
  fixtures use single-arg `println!` and this is not papered over.

## Files changed (stream/module-tree)

- `rsc/driver/driver.rs`, `rsc/driver/emit.rs`: mir.json gains a
  top-level `crate` name and a per-function `path` (additive; old
  mir.json still parses, single-file `lower` output unchanged).
- `rsc/mir2sa/src/mir.rs`, `version.rs`, `main.rs`: `Function.path`
  and `MirFile.crate_name` plumbing (serde defaults keep old JSON
  working).
- `rsc/mir2sa/src/link.rs` (new): name resolution + `mir2sa link`
  subcommand + unit tests.
- `rsc/mir2sa/src/lower.rs`: `render_sa` extracted from `cmd_lower`
  (behavior-identical) and shared by `lower` and `link`.
- `tools/build-project.sh` (new), `tools/plan-adapter.py` (new),
  `tools/test-module-tree.sh` (new).
- `examples/module-tree/`, `examples/multi-crate/` (new fixtures).
