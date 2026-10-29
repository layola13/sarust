#!/usr/bin/env bash
# tools/build-project.sh — multi-crate build orchestration (stream 8).
#
# Builds a small multi-crate Rust project through the sarust pipeline:
#   per crate (dependency order): rustc --crate-type rlib (deps only,
#     so later crates can --extern them for typechecking) -> sars
#     (crate MIR -> mir.json) -> mir2sa link (all mir.json -> one .sa)
#     -> sa check.
#
# Why rustc builds the rlibs: sars emits mir.json, not an rlib, so a
# dependent crate's typechecking needs a real rlib on --extern. That
# rlib comes from the same pinned nightly rustc, never from sars.
#
# Two ways to name the crates (mutually exclusive):
#
#   1. Explicit (stream 8's original interface; stream 7's parser will
#      generate exactly these lines later if ever needed):
#        tools/build-project.sh \
#          --crate mylib=path/to/mylib/src/lib.rs \
#          --crate mybin=path/to/mybin/src/main.rs \
#          --bin mybin --out-dir /tmp/out --out /tmp/out/linked.sa
#      Crates are given in dependency order, deps first, bin last.
#      --bin defaults to the last --crate and must be last (a bin cannot
#      be --extern'd by a later crate).
#
#   2. Stream 7 build plan (preferred when a Cargo.toml exists):
#        mir2sa cargo <manifest-dir> --out plan.json   # stream 7
#        tools/build-project.sh --plan plan.json [--bin <target>]
#          --out-dir /tmp/out --out /tmp/out/linked.sa
#      tools/plan-adapter.py translates the plan (schema_version 1,
#      crates in dependency-first post-order, root last) into build
#      entries. The adapter prints the plan's unsupported / warnings /
#      excluded_target_deps sections BEFORE any build step; building a
#      partial plan while ignoring those sections would silently drop
#      dependencies, so the display is mandatory, not advisory. A
#      actually-used missing dependency still fails loudly later at
#      sars typechecking (unresolved import -> rustc error).
#      --bin in plan mode names a bin TARGET of the root crate; with no
#      --bin, a single root bin is used, zero root bins means lib-only
#      link, and several root bins without --bin is a loud error.
#      Per-crate cfg_args from the plan are spliced into both the rustc
#      rlib build and the sars invocation (sars forwards non---rsc-out
#      arguments to rustc, so they are real cfg(feature) gates).
#
# crates.io / git dependencies are out of scope here (stream 7 reports
# them in `unsupported`; they never reach the crates list). A crate
# that needs one fails loudly at the rustc/sars stage, never silently.
#
# Rules: every stage saves its own exit code; a failure prints the
# stage and its rc and the script exits with that rc. New unsupported
# constructs stay loud via mir2sa's UNSUPPORTED count, never swallowed.
# Output files are indexed (<idx>_<name>.mir.json, lib<idx>_<name>.rlib)
# because a package's lib and bin commonly share one crate name.
#
# Usage:
#   tools/build-project.sh --crate <name>=<root.rs> [--crate ...]
#     [--bin <name>] [--edition 2021] [--out-dir <dir>] [--out <file.sa>]
#     [--no-check]
#   tools/build-project.sh --plan <plan.json> [--bin <target>]
#     [--out-dir <dir>] [--out <file.sa>] [--no-check]

set -uo pipefail

SARUST_DIR="$(cd "$(dirname "$0")/.." && pwd)"
SARS_BIN="$SARUST_DIR/rsc/driver/target/sars"
MIR2SA_BIN="$SARUST_DIR/rsc/target/debug/mir2sa"
PLAN_ADAPTER="$SARUST_DIR/tools/plan-adapter.py"
ENV_SH="/content/sa_all/env.sh"

TOOLCHAIN="${RUSTUP_TOOLCHAIN:-nightly-2026-09-28}"
EDITION="2021"
EDITION_SET=0
OUT_DIR=""
OUT=""
BIN=""
PLAN=""
NO_CHECK=0
declare -a CRATE_NAMES=()
declare -a CRATE_ROOTS=()

