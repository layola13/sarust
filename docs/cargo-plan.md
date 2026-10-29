# Cargo build plan — stream 7 interface for stream 8

Stream 7 delivers Cargo.toml parsing + feature resolution as a JSON build
plan. Stream 8 (module tree + multi-crate build orchestration) consumes
this plan; it does not need to link `mir2sa` as a library.

## Command

```sh
mir2sa cargo <manifest-dir | Cargo.toml> \
  [--features a,b] [--no-default-features] [--all-features] \
  [--out plan.json]
```

With `--out`, stdout is a one-line summary
(`wrote <path> crates=N features=[...] unsupported=M`); without it, the
JSON goes to stdout. Exit 0 even when `unsupported` is non-empty (a
partial plan with an explicit gap list); exit 2 on fatal problems
(unreadable manifest, TOML syntax error, missing `[package] name`,
unknown `--features` entry, unknown flag).

## JSON schema (schema_version 1)

```json
{
  "schema_version": 1,
  "root_manifest": "/abs/path/to/Cargo.toml",
  "root_crate": "app",
  "crates": [ "<PlannedCrate>, dependency-first, root LAST" ],
  "enabled_features": ["fast"],
  "cfg_args": ["--cfg", "feature=\"fast\""],
  "unsupported": ["crates.io dependency 'serde' of 'app' (version req 1) ..."],
  "warnings": ["... parse-level loud items ..."],
  "excluded_target_deps": ["win-dep (target 'cfg(windows)', from app)"]
}
```

`PlannedCrate`:

| field | meaning |
|---|---|
| `name` | package name |
| `manifest_dir`, `manifest_path` | absolute paths |
| `edition` | manifest edition, or `"2015"` when absent |
| `lib` / `bins` | targets; `path` is absolute (manifest-dir-joined); `crate_type` is `"lib"` / `"bin"`; `explicit_crate_type` only when the manifest sets `crate-type` |
| `enabled_features` | this crate's resolved features, sorted, `"default"` excluded |
| `cfg_args` | argv tokens to splice into a sars invocation (sars forwards every non-`--rsc-out` argument to rustc, so these are real `cfg(feature)` gates) |
| `edge_kind` | `"root"` for the root, else `"normal"` / `"build"` (the edge from its parent) |
| `dependencies` | this crate's own parsed deps; path deps carry `resolved_path` (absolute dir) when resolvable |

`Dependency` fields: `name` (manifest key), `package` (real name on
rename), `version`, `path` (as written), `resolved_path`, `git`,
`git_ref`, `optional`, `default_features`, `features`, `kind`
(`normal`/`dev`/`build`), `target` (raw `[target.…]` key, if any),
`workspace_inherited`.

## Semantics stream 8 can rely on

- **Order:** `crates` is post-order (dependencies first, root last), so a
  straight left-to-right walk builds leaves before their users.
  Deduplication is by manifest path; see the limitation below.
- **Features (Cargo rules, subset):** `default` unless
  `--no-default-features`; `--features` accepts comma lists, `dep:foo`
  (optional dep), and `dep/feat` (feature of a dep, which also enables an
  optional dep). Closure follows `[features]` items: `dep:` items,
  `dep/feat` items, plain items, and implicit features for optional deps.
  An unknown requested or referenced feature is a hard error, not a
  silent drop.
- **Path deps:** resolved recursively (normal + build edges; dev deps are
  parsed but never built by a plan). A path dep whose dir has no
  `Cargo.toml` lands in `unsupported`, not in `crates`.
- **Target deps:** `[target.<cfg|triple>.dependencies]` are evaluated on
  this host (`cfg(unix)`, `cfg(windows)`, `target_os`, `target_arch`,
  `target_family`, `target_pointer_width`, `target_endian`, `target_env`,
  `feature`, `all`/`any`/`not`). Non-matching deps go to
  `excluded_target_deps` (visible, not silent); unknown cfg atoms are
  false + a warning.
- **Loud, never silent:** `unsupported` covers crates.io downloads +
  version resolution, git deps, `workspace = true` inheritance, and
  missing path dirs. `warnings` covers parse-level subset gaps (unknown
  dep keys, non-array features, `[patch]`/`[replace]`, unexpanded
  `[workspace]` members). A plan consumer must surface `unsupported`
  before building; building only `crates` while ignoring `unsupported`
  would silently drop dependencies.

## Known limitations (stream 7 subset, explicit)

- No crates.io download / version resolution, no git fetch, no
  `[workspace]` member expansion, no `[patch]`/`[replace]`.
- Feature unification across multiple parents is partial: on revisit,
  the existing entry's `enabled_features`/`cfg_args` are merged, but its
  children are not re-expanded (a warning is recorded when the merge
  changes the set). First visit wins for child expansion.
- `cfg(feature)` inside `[target.…]` sections is evaluated against the
  *parent* crate's resolved features.
- Dotted keys inside `[dependencies]` tables and dotted keys in inline
  tables are not nested (they stay single dotted keys and are ignored
  as unknown keys, loudly).
- `autobins`/`autolib` follow Cargo defaults (true unless set false);
  `src/main.rs` / `src/lib.rs` existence is checked on disk.

## Tests / fixtures

- Unit tests: `rsc/mir2sa/src/cargo.rs` (`cargo::tests`, 7 tests).
- End to end: `tools/cargo-e2e.sh` (work under
  `$CARGO_E2E_WORK_ROOT`, default `/tmp/sarust-cargo-e2e`), fixtures in
  `rsc/mir2sa/tests/fixtures/cargo-path-dep/` (foo lib + app bin) and
  `rsc/mir2sa/tests/fixtures/cargo-feature/` (cfg-gated `mode()`).
  The path-dep case builds foo's rlib with real rustc and passes it to
  sars via `--extern`; cross-crate linking rules remain stream 8's work.

## What stream 7 did NOT change

The single-file pipeline (`sars` → `mir2sa lower` → `sa check`) is
untouched: `cargo.rs` is a new module plus one dispatch arm in
`main.rs`. The full demos sweep (332 files) is the regression gate;
see the stream 7 report for its counts.
