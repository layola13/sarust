#!/usr/bin/env bash
# Stream 7 (cargo-toml) end-to-end checks. Proves, against the real
# toolchain, that a Cargo plan drives sars -> mir2sa -> sa check:
#   A. multi-crate path dependency (foo lib + app bin, --extern via a
#      rustc-built rlib; cross-crate LINKING rules are stream 8's job,
#      this only proves the plan + per-crate pipeline)
#   B. feature switch: same crate with / without --cfg feature="fast"
#      must both check, and the lowered .sa must differ at the gated value
#   C. negative: an unknown --features entry must fail (rc != 0), never
#      silently build the wrong cfg set
#
# Prerequisites (built beforehand, same as tools/sweep.sh):
#   rsc/driver/target/sars, rsc/target/debug/mir2sa, sa via env.sh
# Outputs live only under $CARGO_E2E_WORK_ROOT (default /tmp/sarust-cargo-e2e).
# Every tested command saves its own $?; a failure is reported, never hidden.
#
# Usage: tools/cargo-e2e.sh
set -uo pipefail

SARUST_DIR="$(cd "$(dirname "$0")/.." && pwd)"
FIXTURES="$SARUST_DIR/rsc/mir2sa/tests/fixtures"
SARS_BIN="$SARUST_DIR/rsc/driver/target/sars"
MIR2SA_BIN="$SARUST_DIR/rsc/target/debug/mir2sa"
ENV_SH="/content/sa_all/env.sh"
WORK_ROOT="${CARGO_E2E_WORK_ROOT:-/tmp/sarust-cargo-e2e}"

export PATH="$HOME/.cargo/bin:$PATH"
if [ -f "$ENV_SH" ]; then
  # shellcheck disable=SC1090
  source "$ENV_SH"
fi
SA_BIN="$(command -v sa || true)"
[ -z "$SA_BIN" ] && SA_BIN="/content/sa_all/sci/zig-out/bin/sa"

PASS=0
FAIL=0
ok()   { echo "PASS: $1"; PASS=$((PASS + 1)); }
bad()  { echo "FAIL: $1"; FAIL=$((FAIL + 1)); }

for bin in "$SARS_BIN" "$MIR2SA_BIN" "$SA_BIN"; do
  if [ ! -x "$bin" ]; then echo "missing executable: $bin (build first)" >&2; exit 1; fi
done
SYSROOT="$(RUSTUP_TOOLCHAIN=nightly-2026-09-28 rustc --print sysroot 2>/dev/null || true)"
[ -z "$SYSROOT" ] && { echo "cannot determine rustc sysroot" >&2; exit 1; }

mkdir -p "$WORK_ROOT"
APP_DIR="$FIXTURES/cargo-path-dep/app"
FOO_DIR="$FIXTURES/cargo-path-dep/foo"
FEAT_DIR="$FIXTURES/cargo-feature"

# --- A1: plan for the path-dep project ---------------------------------------
"$MIR2SA_BIN" cargo "$APP_DIR" --out "$WORK_ROOT/plan.json" >"$WORK_ROOT/plan.log" 2>&1
rc=$?
if [ "$rc" -ne 0 ]; then bad "A1 plan rc=$rc"; else
  python3 - "$WORK_ROOT/plan.json" <<'PYEOF'
import json, sys
d = json.load(open(sys.argv[1]))
names = [c["name"] for c in d["crates"]]
assert names == ["foo", "app"], names
assert d["root_crate"] == "app"
assert d["unsupported"] == [], d["unsupported"]
assert d["crates"][1]["dependencies"][0]["resolved_path"].endswith("foo")
print("plan ok: crates=%s" % names)
PYEOF
  rc=$?
  if [ "$rc" -eq 0 ]; then ok "A1 path-dep plan (crates=[foo, app], unsupported=0)"; else bad "A1 plan content check failed"; fi
fi

# --- A2: rustc rlib for foo, then sars app with --extern ---------------------
RUSTUP_TOOLCHAIN=nightly-2026-09-28 rustc --edition 2021 --crate-type rlib \
  "$FOO_DIR/src/lib.rs" --crate-name foo -o "$WORK_ROOT/libfoo.rlib" \
  >"$WORK_ROOT/rlib.log" 2>&1
rc=$?
[ "$rc" -eq 0 ] && ok "A2 rustc rlib for foo" || bad "A2 rustc rlib rc=$rc"

LD_LIBRARY_PATH="$SYSROOT/lib" "$SARS_BIN" --edition 2021 "$APP_DIR/src/main.rs" \
  --crate-type bin --extern "foo=$WORK_ROOT/libfoo.rlib" \
  --rsc-out "$WORK_ROOT/app.mir.json" >"$WORK_ROOT/app.sars.log" 2>&1
