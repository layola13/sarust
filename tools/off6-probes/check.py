#!/usr/bin/env python3
"""off6 regression check: assert NO direct ptr field-0 operand in
BinOp / Call / Aggregate positions of an optimized-MIR mir.json.

Direct = a Copy/Move operand carrying a driver projection (pty key)
inside a BinOp, a Call terminator arg, or an Aggregate element.
Assign-Use positions are allowed (that is the materialization the
pipeline relies on): at least one Assign-Use with pty == "ptr" and
no off/off2/deref keys must be present (the ptr field-0 read itself).

Usage: python3 check.py <mir.json>
Exit 0 = materialized (residual unreachable); exit 1 = direct shape
found (residual has become reachable -> do NOT silently pass).
"""
import json, sys

def main(path):
    doc = json.load(open(path))
    direct = []
    materialized = []
    for func in doc.get("functions", []):
        fname = func.get("name")
        for blk in func.get("blocks", []):
            for st in blk.get("statements", []):
                if st.get("kind") != "Assign":
                    continue
                rv = st.get("rvalue", {}) or {}
                dest = st.get("dest")
                def is_proj(op):
                    return isinstance(op, dict) and op.get("kind") in ("Copy", "Move") and "pty" in op
                if rv.get("kind") == "Use" and is_proj(rv.get("op")):
                    op = rv["op"]
                    if op.get("pty") == "ptr" and "off" not in op and "off2" not in op and not op.get("deref"):
                        materialized.append("%s/%s Assign-Use %s %s" % (fname, blk.get("id"), dest, op))
                elif rv.get("kind") == "BinOp":
                    for side in ("left", "right"):
                        if is_proj(rv.get(side)):
                            direct.append("%s/%s BinOp-%s %s" % (fname, blk.get("id"), side, rv[side]))
                elif rv.get("kind") == "Aggregate":
                    for i, e in enumerate(rv.get("elems", [])):
                        if is_proj(e):
                            direct.append("%s/%s Agg-elem%d %s" % (fname, blk.get("id"), i, e))
            term = blk.get("terminator", {}) or {}
            if term.get("kind") == "Call":
                for i, a in enumerate(term.get("args", [])):
                    if isinstance(a, dict) and a.get("kind") in ("Copy", "Move") and "pty" in a:
                        direct.append("%s/%s Call-arg%d %s" % (fname, blk.get("id"), i, a))
    print("materialized ptr field-0 Assign-Use: %d" % len(materialized))
    for m in materialized:
        print("  " + m)
    if direct:
        print("DIRECT ptr field-0 operand(s): %d -- RESIDUAL REACHABLE" % len(direct))
        for d in direct:
            print("  " + d)
        return 1
    print("direct BinOp/Call/Aggregate ptr field-0 operands: 0 -- unreachable")
    if not materialized:
        print("WARNING: no ptr field-0 materialization found; probe may not exercise the shape")
        return 2
    return 0

if __name__ == "__main__":
    sys.exit(main(sys.argv[1]))