while [ $# -gt 0 ]; do
  case "$1" in
    --crate)
      spec="$2"; shift 2
      name="${spec%%=*}"; root="${spec#*=}"
      [ -n "$name" ] && [ -n "$root" ] && [ "$name" != "$spec" ] || { echo "bad --crate spec: $spec (want name=root.rs)" >&2; exit 2; }
      CRATE_NAMES+=("$name"); CRATE_ROOTS+=("$root") ;;
    --plan) PLAN="$2"; shift 2 ;;
    --bin) BIN="$2"; shift 2 ;;
    --edition) EDITION="$2"; EDITION_SET=1; shift 2 ;;
    --out-dir) OUT_DIR="$2"; shift 2 ;;
    --out) OUT="$2"; shift 2 ;;
    --no-check) NO_CHECK=1; shift ;;
    *) echo "unknown arg: $1" >&2; exit 2 ;;
  esac
done

[ -n "$PLAN" ] && [ "${#CRATE_NAMES[@]}" -gt 0 ] && { echo "--plan and --crate are mutually exclusive" >&2; exit 2; }
[ -n "$PLAN" ] && [ "$EDITION_SET" -eq 1 ] && { echo "--edition with --plan is rejected: per-crate editions come from the plan" >&2; exit 2; }
[ -n "$PLAN" ] || [ "${#CRATE_NAMES[@]}" -gt 0 ] || { echo "usage: build-project.sh (--crate <name>=<root.rs> [...] | --plan <plan.json>) [--bin <name>] [--out-dir <dir>] [--out <file.sa>]" >&2; exit 2; }

[ -n "$OUT_DIR" ] || OUT_DIR="./sarust-out"
[ -n "$OUT" ] || OUT="$OUT_DIR/linked.sa"
mkdir -p "$OUT_DIR" || exit 2

export PATH="$HOME/.cargo/bin:$PATH"
export RUSTUP_TOOLCHAIN="$TOOLCHAIN"
if [ -f "$ENV_SH" ]; then
  # shellcheck disable=SC1090
  source "$ENV_SH"
fi
SA_BIN="$(command -v sa || true)"
[ -z "$SA_BIN" ] && SA_BIN="/content/sa_all/sci/zig-out/bin/sa"

for bin in "$SARS_BIN" "$MIR2SA_BIN"; do
  [ -x "$bin" ] || { echo "missing executable: $bin" >&2; exit 1; }
done
[ "$NO_CHECK" -eq 1 ] || [ -x "$SA_BIN" ] || { echo "missing executable: $SA_BIN" >&2; exit 1; }

SYSROOT="$(RUSTUP_TOOLCHAIN="$TOOLCHAIN" rustc --print sysroot 2>/dev/null || true)"
[ -z "$SYSROOT" ] && { echo "cannot determine rustc sysroot" >&2; exit 1; }
[ -d "$SYSROOT/lib" ] || { echo "missing sysroot lib: $SYSROOT/lib" >&2; exit 1; }

# --- build entries: parallel arrays NAME / KIND / ROOT / EDITION --------
# KIND is rlib or bin. In --crate mode the last entry is the bin; in
# --plan mode the adapter fills these (plus PLAN_CFG_<i> per entry).
declare -a ENTRY_NAMES=()
declare -a ENTRY_KINDS=()
declare -a ENTRY_ROOTS=()
declare -a ENTRY_EDITIONS=()
PLAN_MODE=0

if [ -n "$PLAN" ]; then
  PLAN_MODE=1
  [ -x "$PLAN_ADAPTER" ] || [ -f "$PLAN_ADAPTER" ] || { echo "missing adapter: $PLAN_ADAPTER" >&2; exit 1; }
  adapter_args=(--plan "$PLAN" --out "$OUT_DIR/.plan.sh")
  [ -n "$BIN" ] && adapter_args+=(--bin "$BIN")
  # Gap sections print here, BEFORE any build step (adapter contract).
  python3 "$PLAN_ADAPTER" "${adapter_args[@]}" || exit $?
  # shellcheck disable=SC1090
  source "$OUT_DIR/.plan.sh"
  for ((i = 0; i < PLAN_COUNT; i++)); do
    ENTRY_NAMES+=("${PLAN_NAMES[$i]}")
    ENTRY_KINDS+=("${PLAN_KINDS[$i]}")
    ENTRY_ROOTS+=("${PLAN_ROOTS[$i]}")
    ENTRY_EDITIONS+=("${PLAN_EDITIONS[$i]}")
  done
