#!/usr/bin/env bash
# tools/test-module-tree.sh — end-to-end tests for stream 8 (module tree
# + multi-crate linking). Runs both fixtures through build-project.sh
# and asserts sa check passes, collision renames land, and no duplicate
# definitions survive. Also asserts the duplicate-entry-main link fails
# loudly instead of silently picking a main.
#
# Work dir: ${TEST_WORK:-/tmp/sarust-module-tree-test} (this script's own
# scratch; it removes and recreates it). Every checked command saves
# its own rc; a failed assertion prints FAIL and exits 1.
#
# Usage: tools/test-module-tree.sh

set -uo pipefail

SARUST_DIR="$(cd "$(dirname "$0")/.." && pwd)"
WORK="${TEST_WORK:-/tmp/sarust-module-tree-test}"
rm -rf "$WORK"
mkdir -p "$WORK" || exit 1

FAIL=0
ok() { echo "PASS: $1"; }
bad() { echo "FAIL: $1" >&2; FAIL=1; }
check_rc() { # $1=label $2=rc $3=want(0 or nonzero)
  if [ "$3" = "0" ]; then
    [ "$2" -eq 0 ] && ok "$1 rc=0" || bad "$1 rc=$2 (want 0)"
  else
    [ "$2" -ne 0 ] && ok "$1 rc=$2 (nonzero as expected)" || bad "$1 rc=0 (want nonzero)"
  fi
}
has() { # $1=label $2=file $3=pattern
  if grep -q "$3" "$2" 2>/dev/null; then ok "$1"; else bad "$1 (missing $3 in $2)"; fi
}
not_has() { # $1=label $2=file $3=pattern
  if grep -q "$3" "$2" 2>/dev/null; then bad "$1 (unexpected $3 in $2)"; else ok "$1"; fi
}
count_is() { # $1=label $2=file $3=pattern $4=want
  n="$(grep -c "$3" "$2" 2>/dev/null || true)"
  [ "$n" = "$4" ] && ok "$1 (count=$n)" || bad "$1 (count=$n, want $4)"
}

# --- Fixture 1: multi-file mod tree, same fn name in two modules -------
"$SARUST_DIR/tools/build-project.sh" \
  --crate modtree_demo="$SARUST_DIR/examples/module-tree/src/main.rs" \
  --bin modtree_demo --out-dir "$WORK/modtree" --out "$WORK/modtree/linked.sa" \
  >"$WORK/modtree.build.log" 2>&1
rc=$?
check_rc "modtree build-project" "$rc" 0
MT="$WORK/modtree/linked.sa"
if [ "$rc" -eq 0 ]; then
  has "modtree m1 compute qualified" "$MT" '^@modtree_demo__m1__compute('
  has "modtree m2 compute qualified" "$MT" '^@modtree_demo__m2__compute('
  has "modtree m1 call rewritten" "$MT" 'call @modtree_demo__m1__compute('
  has "modtree m2 call rewritten" "$MT" 'call @modtree_demo__m2__compute('
  has "modtree nested mod triple kept" "$MT" '^@triple('
  count_is "modtree exactly one main" "$MT" '^@main(' 1
  not_has "modtree no duplicate bare compute" "$MT" '^@compute('
fi

# --- Fixture 2: multi-crate path dependency + cross-crate collision ----
"$SARUST_DIR/tools/build-project.sh" \
  --crate mylib="$SARUST_DIR/examples/multi-crate/mylib/src/lib.rs" \
  --crate mybin="$SARUST_DIR/examples/multi-crate/mybin/src/main.rs" \
  --bin mybin --out-dir "$WORK/multicrate" --out "$WORK/multicrate/linked.sa" \
  >"$WORK/multicrate.build.log" 2>&1
rc=$?
check_rc "multicrate build-project" "$rc" 0
MC="$WORK/multicrate/linked.sa"
if [ "$rc" -eq 0 ]; then
  has "multicrate lib helper qualified" "$MC" '^@mylib__helper('
  has "multicrate bin helper qualified" "$MC" '^@mybin__helper('
  has "multicrate lib-helper call rewritten" "$MC" 'call @mylib__helper('
  has "multicrate bin-helper call rewritten" "$MC" 'call @mybin__helper('
  has "multicrate triple call normalized" "$MC" 'call @triple('
  not_has "multicrate no Debug-fallback stub call" "$MC" 'call @mylib_triple('
  count_is "multicrate triple defined once" "$MC" '^@triple(' 1
  count_is "multicrate exactly one main" "$MC" '^@main(' 1
  not_has "multicrate no duplicate bare helper" "$MC" '^@helper('
  not_has "multicrate crate-mode feature off" "$MC" '^@fast_mode('
