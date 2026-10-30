#!/usr/bin/env bash
# Field-precision value tests (sarust field-precision branch).
#
# Corpus contract: every case under tools/fieldprec-corpus/ defines
# `fn probe() -> u64` with a distinct expected value below 256, so the
# process exit code is the observable. Per case the runner compares
# the native rustc binary's exit code against the sa build-exe binary's
# exit code. PASS requires all three: native exit == SA exit,
# `sa check` clean, and mir2sa UNSUPPORTED=0 (a loud case is reported
# as FAIL here, never as a silent pass; a case that is honestly loud
# by design is documented in the task report, not in this script).
#
# One exception, by design: OverflowAssertFolded. Every integer `+`
# lowers to a checked helper (which traps on overflow, so the VALUE
# is exact) plus an overflow assert that mir2sa folds because the
# (value, flag) pair is collapsed to the value alone. The fold is
# counted loudly so it stays visible, but it is value-preserving and
# pre-existing -- not a field-precision gap. A case whose only loud
# kinds are OverflowAssertFolded therefore PASSES, with the kind
# printed on its line; any other loud kind fails the case.
#
# Binaries: mir2sa is always rebuilt first (cheap, guarantees the
# binary matches the source); sars is rebuilt only when a driver
# source is newer than the binary. Toolchain is pinned to
# nightly-2026-09-28 (the driver binds rustc_private).
#
# Usage: tools/fieldprec-runner.sh [out-dir]
#   out-dir defaults to /tmp/fieldprec-run (recreated each run).
WT=$(cd "$(dirname "$0")/.." && pwd)
PARENT=$(dirname "$WT")
OUT=${1:-/tmp/fieldprec-run}
[ -f "$PARENT/env.sh" ] && source "$PARENT/env.sh"
export PATH="/root/.cargo/bin:$HOME/.cargo/bin:$PATH"
export RUSTUP_TOOLCHAIN=nightly-2026-09-28
SYSROOT=$(rustc --print sysroot)
SARS=$WT/rsc/driver/target/sars
MIR2SA=$WT/rsc/target/debug/mir2sa
if [ -x "$PARENT/sci/zig-out/bin/sa" ]; then SA=$PARENT/sci/zig-out/bin/sa; else SA=sa; fi

(cd "$WT/rsc" && cargo build -p mir2sa) || { echo "mir2sa build failed"; exit 1; }
if [ ! -x "$SARS" ] || [ -n "$(find "$WT/rsc/driver" -maxdepth 1 -name '*.rs' -newer "$SARS" -print -quit 2>/dev/null)" ]; then
  (cd "$WT/rsc/driver" && RUSTUP_TOOLCHAIN=nightly-2026-09-28 bash build.sh) || { echo "sars build failed"; exit 1; }
fi

rm -rf "$OUT"; mkdir -p "$OUT"
pass=0; fail=0
for f in "$WT"/tools/fieldprec-corpus/*.rs; do
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
    echo "$n FAIL native=$native_rc sa=$rrc check=$crc build=$brc mir2sa=$mrc UNSUPPORTED=$loud kinds=$kinds"; fail=$((fail+1))
  fi
done
echo "fieldprec: PASS=$pass FAIL=$fail (out: $OUT)"
[ "$fail" -eq 0 ]
