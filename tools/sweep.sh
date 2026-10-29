#!/usr/bin/env bash
# sarust T28 baseline sweep.
#
# For every demos/rosetta main.rs, run:
#   sars -> mir.json -> mir2sa -> .sa -> sa check
# and classify the result. The classification rules live in
# tools/sweep-README.md and are applied exactly as written there.
#
# Usage:
#   tools/sweep.sh [--force] [--limit N] [--summarize-only]
#
#   --force           Discard existing results and re-run everything
#                     (old results are kept as sweep-results-T28.tsv.bak).
#   --limit N         Only the first N files in sorted order (testing).
#   --summarize-only  Regenerate the summary from existing results only.
#
# Outputs (all under the sarust repo tools/ dir):
#   tools/sweep-results-T28.tsv  per-file results (TSV, append-only, resumable)
#   tools/sweep-baseline-T28.md  summary table (regenerated each run)
# Per-file work outputs (mir.json, .sa, stage logs) live in
# /tmp/sarust-sweep-T28/<slug>/ and are NOT committed.
#
# Rules: every stage command saves its own exit code; a timeout is a TRAP,
# never a silent skip; new unsupported constructs stay loud (UNSUPPORTED),
# never silently swallowed.

set -uo pipefail

SARUST_DIR="$(cd "$(dirname "$0")/.." && pwd)"
CORPUS_DIR="/content/sa_all/sci/demos/rosetta"
SARS_BIN="$SARUST_DIR/rsc/driver/target/sars"
MIR2SA_BIN="$SARUST_DIR/rsc/target/debug/mir2sa"
ENV_SH="/content/sa_all/env.sh"
RESULTS_FILE="$SARUST_DIR/tools/sweep-results-T28.tsv"
SUMMARY_FILE="$SARUST_DIR/tools/sweep-baseline-T28.md"
WORK_ROOT="${SWEEP_WORK_ROOT:-/tmp/sarust-sweep-T28}"

SARS_TIMEOUT=180
MIR2SA_TIMEOUT=120
SA_TIMEOUT=300

FORCE=0
LIMIT=0
SUMMARIZE_ONLY=0

while [ $# -gt 0 ]; do
  case "$1" in
    --force) FORCE=1; shift ;;
    --limit) LIMIT="$2"; shift 2 ;;
    --summarize-only) SUMMARIZE_ONLY=1; shift ;;
    *) echo "unknown arg: $1" >&2; exit 2 ;;
  esac
done

# --- environment ------------------------------------------------------------
export PATH="$HOME/.cargo/bin:$PATH"
if [ -f "$ENV_SH" ]; then
  # shellcheck disable=SC1090
  source "$ENV_SH"
fi
SA_BIN="$(command -v sa || true)"
[ -z "$SA_BIN" ] && SA_BIN="/content/sa_all/sci/zig-out/bin/sa"

for bin in "$SARS_BIN" "$MIR2SA_BIN" "$SA_BIN"; do
  if [ ! -x "$bin" ]; then echo "missing executable: $bin" >&2; exit 1; fi
done
[ -d "$CORPUS_DIR" ] || { echo "missing corpus dir: $CORPUS_DIR" >&2; exit 1; }

SYSROOT="$(RUSTUP_TOOLCHAIN=nightly-2026-09-28 rustc --print sysroot 2>/dev/null || true)"
[ -z "$SYSROOT" ] && { echo "cannot determine rustc sysroot" >&2; exit 1; }
SYSROOT_LIB="$SYSROOT/lib"
[ -d "$SYSROOT_LIB" ] || { echo "missing sysroot lib: $SYSROOT_LIB" >&2; exit 1; }

SARUST_HEAD="$(git -C "$SARUST_DIR" rev-parse HEAD 2>/dev/null || echo unknown)"
SA_VERSION="$("$SA_BIN" --version 2>/dev/null | head -1 || echo unknown)"
RUSTC_VERSION="$(RUSTUP_TOOLCHAIN=nightly-2026-09-28 rustc --version 2>/dev/null || echo unknown)"

mkdir -p "$WORK_ROOT"
touch "$RESULTS_FILE"

if [ "$FORCE" -eq 1 ] && [ "$SUMMARIZE_ONLY" -eq 0 ]; then
  cp "$RESULTS_FILE" "$RESULTS_FILE.bak"
  : > "$RESULTS_FILE"
  echo "sweep: --force, old results backed up to $RESULTS_FILE.bak"
fi

