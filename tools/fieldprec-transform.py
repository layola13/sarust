#!/usr/bin/env python3
"""Field-precision runner transform: mir2sa output -> runnable .sa.

Corpus contract: every case defines `fn probe() -> u64`.
- Drops the generated @main (the Rust main calls process::exit, which
  mir2sa cannot reproduce; the wrapper below returns probe()'s value
  as the process exit code instead).
- Drops @extern lines for functions defined in the file (otherwise a
  locally defined function is treated as an unresolved extern).
- Appends a wrapper @main that calls @probe and returns its value.
- Deliberately NO return surgery: mir2sa itself must emit
  `return _0` when the return place is bound (see lower.rs Return
  arm); a transform that patched returns would hide a mir2sa bug.
"""
import re
import sys

src = open(sys.argv[1]).read()
funcs = []
pre = []
cur = None
for line in src.splitlines():
    m = re.match(r"^@([A-Za-z_][\w]*)\((.*)\) -> (\w+):$", line)
    if m:
        if cur:
            funcs.append(cur)
        cur = (m.group(1), m.group(3), [line])
    elif cur is not None:
        cur[2].append(line)
    else:
        pre.append(line)
if cur:
    funcs.append(cur)
kept = []
for (name, ret, lines) in funcs:
    if name == "main":
        continue
    kept.append((name, ret, lines))
defined = set(nm for (nm, rt, fn) in kept)
pre2 = []
for l in pre:
    m = re.match(r"^@extern ([A-Za-z_][\w]*)\(", l)
    if m and m.group(1) in defined:
        continue
    pre2.append(l)
body = "\n".join(pre2 + [l for (nm, rt, fn) in kept for l in fn])
body += "\n@main() -> u64:\n    _r = call @probe()\n    return _r\n"
open(sys.argv[2], "w").write(body)
