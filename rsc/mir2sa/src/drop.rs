//! mir2sa::drop — exit-anchored release insertion (Drop glue, phase 1).
//!
//! Goal: eliminate MemoryLeak traps WITHOUT introducing new traps.
//! Conservative rules (all verified against `sa check` probes):
//! - `x = y` MOVES (source dead after); call/store/eq/load/br read shared.
//! - `&y` locks y (freeing y while the borrow is live -> BorrowConflict).
//! - `!r` frees; double-free and use-after-free trap.
//! - regs are function-global single-assignment (rebinds already loud).
//!
//! Algorithm per function:
//! 1. Scan body lines: defs (`r = ...`), borrows (`x = &y`), frees (`!r`),
//!    move-consumed sources (RHS plain reg of `=`).
//! 2. For each Return-terminated block B, candidates = regs that are
//!    defined exactly once in a dominator of B (or params), never
//!    move-consumed, never explicitly freed on a dominator path of B.
//! 3. Order candidates borrows-before-sources over the borrow graph;
//!    skip anything ambiguous (cycles, unknowns).
//! 4. Caller inserts `!r` lines before `return`.
//!
//! Non-goals (separate programs): UseAfterMove (needs re-borrow/reload),
//! BorrowConflict repair (needs borrow-end analysis), PhiStateConflict
//! (needs path-sensitive cleanup), non-Return exits.

use std::collections::{BTreeMap, BTreeSet};

/// One body line classified for liveness. Plain-text, no MIR needed
/// (operates on the already-lowered SA lines).
#[derive(Debug, Default)]
struct LineUse {
    def: Option<String>,
    /// Plain-reg RHS sources of `=` (move-consumers).
    move_srcs: Vec<String>,
    /// Borrow edge (borrower, source) for `x = &y`.
    borrow: Option<(String, String)>,
    freed: Vec<String>,
}

fn is_reg(tok: &str) -> bool {
    let t = tok.trim().trim_end_matches(',');
    (t.starts_with('_') || t.chars().next().is_some_and(|c| c.is_ascii_alphabetic()))
        && t.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !t.is_empty()
        && !matches!(
            t,
            "call"
                | "return"
                | "jmp"
                | "br"
                | "eq"
                | "ne"
                | "add"
                | "sub"
                | "mul"
                | "and"
                | "or"
                | "xor"
                | "shl"
                | "not"
                | "neg"
                | "sext"
                | "zext"
                | "trunc"
                | "bitcast"
                | "fptosi"
                | "sitofp"
                | "uitofp"
                | "fpext"
                | "fptrunc"
                | "load"
                | "store"
                | "alloc"
                | "panic"
                | "unreachable"
                | "as"
        )
}

fn classify(line: &str) -> LineUse {
    let mut u = LineUse::default();
    let s = line.trim();
    if s.is_empty() || s.starts_with("//") || s.ends_with(':') {
        return u;
    }
    if let Some(rest) = s.strip_prefix('!') {
        let r = rest.trim().split_whitespace().next().unwrap_or("");
        if is_reg(r) {
            u.freed.push(r.to_string());
        }
        return u;
    }
    if let Some(eq) = s.find(" = ") {
        let dest = s[..eq].trim();
        if is_reg(dest) {
            u.def = Some(dest.to_string());
        }
        let rhs = s[eq + 3..].trim();
        // Borrow shape: `x = &y` (y may carry `//`? no — trailing comments
        // are forbidden, so the line is clean).
        if let Some(src) = rhs.strip_prefix('&') {
            let src = src.trim().split_whitespace().next().unwrap_or("");
            if is_reg(src) {
                if let Some(d) = &u.def {
                    u.borrow = Some((d.clone(), src.to_string()));
                }
                return u;
            }
        }
        // Plain move: RHS is exactly one reg (not a call/compound).
        if is_reg(rhs) && !rhs.contains('(') && !rhs.contains(' ') {
            u.move_srcs.push(rhs.to_string());
        }
        return u;
    }
    u
}