echo "sweep config: corpus=$CORPUS_DIR"
echo "sweep config: sars=$SARS_BIN mir2sa=$MIR2SA_BIN sa=$SA_BIN"
echo "sweep config: sarust_head=$SARUST_HEAD sa_version=$SA_VERSION rustc=$RUSTC_VERSION"
echo "sweep config: results=$RESULTS_FILE work=$WORK_ROOT"

# --- per-file run -----------------------------------------------------------
append_result() {
  # $1=rel $2=class $3=loud $4=sars_rc $5=mir2sa_rc $6=sa_rc $7=note
  local note
  note="$(printf '%s' "$7" | tr '\t\n' '  ')"
  printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\n' "$1" "$2" "$3" "$4" "$5" "$6" "$note" >> "$RESULTS_FILE"
}

run_one() {
  local rel="$1"
  local src="$CORPUS_DIR/$rel"
  local slug="${rel//\//__}"
  local work="$WORK_ROOT/$slug"
  mkdir -p "$work"

  local class="" loud=0 sars_rc=0 mir2sa_rc=0 sa_rc=0 note="" mismatch=""

  # Stage 1: sars (rustc driver -> mir.json). Must run with
  # LD_LIBRARY_PATH=<sysroot>/lib, otherwise exit 127.
  timeout "$SARS_TIMEOUT" env LD_LIBRARY_PATH="$SYSROOT_LIB" \
    "$SARS_BIN" --edition 2021 "$src" --rsc-out "$work/out.mir.json" --crate-type bin \
    >"$work/sars.log" 2>&1
  sars_rc=$?
  if [ "$sars_rc" -ne 0 ]; then
    if [ "$sars_rc" -eq 124 ]; then
      class="TRAP"; note="sars-timeout"
    elif grep -qE 'error(\[|:)|refusing to emit' "$work/sars.log" 2>/dev/null; then
      class="SARS_FAIL"; note="rustc-reject"
    else
      class="TRAP"
      note="sars-fail rc=$sars_rc: $(head -1 "$work/sars.log" 2>/dev/null | cut -c1-120)"
    fi
    append_result "$rel" "$class" "$loud" "$sars_rc" "$mir2sa_rc" "$sa_rc" "$note"
    echo "$rel -> $class loud=$loud ($note)"
    return 0
  fi
  if [ ! -s "$work/out.mir.json" ]; then
    class="TRAP"; note="sars-empty-mir"
    append_result "$rel" "$class" "$loud" "$sars_rc" "$mir2sa_rc" "$sa_rc" "$note"
    echo "$rel -> $class loud=$loud ($note)"
    return 0
  fi

  # Stage 2: mir2sa (mir.json -> .sa). Its own UNSUPPORTED count is the
  # canonical loud count; the grep count is a cross-check only.
  timeout "$MIR2SA_TIMEOUT" "$MIR2SA_BIN" lower "$work/out.mir.json" --out "$work/out.sa" \
    >"$work/mir2sa.log" 2>&1
  mir2sa_rc=$?
  if [ "$mir2sa_rc" -ne 0 ]; then
    class="TRAP"
    note="mir2sa-fail rc=$mir2sa_rc: $(head -1 "$work/mir2sa.log" 2>/dev/null | cut -c1-120)"
    append_result "$rel" "$class" "$loud" "$sars_rc" "$mir2sa_rc" "$sa_rc" "$note"
    echo "$rel -> $class loud=$loud ($note)"
    return 0
  fi
  loud="$(sed -n 's/^wrote .* UNSUPPORTED=\([0-9][0-9]*\).*/\1/p' "$work/mir2sa.log" | head -1)"
  loud="${loud:-0}"
  loud_grep="$(grep -c -E '^[[:space:]]*// (UNSUPPORTED|overflow assert folded)' "$work/out.sa" 2>/dev/null)"
  loud_grep="${loud_grep:-0}"
  if [ "$loud" != "$loud_grep" ]; then
    mismatch="loud-mismatch mir2sa=$loud grep=$loud_grep"
  fi

  # Stage 3: sa check (hermetic: --no-incremental, no verdict cache).
  timeout "$SA_TIMEOUT" "$SA_BIN" check --no-incremental "$work/out.sa" \
    >"$work/sa.log" 2>&1
  sa_rc=$?
  if [ "$sa_rc" -ne 0 ]; then
    class="TRAP"
    if [ "$sa_rc" -eq 124 ]; then
      note="sa-timeout"
    else
      note="check-fail: $(head -1 "$work/sa.log" 2>/dev/null | cut -c1-120)"
    fi
  else
    if [ "$loud" -eq 0 ]; then class="GREEN"; else class="CHECK_ONLY"; fi
    note="ok"
  fi
  if [ -n "$mismatch" ]; then note="$note; $mismatch"; fi
  append_result "$rel" "$class" "$loud" "$sars_rc" "$mir2sa_rc" "$sa_rc" "$note"
  echo "$rel -> $class loud=$loud ($note)"
}