rc=$?
[ "$rc" -eq 0 ] && ok "A2 sars app with --extern foo" || bad "A2 sars app rc=$rc"

"$MIR2SA_BIN" lower "$WORK_ROOT/app.mir.json" --out "$WORK_ROOT/app.sa" \
  >"$WORK_ROOT/app.mir2sa.log" 2>&1
rc=$?
loud="$(sed -n 's/^wrote .* UNSUPPORTED=\([0-9][0-9]*\).*/\1/p' "$WORK_ROOT/app.mir2sa.log" | head -1)"
[ "$rc" -eq 0 ] && ok "A2 mir2sa lower app (UNSUPPORTED=${loud:-?})" || bad "A2 mir2sa rc=$rc"

"$SA_BIN" check --no-incremental "$WORK_ROOT/app.sa" >"$WORK_ROOT/app.sa.log" 2>&1
rc=$?
[ "$rc" -eq 0 ] && ok "A2 sa check app" || bad "A2 sa check rc=$rc"

# --- B1: feature plans --------------------------------------------------------
"$MIR2SA_BIN" cargo "$FEAT_DIR" --features fast --out "$WORK_ROOT/plan-fast.json" \
  >"$WORK_ROOT/plan-fast.log" 2>&1
rc=$?
[ "$rc" -eq 0 ] && ok "B1 plan --features fast" || bad "B1 plan fast rc=$rc"
"$MIR2SA_BIN" cargo "$FEAT_DIR" --out "$WORK_ROOT/plan-default.json" \
  >"$WORK_ROOT/plan-default.log" 2>&1
rc=$?
[ "$rc" -eq 0 ] && ok "B1 plan default" || bad "B1 plan default rc=$rc"
python3 - "$WORK_ROOT/plan-fast.json" "$WORK_ROOT/plan-default.json" <<'PYEOF'
import json, sys
fast = json.load(open(sys.argv[1]))
default = json.load(open(sys.argv[2]))
assert fast["enabled_features"] == ["fast"], fast["enabled_features"]
assert fast["cfg_args"] == ["--cfg", 'feature="fast"'], fast["cfg_args"]
assert default["enabled_features"] == [], default["enabled_features"]
assert default["cfg_args"] == [], default["cfg_args"]
print("feature plans ok")
PYEOF
rc=$?
[ "$rc" -eq 0 ] && ok "B1 plan cfg_args differ (fast vs default)" || bad "B1 plan cfg content"

# --- B2: both cfg variants must check, and the .sa must differ ---------------
for variant in fast default; do
  if [ "$variant" = "fast" ]; then cfg=(--cfg 'feature="fast"'); else cfg=(); fi
  LD_LIBRARY_PATH="$SYSROOT/lib" "$SARS_BIN" --edition 2021 "$FEAT_DIR/src/main.rs" \
    --crate-type bin "${cfg[@]}" \
    --rsc-out "$WORK_ROOT/feat-$variant.mir.json" >"$WORK_ROOT/feat-$variant.sars.log" 2>&1
  rc=$?
  [ "$rc" -eq 0 ] || { bad "B2 sars feat-$variant rc=$rc"; continue; }
  "$MIR2SA_BIN" lower "$WORK_ROOT/feat-$variant.mir.json" --out "$WORK_ROOT/feat-$variant.sa" \
    >"$WORK_ROOT/feat-$variant.mir2sa.log" 2>&1
  rc=$?
  [ "$rc" -eq 0 ] || { bad "B2 mir2sa feat-$variant rc=$rc"; continue; }
  "$SA_BIN" check --no-incremental "$WORK_ROOT/feat-$variant.sa" >"$WORK_ROOT/feat-$variant.sa.log" 2>&1
  rc=$?
  [ "$rc" -eq 0 ] && ok "B2 sa check feat-$variant" || bad "B2 sa check feat-$variant rc=$rc"
done
diff -q "$WORK_ROOT/feat-fast.sa" "$WORK_ROOT/feat-default.sa" >/dev/null 2>&1
rc=$?
[ "$rc" -eq 1 ] && ok "B2 feat .sa differs (cfg gate changes output)" || bad "B2 feat .sa diff rc=$rc (want 1)"

# --- C: unknown feature must fail --------------------------------------------
"$MIR2SA_BIN" cargo "$FEAT_DIR" --features nope --out "$WORK_ROOT/plan-nope.json" \
  >"$WORK_ROOT/plan-nope.log" 2>&1
rc=$?
[ "$rc" -ne 0 ] && ok "C unknown feature rejected (rc=$rc)" || bad "C unknown feature accepted (must fail)"

echo "cargo-e2e: PASS=$PASS FAIL=$FAIL work=$WORK_ROOT"
[ "$FAIL" -eq 0 ]
