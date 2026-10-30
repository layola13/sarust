#!/usr/bin/env bash
# off6 regression: optimized MIR must materialize ptr field-0 reads
# into Assign-Use temps; no direct BinOp/Call/Aggregate operand may
# carry a projection. Saves every stage exit code; fails closed.
set -uo pipefail
WT="$(cd "$(dirname "$0")/../.." && pwd)"
OUT="${1:-/tmp/off6-run}"
SARS="$WT/rsc/driver/target/sars"
MIR2SA="$WT/rsc/target/debug/mir2sa"
export PATH="/root/.cargo/bin:$HOME/.cargo/bin:$PATH"
export RUSTUP_TOOLCHAIN=nightly-2026-09-28
SYSROOT="$(rustc --print sysroot 2>/dev/null || true)"
[ -x "$SARS" ] || { echo "missing sars: $SARS" >&2; exit 1; }
[ -x "$MIR2SA" ] || { echo "missing mir2sa: $MIR2SA" >&2; exit 1; }
[ -n "$SYSROOT" ] || { echo "cannot determine rustc sysroot" >&2; exit 1; }
SA="$(command -v sa || true)"
[ -z "$SA" ] && [ -x "/content/sa_all/sci/zig-out/bin/sa" ] && SA="/content/sa_all/sci/zig-out/bin/sa"
mkdir -p "$OUT"
fail=0
for f in "$WT"/tools/off6-probes/*.rs; do
  n="$(basename "$f" .rs)"; d="$OUT/$n"; mkdir -p "$d"
  LD_LIBRARY_PATH="$SYSROOT/lib" "$SARS" --edition 2021 "$f" --rsc-out "$d/out.mir.json" --crate-type bin >"$d/sars.log" 2>&1; src=$?
  "$MIR2SA" lower "$d/out.mir.json" --out "$d/out.sa" >"$d/mir2sa.log" 2>&1; mrc=$?
  python3 "$WT/tools/off6-probes/check.py" "$d/out.mir.json" >"$d/check.log" 2>&1; crc=$?
  echo "$n sars=$src mir2sa=$mrc shape_check=$crc"
  if [ "$src" -ne 0 ] || [ "$mrc" -ne 0 ] || [ "$crc" -ne 0 ]; then fail=1; fi
  if [ "$n" = "p_call_local" ] || [ "$n" = "p_agg_local" ]; then
    if [ -n "$SA" ]; then
      rustc --edition 2021 -o "$d/native" "$f" 2>"$d/native.log"; "$d/native" >/dev/null 2>&1; nrc=$?
      python3 "$WT/tools/fieldprec-transform.py" "$d/out.sa" "$d/run.sa"
      timeout 60 "$SA" check --no-incremental "$d/run.sa" >"$d/check2.log" 2>&1; ck=$?
      timeout 300 "$SA" build-exe --no-incremental "$d/run.sa" -o "$d/runbin" >"$d/build.log" 2>&1; brc=$?
      if [ "$brc" -eq 0 ]; then "$d/runbin" >/dev/null 2>&1; rrc=$?; else rrc="build-fail"; fi
      echo "  value native=$nrc sa=$rrc check=$ck"
      if [ "$nrc" != "$rrc" ] || [ "$ck" -ne 0 ]; then fail=1; fi
    else
      echo "  value SKIP (no sa binary)"
    fi
  fi
done
if [ "$fail" -eq 0 ]; then echo "off6: ALL OK"; else echo "off6: FAIL (see above)" >&2; fi
exit "$fail"