else
  LAST_IDX=$((${#CRATE_NAMES[@]} - 1))
  [ -z "$BIN" ] && BIN="${CRATE_NAMES[$LAST_IDX]}"
  BIN_IDX=-1
  for i in "${!CRATE_NAMES[@]}"; do
    [ "${CRATE_NAMES[$i]}" = "$BIN" ] && BIN_IDX="$i"
  done
  [ "$BIN_IDX" -ge 0 ] || { echo "bin crate '$BIN' not among --crate entries" >&2; exit 2; }
  [ "$BIN_IDX" -eq "$LAST_IDX" ] || { echo "bin crate '$BIN' must be the last --crate entry (deps first, bin last)" >&2; exit 2; }
  for i in "${!CRATE_NAMES[@]}"; do
    # Cargo normalization: crate names use '_' where packages use '-'.
    ENTRY_NAMES+=("${CRATE_NAMES[$i]//-/_}")
    if [ "$i" -eq "$LAST_IDX" ]; then ENTRY_KINDS+=("bin"); else ENTRY_KINDS+=("rlib"); fi
    ENTRY_ROOTS+=("${CRATE_ROOTS[$i]}")
    ENTRY_EDITIONS+=("$EDITION")
  done
fi

fail() { echo "build-project: FAILED stage=$1 crate=$2 rc=$3" >&2; exit "$3"; }

declare -a MIR_JSONS=()
declare -a EXTERN_ARGS=()
for i in "${!ENTRY_NAMES[@]}"; do
  name="${ENTRY_NAMES[$i]}"; kind="${ENTRY_KINDS[$i]}"; root="${ENTRY_ROOTS[$i]}"; ed="${ENTRY_EDITIONS[$i]}"
  tag="${i}_${name}"
  [ -f "$root" ] || { echo "crate root not found: $root" >&2; exit 2; }
  if [ "$PLAN_MODE" -eq 1 ]; then
    cfg_ref="PLAN_CFG_${i}[@]"
    cfg_args=("${!cfg_ref}")
  else
    cfg_args=()
  fi
  if [ "$kind" = "rlib" ]; then
    # Real rlib for later crates' --extern (sars emits mir.json, not rlib).
    RUSTUP_TOOLCHAIN="$TOOLCHAIN" rustc --edition "$ed" --crate-type rlib \
      --crate-name "$name" "${cfg_args[@]}" "${EXTERN_ARGS[@]}" "$root" \
      -o "$OUT_DIR/lib${tag}.rlib" >"$OUT_DIR/${tag}.rustc.log" 2>&1
    rc=$?
    echo "crate $name rustc rc=$rc"
    [ "$rc" -eq 0 ] || fail "rustc-rlib" "$name" "$rc"
    EXTERN_ARGS+=("--extern" "${name}=$OUT_DIR/lib${tag}.rlib")
  fi
  LD_LIBRARY_PATH="$SYSROOT/lib" "$SARS_BIN" --edition "$ed" \
    --crate-type "$kind" --crate-name "$name" "${cfg_args[@]}" "${EXTERN_ARGS[@]}" "$root" \
    --rsc-out "$OUT_DIR/${tag}.mir.json" >"$OUT_DIR/${tag}.sars.log" 2>&1
  rc=$?
  echo "crate $name sars rc=$rc"
  [ "$rc" -eq 0 ] || fail "sars" "$name" "$rc"
  MIR_JSONS+=("$OUT_DIR/${tag}.mir.json")
done

"$MIR2SA_BIN" link "${MIR_JSONS[@]}" --out "$OUT" >"$OUT_DIR/link.log" 2>&1
rc=$?
echo "link rc=$rc"
cat "$OUT_DIR/link.log"
[ "$rc" -eq 0 ] || fail "mir2sa-link" "-" "$rc"

if [ "$NO_CHECK" -eq 1 ]; then
  echo "sa check skipped (--no-check)"
  echo "build-project: OK out=$OUT"
  exit 0
fi
"$SA_BIN" check --no-incremental "$OUT" >"$OUT_DIR/sa.log" 2>&1
rc=$?
echo "sa check rc=$rc"
[ "$rc" -eq 0 ] || { head -5 "$OUT_DIR/sa.log" >&2; fail "sa-check" "-" "$rc"; }
echo "build-project: OK out=$OUT"