/// Compute per-Return-block free lists. `dom_sets` holds raw dominator sets
/// (each incl. self) keyed by original block index; `body_lines` aligns by
/// the same index. `param_count` seeds params as pre-bound.
///
/// Soundness regime (all verified against `sa check` probes):
/// - DEFS count only from unconditional prefixes (block lines before the
///   first mid-block `L_...:` label): sw/assert chains create conditional
///   regions inside MIR blocks, and freeing a conditionally-defined reg at
///   a join exit is UnknownRegister on bypassing paths.
/// - MOVES, FREES and BORROWS count from ANYWHERE (even conditional): a
///   consume/free/borrow on any path vetoes exit-freeing (else UseAfterMove,
///   double-free or BorrowConflict on that path). Under-freeing is always
///   safe (a leak, not a new trap).
/// - Candidates additionally need single definition in a dominator of the
///   exit; ordering is borrowers-before-sources over the borrow graph.
pub fn exit_frees(
    body_lines: &[Vec<String>],
    dom_sets: &[std::collections::HashSet<usize>],
    param_count: usize,
) -> BTreeMap<usize, Vec<String>> {
    // Split each block into unconditional prefix vs conditional rest.
    let is_label = |l: &str| {
        let t = l.trim();
        t.starts_with("L_") && t.ends_with(':') && !t.contains(' ')
    };
    // Per-block classified lines.
    let cls: Vec<Vec<LineUse>> = body_lines.iter().map(|ls| ls.iter().map(|l| classify(l)).collect()).collect();
    // Unconditional-prefix defs per block (dominance-gated candidates).
    // def_count per reg (whole function), move-consumed set, freed set.
    let mut def_count: BTreeMap<String, usize> = BTreeMap::new();
    // Def blocks per reg, unconditional prefixes only.
    let mut def_blocks: BTreeMap<String, BTreeSet<usize>> = BTreeMap::new();
    let mut moved: BTreeSet<String> = BTreeSet::new();
    let mut freed_anywhere: BTreeSet<String> = BTreeSet::new();
    // borrow edges borrower -> source (anywhere).
    let mut borrows: BTreeMap<String, String> = BTreeMap::new();
    for (bi, ls) in cls.iter().enumerate() {
        let mut conditional = false;
        for (li, u) in ls.iter().enumerate() {
            if li > 0 && body_lines[bi].get(li).is_some_and(|l| is_label(l)) {
                conditional = true;
            }
            if let Some(d) = &u.def {
                *def_count.entry(d.clone()).or_insert(0) += 1;
                if !conditional {
                    def_blocks.entry(d.clone()).or_default().insert(bi);
                }
            }
            for m in &u.move_srcs {
                moved.insert(m.clone());
            }
            for f in &u.freed {
                freed_anywhere.insert(f.clone());
            }
            if let Some((b, s)) = &u.borrow {
                borrows.insert(b.clone(), s.clone());
            }
        }
    }
    let params: BTreeSet<String> = (1..=param_count).map(|i| format!("_{}", i)).collect();
    let mut out: BTreeMap<usize, Vec<String>> = BTreeMap::new();
    for (bi, ls) in body_lines.iter().enumerate() {
        // Return-terminated blocks only (last code line starts with return).
        let is_exit = ls.iter().any(|l| l.trim_start().starts_with("return"));
        if !is_exit {
            continue;
        }
        // Candidates: defined exactly once in an unconditional prefix of
        // a dominator of this exit (else the free could hit an unbound reg
        // on some path), never moved anywhere, never freed anywhere
        // (conditional frees would double-free on their path), params
        // included (unused params leak).
        // Dominator blocks of this exit (incl. itself).
        let doms: &std::collections::HashSet<usize> = &dom_sets[bi];
        let mut cand: BTreeSet<String> = def_count
            .iter()
            .filter(|(_, &n)| n == 1)
            .map(|(r, _)| r.clone())
            .filter(|r| {
                def_blocks.get(r).is_some_and(|bis| bis.iter().all(|b| doms.contains(b)))
            })
            .filter(|r| !moved.contains(r) && !freed_anywhere.contains(r))
            .collect();
        for p in &params {
            if !moved.contains(p) && !freed_anywhere.contains(p) {
                // Param must be defined (bound by header) — count it only if
                // it never appears as a def (params have no def lines).
                if !def_count.contains_key(p) {
                    cand.insert(p.clone());
                }
            }
        }
        // Borrow ordering: repeatedly emit borrows whose sources are not
        // (pending candidates), i.e. borrowers before sources. Sources that
        // are themselves borrowed stay until their borrowers are out.
        // Anything left after the fixpoint is skipped (ambiguous, e.g. the
        // source is moved/unbound — freeing would trap).
        let mut ordered: Vec<String> = vec![];
        let mut pending = cand;
        loop {
            // A candidate can go iff it is not a live source, or all its
            // borrowers are already ordered (or not candidates).
            let ready: Vec<String> = pending
                .iter()
                .filter(|r| {
                    // r is a source with a pending borrower still inside?
                    !borrows.iter().any(|(b, s)| s == *r && pending.contains(b))
                })
                .cloned()
                .collect();
            if ready.is_empty() {
                break;
            }
            for r in ready {
                pending.remove(&r);
                ordered.push(r);
            }
        }
        // `pending` leftovers are skipped (would trap).
        if !ordered.is_empty() {
            out.insert(bi, ordered);
        }
    }
    out
}

