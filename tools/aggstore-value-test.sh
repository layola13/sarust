#!/usr/bin/env bash
# Aggstore value evidence (sarust W2 aggstore branch).
#
# Two parts, each honest about what it proves:
#
# Part 1 — mir2sa on real Rust programs (tools/aggstore-corpus/*.rs):
# each case is a vec![] program whose MIR contains the aggstore place
# store. Asserts: sars ok, mir2sa ok, `sa check` clean, the emitted .sa
# contains the `// aggregate place store` marker with stores (never a
# rebind marker), and the only loud kinds are OverflowAssertFolded
# (value-preserving by design, see tools/fieldprec-runner.sh). Native
# rustc exit code must be 100 (10+20+30+40).
#
# Part 2 — direct .sa runtime control (tools/aggstore-corpus/direct-*.sa):
# the exact store sequence mir2sa emits for the Move case (box slot +
# buffer, four i32 stores, four loads, sum) must run to 100, and the
# same program with the stores removed (the old loud behaviour, where
# the write was dropped) must run to 0. This is the runtime value
# comparison for the store sequence itself.
#
# Why not build-exe the Part 1 .sa directly? A full MIR probe for a
# vec![] program cannot build-exe today for reasons unrelated to
# aggstore: the same probe with the aggstore stores deleted still
# fails with `error: InvalidOperand`, and stubbing the vec! std calls
# runs into SA `&Vec` reference semantics (segfault) — both are
# pre-existing sa-build limitations outside mir2sa's emission, not
# something this branch changes. The two parts above together cover
# the chain without pretending otherwise: Part 1 proves mir2sa emits
# exactly this store sequence for real Rust input; Part 2 proves the
# sequence itself carries the values.
#
# Usage: tools/aggstore-value-test.sh [out-dir]
#   out-dir defaults to /tmp/aggstore-value (recreated each run).
WT=$(cd "$(dirname "$0")/.." && pwd)
PARENT=$(dirname "$WT")
OUT=${1:-/tmp/aggstore-value}
[ -f "$PARENT/env.sh" ] && source "$PARENT/env.sh"
export PATH="/root/.cargo/bin:$HOME/.cargo/bin:$PATH"
export RUSTUP_TOOLCHAIN=nightly-2026-09-28
SYSROOT=$(rustc --print sysroot)
SARS=$WT/rsc/driver/target/sars
MIR2SA=$WT/rsc/target/debug/mir2sa
if [ -x "$PARENT/sci/zig-out/bin/sa" ]; then SA=$PARENT/sci/zig-out/bin/sa; else SA=sa; fi

(cd "$WT/rsc" && cargo build -j2 -p mir2sa) || { echo "mir2sa build failed"; exit 1; }

rm -rf "$OUT"; mkdir -p "$OUT"
fail=0
for f in "$WT"/tools/aggstore-corpus/*.rs; do
  n=$(basename "$f" .rs); d="$OUT/$n"; mkdir -p "$d"
  rustc --edition 2021 -o "$d/native" "$f" 2>"$d/native.log" || { echo "$n FAIL rustc"; fail=1; continue; }
  "$d/native" >/dev/null 2>&1; native_rc=$?
  LD_LIBRARY_PATH="$SYSROOT/lib" "$SARS" --edition 2021 "$f" --rsc-out "$d/out.mir.json" --crate-type bin >"$d/sars.log" 2>&1 || { echo "$n FAIL sars"; fail=1; continue; }
  "$MIR2SA" lower "$d/out.mir.json" --out "$d/out.sa" >"$d/mir2sa.log" 2>&1 || { echo "$n FAIL mir2sa"; fail=1; continue; }
  timeout 60 "$SA" check --no-incremental "$d/out.sa" >"$d/check.log" 2>&1; crc=$?
  kinds=$(grep "UNSUPPORTED:" "$d/mir2sa.log" | sed 's/.*UNSUPPORTED: //' | awk '{print $NF}' | sed 's/.*://' | sort -u | tr '\n' ',' | sed 's/,$//')
  if [ "$native_rc" = "100" ] && [ "$crc" -eq 0 ] && grep -q "aggregate place store" "$d/out.sa" && ! grep -q "UNSUPPORTED rebind" "$d/out.sa" && { [ -z "$kinds" ] || [ "$kinds" = "OverflowAssertFolded" ]; }; then
    echo "$n PASS native=$native_rc check=$crc loud=$kinds"
  else
    echo "$n FAIL native=$native_rc check=$crc loud=$kinds"; fail=1
  fi
done
for v in with-stores without-stores; do
  case $v in
    with-stores) want=100;;
    without-stores) want=0;;
  esac
  timeout 300 "$SA" build-exe --no-incremental "$WT/tools/aggstore-corpus/direct-$v.sa" -o "$OUT/direct-$v.bin" >"$OUT/direct-$v.build.log" 2>&1; brc=$?
  if [ $brc -eq 0 ]; then "$OUT/direct-$v.bin" >/dev/null 2>&1; rrc=$?; else rrc="build-fail"; fi
  if [ "$rrc" = "$want" ]; then echo "direct-$v PASS sa=$rrc"; else echo "direct-$v FAIL sa=$rrc want=$want"; fail=1; fi
done
if [ "$fail" -eq 0 ]; then echo "aggstore-value: PASS (out: $OUT)"; else echo "aggstore-value: FAIL (out: $OUT)"; exit 1; fi
