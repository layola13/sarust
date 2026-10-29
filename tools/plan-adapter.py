#!/usr/bin/env python3
"""tools/plan-adapter.py — translate a stream 7 JSON build plan into the
build-entry arrays tools/build-project.sh consumes.

Input: a `mir2sa cargo --out plan.json` file (schema_version 1, see
docs/cargo-plan.md on branch stream/cargo-toml). Output: a bash snippet
defining PLAN_* arrays (written to --out, shell-quoted with shlex).
Gap sections (unsupported / warnings / excluded_target_deps) are
printed to stdout FIRST, before build-project.sh runs any build step;
build-project.sh calls this adapter before its build loop, so the
display-before-build requirement holds by construction.

Build-entry rules (see tools/module-tree-README.md for rationale):
- crates are walked in plan order (dependencies first, root last).
- Every lib becomes an rlib entry (built with real rustc, then sars);
  every earlier rlib is --extern'd into later crates.
- Only the ROOT crate's bins are link targets. Non-root bins are
  skipped with a printed note (a dependency's bins are never linked).
  A non-root crate with no lib is a loud error (nothing linkable).
- Root bin selection: --bin <target name> selects one; with no --bin,
  a single root bin is used; zero root bins means lib-only link; more
  than one root bin without --bin is a loud error (never a silent pick).
- edge_kind is advisory only: build-edge libs are still built and
  --extern'd (harmless: crate code cannot import a build-only dep, and
  if it tries, rustc reports it loudly). This avoids misclassifying a
  crate reached via both a normal and a build edge.
- --crate-name values are sanitized ('-' -> '_'), matching Cargo's own
  normalization; the same sanitized name feeds both the rustc rlib
  build and the sars run, so cross-crate call paths resolve.
- explicit lib crate-type is noted but does not change the build: the
  rlib is only needed for --extern typechecking and for sars MIR.

Exit 2 on any validation failure (bad schema, unreadable paths,
ambiguous bin selection); nothing is built in that case.
"""

import json
import shlex
import sys
from pathlib import Path


def fail(msg):
    print(f"plan-adapter: error: {msg}", file=sys.stderr)
    sys.exit(2)


def sh_array(name, items):
    return f"{name}=({''.join(' ' + shlex.quote(str(x)) for x in items)})\n"


def main(argv):
    plan_path = None
    out_path = None
    want_bin = None
    i = 0
    while i < len(argv):
        if argv[i] == "--plan":
            i += 1
            plan_path = argv[i]
        elif argv[i] == "--out":
            i += 1
            out_path = argv[i]
        elif argv[i] == "--bin":
            i += 1
            want_bin = argv[i]
        else:
            fail(f"unknown arg: {argv[i]}")
        i += 1
    if not plan_path or not out_path:
        fail("usage: plan-adapter.py --plan <plan.json> --out <snippet.sh> [--bin <name>]")
    try:
        plan = json.loads(Path(plan_path).read_text())
    except (OSError, json.JSONDecodeError) as e:
        fail(f"cannot read plan {plan_path}: {e}")
    if plan.get("schema_version") != 1:
        fail(f"unsupported schema_version: {plan.get('schema_version')!r} (want 1)")
    crates = plan.get("crates")
    if not isinstance(crates, list) or not crates:
        fail("plan has no crates list")
    root = crates[-1]
    root_crate = plan.get("root_crate")
    if root_crate is not None and root.get("name") != root_crate:
        fail(f"root crate mismatch: last crate is {root.get('name')!r}, "
             f"root_crate is {root_crate!r}")

    # --- gap sections: printed FIRST, before any build step ---
    def section(title, items):
        print(f"plan {title} ({len(items)}):")
        if items:
            for it in items:
                print(f"  - {it}")
        else:
            print("  (none)")

    print(f"plan: {plan_path} schema_version=1 root_crate={root.get('name')} "
          f"crates={len(crates)}")
    section("unsupported", plan.get("unsupported", []))
    section("warnings", plan.get("warnings", []))
    section("excluded_target_deps", plan.get("excluded_target_deps", []))

    names, kinds, roots, editions, packages, bin_names = [], [], [], [], [], []
    cfgs = []
    notes = []
    for ci, crate in enumerate(crates):
        is_root = ci == len(crates) - 1
        cname = crate.get("name")
        lib = crate.get("lib")
        bins = crate.get("bins", [])
        edition = crate.get("edition", "2021")
        cfg = crate.get("cfg_args", [])
        if lib is not None and lib.get("explicit_crate_type"):
            notes.append(
                f"crate {cname}: explicit lib crate-type "
                f"{lib['explicit_crate_type']} noted; building as rlib for "
                f"--extern typechecking and sars MIR"
            )
        if is_root:
            if lib is not None:
                names.append(lib["name"].replace("-", "_"))
                kinds.append("rlib")
                roots.append(lib["path"])
                editions.append(edition)
                packages.append(cname)
                bin_names.append("")
                cfgs.append(cfg)
            if want_bin is not None:
                sel = [b for b in bins if b.get("name") == want_bin]
                if len(sel) != 1:
                    fail(f"root bin {want_bin!r} not found uniquely; "
                         f"root bins: {[b.get('name') for b in bins]}")
                bins = sel
            elif len(bins) > 1:
                fail(f"root crate {cname!r} has {len(bins)} bins "
                     f"{[b.get('name') for b in bins]}; pass --bin <name> "
                     f"to pick one (no silent pick)")
            for b in bins:
                names.append(b["name"].replace("-", "_"))
                kinds.append("bin")
                roots.append(b["path"])
                editions.append(edition)
                packages.append(cname)
                bin_names.append(b["name"])
                cfgs.append(cfg)
        else:
            if lib is None:
                fail(f"crate {cname!r} is a dependency but has no lib "
                     f"target; its bins (if any) are not linkable")
            names.append(lib["name"].replace("-", "_"))
            kinds.append("rlib")
            roots.append(lib["path"])
            editions.append(edition)
            packages.append(cname)
            bin_names.append("")
            cfgs.append(cfg)
            if bins:
                notes.append(
                    f"crate {cname}: skipping non-root bin targets "
                    f"{[b.get('name') for b in bins]} (dependency bins "
                    f"are never linked)"
                )
    for n in notes:
        print(f"plan note: {n}")
    for r in roots:
        if not Path(r).is_file():
            fail(f"crate root not found: {r}")
    if not names:
        fail("plan produced no build entries")
    if "bin" not in kinds:
        print("plan note: root crate has no bin; linking lib-only output "
              "(no entry-point main)")
    snippet = []
    snippet.append(sh_array("PLAN_NAMES", names))
    snippet.append(sh_array("PLAN_KINDS", kinds))
    snippet.append(sh_array("PLAN_ROOTS", roots))
    snippet.append(sh_array("PLAN_EDITIONS", editions))
    snippet.append(sh_array("PLAN_CRATES", packages))
    snippet.append(sh_array("PLAN_BIN_NAMES", bin_names))
    for idx, cfg in enumerate(cfgs):
        snippet.append(sh_array(f"PLAN_CFG_{idx}", cfg))
    snippet.append(f"PLAN_COUNT={len(names)}\n")
    Path(out_path).write_text("".join(snippet))
    print(f"plan entries: {len(names)} "
          f"({kinds.count('rlib')} rlib, {kinds.count('bin')} bin)")


if __name__ == "__main__":
    main(sys.argv[1:])