# --- summary ----------------------------------------------------------------
summarize() {
  local total green check_only trap_n sars_fail loud_total
  total="$(wc -l < "$RESULTS_FILE" | tr -d ' ')"
  green="$(awk -F'\t' '$2=="GREEN" {n++} END {print n+0}' "$RESULTS_FILE")"
  check_only="$(awk -F'\t' '$2=="CHECK_ONLY" {n++} END {print n+0}' "$RESULTS_FILE")"
  trap_n="$(awk -F'\t' '$2=="TRAP" {n++} END {print n+0}' "$RESULTS_FILE")"
  sars_fail="$(awk -F'\t' '$2=="SARS_FAIL" {n++} END {print n+0}' "$RESULTS_FILE")"
  loud_total="$(awk -F'\t' '{s+=$3} END {print s+0}' "$RESULTS_FILE")"
  {
    echo "# sarust T28 baseline sweep"
    echo ""
    echo "- date (UTC): $(date -u +%Y-%m-%dT%H:%M:%SZ)"
    echo "- sarust HEAD: $SARUST_HEAD"
    echo "- sa: $SA_VERSION"
    echo "- rustc: $RUSTC_VERSION"
    echo "- sars: $SARS_BIN"
    echo "- mir2sa: $MIR2SA_BIN"
    echo "- corpus: $CORPUS_DIR (main.rs only)"
    echo "- pipeline: sars --edition 2021 --crate-type bin -> mir2sa lower -> sa check --no-incremental"
    echo ""
    echo "| class | count |"
    echo "|-------|-------|"
    echo "| GREEN | $green |"
    echo "| CHECK_ONLY | $check_only |"
    echo "| TRAP | $trap_n |"
    echo "| SARS_FAIL | $sars_fail |"
    echo "| TOTAL | $total |"
    echo ""
    echo "- loud total (sum of mir2sa UNSUPPORTED over lowered files): $loud_total"
    echo ""
    echo "## TRAP files"
    echo ""
    awk -F'\t' '$2=="TRAP" {printf "- %s (%s)\n", $1, $7}' "$RESULTS_FILE"
    echo ""
    echo "## SARS_FAIL files (rustc rejects the input; unrelated to the driver)"
    echo ""
    awk -F'\t' '$2=="SARS_FAIL" {printf "- %s (%s)\n", $1, $7}' "$RESULTS_FILE"
    echo ""
    echo "Classification rules: see tools/sweep-README.md. Per-file results:"
    echo "tools/sweep-results-T28.tsv. Per-file work outputs (mir.json, .sa,"
    echo "stage logs) are under $WORK_ROOT/ and are not committed."
  } > "$SUMMARY_FILE"
  echo "summary: GREEN=$green CHECK_ONLY=$check_only TRAP=$trap_n SARS_FAIL=$sars_fail TOTAL=$total loud=$loud_total"
  echo "summary written to $SUMMARY_FILE"
}

# --- main -------------------------------------------------------------------
if [ "$SUMMARIZE_ONLY" -eq 1 ]; then
  summarize
  exit 0
fi

mapfile -t FILES < <(cd "$CORPUS_DIR" && find . -name main.rs | sed 's|^\./||' | LC_ALL=C sort)
echo "sweep: ${#FILES[@]} files in corpus"

declare -A DONE=()
if [ -s "$RESULTS_FILE" ]; then
  while IFS=$'\t' read -r rel _rest; do
    [ -n "$rel" ] && DONE["$rel"]=1
  done < "$RESULTS_FILE"
fi

START_TS="$(date +%s)"
i=0
ran=0
skipped=0
for rel in "${FILES[@]}"; do
  i=$((i + 1))
  if [ "$LIMIT" -gt 0 ] && [ "$i" -gt "$LIMIT" ]; then break; fi
  if [ "$FORCE" -eq 0 ] && [ "${DONE[$rel]:-}" = "1" ]; then
    skipped=$((skipped + 1))
    continue
  fi
  echo "[$i/${#FILES[@]}] $rel"
  run_one "$rel"
  ran=$((ran + 1))
done
END_TS="$(date +%s)"
echo "sweep done: ran=$ran skipped=$skipped elapsed=$((END_TS - START_TS))s"
summarize