fi

# --- Fixture 2b: same project via a stream 7 build plan ---------------
# The plan JSON below mirrors stream 7's schema_version 1 exactly
# (docs/cargo-plan.md, branch stream/cargo-toml), including one entry
# in each gap section to prove display-before-build, and a cfg-gated
# feature on mylib to prove cfg_args splicing.
python3 - "$SARUST_DIR" "$WORK" <<'PYEOF'
import json, sys
S, W = sys.argv[1], sys.argv[2]
plan = {
  "schema_version": 1,
  "root_manifest": f"{S}/examples/multi-crate/mybin/Cargo.toml",
  "root_crate": "mybin",
  "crates": [
    {"name": "mylib", "manifest_dir": f"{S}/examples/multi-crate/mylib",
     "manifest_path": f"{S}/examples/multi-crate/mylib/Cargo.toml",
     "edition": "2021",
     "lib": {"name": "mylib", "path": f"{S}/examples/multi-crate/mylib/src/lib.rs", "crate_type": "lib"},
     "bins": [], "enabled_features": ["fast"], "cfg_args": ["--cfg", 'feature="fast"'],
     "edge_kind": "normal", "dependencies": []},
    {"name": "mybin", "manifest_dir": f"{S}/examples/multi-crate/mybin",
     "manifest_path": f"{S}/examples/multi-crate/mybin/Cargo.toml",
     "edition": "2021", "lib": None,
     "bins": [{"name": "mybin", "path": f"{S}/examples/multi-crate/mybin/src/main.rs", "crate_type": "bin"}],
     "enabled_features": [], "cfg_args": [], "edge_kind": "root",
     "dependencies": [{"name": "mylib", "path": "../mylib",
       "resolved_path": f"{S}/examples/multi-crate/mylib",
       "optional": False, "default_features": True, "features": [],
       "kind": "normal", "workspace_inherited": False}]},
  ],
  "enabled_features": [], "cfg_args": [],
  "unsupported": ["crates.io dependency 'serde' of 'mybin' (version req 1) -- plan-test gap"],
  "warnings": ["plan-test warning"],
  "excluded_target_deps": ["win-dep (target 'cfg(windows)', from mybin)"],
}
open(f"{W}/plan.json", "w").write(json.dumps(plan, indent=2))
PYEOF
"$SARUST_DIR/tools/build-project.sh" \
  --plan "$WORK/plan.json" \
  --out-dir "$WORK/plan" --out "$WORK/plan/linked.sa" \
  >"$WORK/plan.build.log" 2>&1
rc=$?
check_rc "plan-mode build-project" "$rc" 0
if [ "$rc" -eq 0 ]; then
  first_build="$(grep -n -m1 '^crate ' "$WORK/plan.build.log" | cut -d: -f1)"
  for pat in "plan unsupported" "plan warnings" "plan excluded_target_deps"; do
    ln="$(grep -n -m1 "$pat" "$WORK/plan.build.log" | cut -d: -f1)"
    if [ -n "$ln" ] && [ -n "$first_build" ] && [ "$ln" -lt "$first_build" ]; then
      ok "plan $pat displayed before build (line $ln < $first_build)"
    else
      bad "plan $pat displayed before build (line ${ln:-?} vs first build ${first_build:-?})"
    fi
  done
  PL="$WORK/plan/linked.sa"
  has "plan-mode triple kept" "$PL" '^@triple('
  has "plan-mode lib helper qualified" "$PL" '^@mylib__helper('
  has "plan-mode bin helper qualified" "$PL" '^@mybin__helper('
  has "plan-mode cfg-gated fast fn present" "$PL" '^@fast_mode('
  count_is "plan-mode exactly one main" "$PL" '^@main(' 1
fi

# --- Negative: two entry mains must fail loudly, not pick one ---------
if [ -f "$WORK/modtree/0_modtree_demo.mir.json" ] && [ -f "$WORK/multicrate/1_mybin.mir.json" ]; then
  "$SARUST_DIR/rsc/target/debug/mir2sa" link \
    "$WORK/modtree/0_modtree_demo.mir.json" "$WORK/multicrate/1_mybin.mir.json" \
    --out "$WORK/neg.sa" >"$WORK/neg.log" 2>&1
  rc=$?
  check_rc "duplicate-main link rejected" "$rc" nonzero
  if grep -q "duplicate entry-point main" "$WORK/neg.log" 2>/dev/null; then
    ok "duplicate-main error message loud"
  else
    bad "duplicate-main error message loud"
  fi
fi

if [ "$FAIL" -eq 0 ]; then echo "test-module-tree: ALL PASS"; else echo "test-module-tree: FAILURES PRESENT" >&2; exit 1; fi
