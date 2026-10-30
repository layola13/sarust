#!/usr/bin/env bash
# Index-ref value tests (sarust gap/index-ref branch).
#
# Corpus contract: same as tools/fieldprec-runner.sh -- every case under
# tools/idxref-corpus/ defines `fn probe() -> u64` with a distinct
# expected value below 256, so the process exit code is the observable.
# PASS requires native exit == SA exit, `sa check` clean, and mir2sa
# UNSUPPORTED=0 (OverflowAssertFolded alone is value-preserving and
# pre-existing, so it passes like in the field-precision runner).
#
# Usage: tools/idxref-runner.sh [out-dir]
#   out-dir defaults to /tmp/idxref-run (recreated each run).
WT=$(cd "$(dirname "$0")/.." && pwd)
PARENT=$(dirname "$WT")
OUT=${1:-/tmp/idxref-run}
[ -f "$PARENT/env.sh" ] && source "$PARENT/env.sh"
export PATH="/root/.cargo/bin:$HOME/.cargo/bin:$PATH"
export RUSTUP_TOOLCHAIN=nightly-2026-09-28
SYSROOT=$(rustc --print sysroot)
SARS=$WT/rsc/driver/target/sars
MIR2SA=$WT/rsc/target/debug/mir2sa
if [ -x "$PARENT/sci/zig-out/bin/sa" ]; then SA=$PARENT/sci/zig-out/bin/sa; else SA=sa; fi

(cd "$WT/rsc" && cargo build -j 2 -p mir2sa) || { echo "mir2sa build failed"; exit 1; }
if [ ! -x "$SARS" ] || [ -n "$(find "$WT/rsc/driver" -maxdepth 1 -name '*.rs' -newer "$SARS" -print -quit 2>/dev/null)" ]; then
  (cd "$WT/rsc/driver" && RUSTUP_TOOLCHAIN=nightly-2026-09-28 bash build.sh) || { echo "sars build failed"; exit 1; }
fi

rm -rf "$OUT"; mkdir -p "$OUT"
pass=0; fail=0
for f in "$WT"/tools/idxref-corpus/*.rs; do
  n=$(basename "$f" .rs); d="$OUT/$n"; mkdir -p "$d"
  rustc --edition 2021 -o "$d/native" "$f" 2>"$d/native.log"; nrc=$?
  if [ $nrc -eq 0 ]; then "$d/native" >/dev/null 2>&1; native_rc=$?; else native_rc="rustc-fail"; fi
  LD_LIBRARY_PATH="$SYSROOT/lib" "$SARS" --edition 2021 "$f" --rsc-out "$d/out.mir.json" --crate-type bin >"$d/sars.log" 2>&1; src=$?
  if [ $src -ne 0 ]; then echo "$n FAIL sars_rc=$src native=$native_rc"; fail=$((fail+1)); continue; fi
  "$MIR2SA" lower "$d/out.mir.json" --out "$d/out.sa" >"$d/mir2sa.log" 2>&1; mrc=$?
  python3 "$WT/tools/fieldprec-transform.py" "$d/out.sa" "$d/run.sa"
  timeout 60 "$SA" check --no-incremental "$d/run.sa" >"$d/check.log" 2>&1; crc=$?
  timeout 300 "$SA" build-exe --no-incremental "$d/run.sa" -o "$d/runbin" >"$d/build.log" 2>&1; brc=$?
  if [ $brc -eq 0 ]; then "$d/runbin" >/dev/null 2>&1; rrc=$?; else rrc="build-fail"; fi
  loud=$(grep -o "UNSUPPORTED=[0-9]*" "$d/mir2sa.log" | head -1 | grep -o "[0-9]*")
  loud=${loud:-0}
  kinds=$(grep "UNSUPPORTED:" "$d/mir2sa.log" | sed 's/.*UNSUPPORTED: //' | awk '{print $NF}' | sed 's/.*://' | sort -u | tr '\n' ',' | sed 's/,$//')
  if [ "$native_rc" = "$rrc" ] && [ "$crc" -eq 0 ] && [ "$mrc" -eq 0 ] && { [ "$loud" -eq 0 ] || [ "$kinds" = "OverflowAssertFolded" ]; }; then
    if [ -n "$kinds" ]; then echo "$n PASS native=$native_rc sa=$rrc loud=$kinds"; else echo "$n PASS native=$native_rc sa=$rrc"; fi
    pass=$((pass+1))
  else
    echo "$n FAIL native=$native_rc sa=$rrc check=$crc build=$brc mir2sa=$mrc UNSUPPORTED=$loud kinds=$kinds"
    fail=$((fail+1))
  fi
done
echo "idxref: PASS=$pass FAIL=$fail (out: $OUT)"
[ "$fail" -eq 0 ]