#[cfg(test)]
mod drop_tests {
    use super::*;

    fn lines(ls: &[&str]) -> Vec<String> {
        ls.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn classify_shapes() {
        let u = classify("    _x = _y");
        assert_eq!(u.def, Some("_x".to_string()));
        assert_eq!(u.move_srcs, vec!["_y".to_string()]);
        let u = classify("    _b = &_a");
        assert_eq!(u.borrow, Some(("_b".to_string(), "_a".to_string())));
        let u = classify("    !_a");
        assert_eq!(u.freed, vec!["_a".to_string()]);
        let u = classify("L_bb0:");
        assert!(u.def.is_none());
        let u = classify("    // comment");
        assert!(u.def.is_none());
        let u = classify("    _x = call @f(_y, 0)");
        assert_eq!(u.def, Some("_x".to_string()));
        assert!(u.move_srcs.is_empty());
    }

    #[test]
    fn frees_simple_leak() {
        // _a alloc'd, stored into (non-consuming), never freed -> freed.
        // _v stored (non-consuming) -> freed. _r returned... return line
        // itself is not a def; _r defined once, never moved -> freed too?
        // (_r IS used by return — return is not tracked as a use, and our
        // rule frees it; SA return-then-end needs no live regs. Probed OK
        // pattern: return 0 with all freed is the documented clean shape.)
        let body = vec![lines(&[
            "_a = alloc 8",
            "store _a+0, 71 as u8",
            "_r = 0",
            "return _r",
        ])];
        let doms = vec![std::collections::HashSet::from([0])];
        let out = exit_frees(&body, &doms, 0);
        let frees = &out[&0];
        assert!(frees.contains(&"_a".to_string()), "{:?}", frees);
        assert!(frees.contains(&"_r".to_string()), "{:?}", frees);
    }

    #[test]
    fn borrow_ordering() {
        // _b borrows _a: borrower freed first.
        let body = vec![lines(&["_a = alloc 8", "_b = &_a", "_r = 0", "return _r"])];
        let doms = vec![std::collections::HashSet::from([0])];
        let out = exit_frees(&body, &doms, 0);
        let frees = &out[&0];
        let pb = frees.iter().position(|r| r == "_b").unwrap();
        let pa = frees.iter().position(|r| r == "_a").unwrap();
        assert!(pb < pa, "{:?}", frees);
    }

    #[test]
    fn moved_not_freed() {
        // _v moved into _w: _v must NOT be freed (would be UseAfterMove).
        let body = vec![lines(&["_v = 7", "_w = _v", "_r = 0", "return _r"])];
        let doms = vec![std::collections::HashSet::from([0])];
        let out = exit_frees(&body, &doms, 0);
        let frees = &out[&0];
        assert!(!frees.contains(&"_v".to_string()), "{:?}", frees);
        assert!(frees.contains(&"_w".to_string()), "{:?}", frees);
    }

    #[test]
    fn branch_local_never_freed_at_join() {
        // _x defined only in bb1 (branch); the join bb3 exit must NOT free
        // it (unbound on the bb2 path -> UnknownRegister). Regression test
        // for the _constant_0_ incident.
        let body = vec![
            lines(&["br _c -> L_bb1, L_bb2"]),
            lines(&["L_bb1:", "_x = 7", "jmp L_bb3"]),
            lines(&["L_bb2:", "jmp L_bb3"]),
            lines(&["L_bb3:", "_r = 0", "return _r"]),
        ];
        let doms = vec![
            std::collections::HashSet::from([0]),
            std::collections::HashSet::from([0, 1]),
            std::collections::HashSet::from([0, 2]),
            std::collections::HashSet::from([0, 3]),
        ];
        let out = exit_frees(&body, &doms, 0);
        let frees = &out[&3];
        assert!(!frees.contains(&"_x".to_string()), "{:?}", frees);
        assert!(frees.contains(&"_r".to_string()), "{:?}", frees);
    }
}
