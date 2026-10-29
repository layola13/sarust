//! mir2sa::borrow_end — borrow-end analysis for MIR Drop sites (phase 1).
//!
//! SA's Referee is lexical: `b = &p` locks `p` until `!b` is emitted. MIR
//! (NLL) treats the borrow as dead after its last use, so `Drop(p)` routinely
//! runs while `b` is still textually live. Lowering `Drop(p)` to a bare `!p`
//! then traps BorrowConflict (corpus 5, sci 14, sla 21 at T17).
//!
//! Fix: end provably-dead borrowers just before the drop (`!b` lines, then
//! `!p`). A borrower `b` of dropped `p` at block D is endable iff ALL hold:
//! - single MIR definition, in a block that dominates D (bound on every path);
//! - no use of `b` on any path from D (excluding D's own statements, which
//!   precede the terminator);
//! - `b` is itself never borrowed (no reborrow chains) and never MIR-dropped
//!   (no double free), and carries no version-conflict marker;
//! - the defining block cannot run again after D (no loop-carried rebind:
//!   def block unreachable from D);
//! - D is reachable from the entry (cleanup-path duplicates have invisible
//!   unwind edges; unconditional ends there are unsound — they stay loud).
//! A site is fixed only when EVERY borrower passes (a leftover live borrow
//! would still trap); otherwise the site keeps its old shape and is counted
//! loud (`DropBorrowLive`) — never a new trap, never silent.
//!
//! Phase 2 — cleanup dedup. rustc's unwind cleanup path repeats every drop
//! of the normal path; those blocks are unreachable from the entry (the
//! driver emits no unwind edges) and end in `panic("unwind-resume")`.
//! Runtime never executes them (panic aborts; `sa check` probes: panic path
//! skips leak checks, `fu` shows a fresh def after `return` is fine), yet SA
//!'s Referee keeps scanning past `return` and sees the duplicate `!p` as a
//! second release (UseAfterMove). So an UNREACHABLE Drop site emits no `!p`
//! at all (`cleanup` set below): the main path already freed it, and the
//! cleanup path owns nothing to free. Sound because unwinding is not modelled
//! in phase 1 (`Resume` already lowers to `panic`).
//!
//! Phase 3 - merge ends (join, non-exit). A two-deep borrow chain
//! (`m = &p`, `l = &m`) whose every use dies inside its defining block B
//! still locks `p` textually on outgoing paths from B, so a join block fed
//! both by B and by a bypass path sees `p` as Locked_Read on one side
//! and Active on the other (PhiStateConflict, corpus 313). The Drop-site
//! plan above cannot fix this: there is no Drop of `p` in the function,
//! and the old chain gate vetoes twice over - `m` is itself borrowed
//! (by `l`) and `l` is a reborrow - while exit-anchored ordering
//! (drop.rs) only runs at Return blocks, never at a join. The fix is
//! narrow: when the chain is exactly two deep, single-defined in B,
//! never used outside B (nor by the terminator of B), never MIR-dropped,
//! unconflicted, with no other borrower of `p` or of `m`, and B ends in
//! a branch to a non-exit join (a successor with another predecessor),
//! emit `!l` then `!m` at the end of B, before its terminator. Leaf
//! first is the only sound order (freeing `m` while `l` locks it would
//! trap). Anything outside this shape is left untouched - no loud mark,
//! no new trap, the file keeps its previous class.
//!
//! Operates on the versioned MIR (same input lower and coverage share, so
//! parity holds by construction). Text-level passes (drop.rs) see the
//! inserted `!b` lines as ordinary frees and skip `b` at exits.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use crate::mir::{Block, Operand, Rvalue, Stmt, Term};

/// Drop-block index -> borrower regs to free first (sorted, independent).
/// Drop-block index -> live borrow we cannot provably end (loud).
/// Drop-block index -> release omitted (unreachable cleanup duplicate).
/// Block index -> double-borrow chain regs to free at the block end,
/// leaf first, before a branch into a join (phase 3).
pub struct DropPlan {
    pub ends: BTreeMap<usize, Vec<String>>,
    pub loud: BTreeSet<usize>,
    pub cleanup: BTreeSet<usize>,
    pub merge_ends: BTreeMap<usize, Vec<String>>,
}

/// Successor table (same edges as order.rs: explicit terminator targets;
// unwind/cleanup edges are invisible to the driver, hence unreachable here).
fn block_succ(t: &Term) -> Vec<&str> {
    match t {
        Term::Goto { target } => vec![target],
        Term::Call { target, .. } => target.iter().map(|s| s.as_str()).collect(),
        Term::Drop { target, .. } => vec![target],
        Term::SwitchInt { targets, otherwise, .. } => {
            let mut v: Vec<&str> = targets.iter().map(|(_, t)| t.as_str()).collect();
            v.push(otherwise);
            v
        }
        Term::Assert { target, .. } => vec![target],
        Term::InlineAsm { target, .. } => target.iter().map(|s| s.as_str()).collect(),
        _ => vec![],
    }
}

fn successors(blocks: &[Block]) -> Vec<Vec<usize>> {
    let index: HashMap<&str, usize> =
        blocks.iter().enumerate().map(|(i, b)| (b.id.as_str(), i)).collect();
    blocks
        .iter()
        .map(|b| {
            block_succ(&b.terminator)
                .into_iter()
                .filter_map(|s| index.get(s).copied())
                .collect()
        })
        .collect()
}

/// Blocks reachable from `from`, excluding `from` itself.
fn reachable_from(succ: &[Vec<usize>], from: usize) -> HashSet<usize> {
    let mut seen = HashSet::from([from]);
    let mut stack = vec![from];
    while let Some(i) = stack.pop() {
        for &j in &succ[i] {
            if seen.insert(j) {
                stack.push(j);
            }
        }
    }
    seen.remove(&from);
    seen
}

fn operand_place(op: &Operand) -> Option<&str> {
    match op {
        Operand::Move { place } | Operand::Copy { place } | Operand::Conflict { place } => {
            Some(place.as_str())
        }
        _ => None,
    }
}

/// Places a terminator reads (its own block terminator must not read a
/// merge-ended chain: the ends are emitted before the terminator renders,
/// so a terminator use would read a freed register).
fn term_uses(t: &Term, name: &str) -> bool {
    match t {
        Term::Call { args, .. } => args.iter().any(|a| operand_place(a) == Some(name)),
        Term::Drop { place, .. } => place == name,
        Term::SwitchInt { discr, .. } => operand_place(discr) == Some(name),
        Term::Assert { cond, .. } => operand_place(cond) == Some(name),
        Term::Return { ret } => ret.as_deref() == Some(name),
        Term::InlineAsm { ins, .. } => ins.iter().any(|a| operand_place(a) == Some(name)),
        _ => false,
    }
}

/// Plan phase-3 merge ends (see module docs). Returns block index ->
/// chain regs in free order (leaf first). Pure MIR-level.
#[allow(clippy::too_many_arguments)]
fn plan_merge_ends(
    blocks: &[Block],
    succ: &[Vec<usize>],
    edges: &[(String, String, usize)],
    defs: &BTreeMap<String, Vec<usize>>,
    uses: &BTreeMap<String, BTreeSet<usize>>,
    drop_at: &BTreeMap<String, Vec<usize>>,
    conflicted: &BTreeSet<String>,
    entry_reach: &HashSet<usize>,
) -> BTreeMap<usize, Vec<String>> {
    // Predecessor sets over the explicit CFG edges.
    let mut preds: Vec<BTreeSet<usize>> = vec![BTreeSet::new(); blocks.len()];
    for (i, ss) in succ.iter().enumerate() {
        for &s in ss {
            preds[s].insert(i);
        }
    }
    let borrowers_of = |src: &str| -> BTreeSet<String> {
        edges.iter().filter(|(_, s, _)| s == src).map(|(b, _, _)| b.clone()).collect()
    };
    let mut out: BTreeMap<usize, Vec<String>> = BTreeMap::new();
    for bi in 0..blocks.len() {
        if !entry_reach.contains(&bi) {
            continue;
        }
        // Only branch terminators can feed a join; Drop/Return/Call/
        // Resume/Unreachable blocks are handled elsewhere (or never).
        if !matches!(
            blocks[bi].terminator,
            Term::SwitchInt { .. } | Term::Goto { .. } | Term::Assert { .. }
        ) {
            continue;
        }
        // No loop-back: the defining block must not run again after the
        // branch (a second execution would rebind single-assignment regs).
        if succ[bi].contains(&bi) {
            continue;
        }
        if succ[bi].iter().any(|s| reachable_from(succ, *s).contains(&bi)) {
            continue;
        }
        // At least one successor is a non-exit join: another predecessor
        // besides this block, and not itself a Return block.
        let has_join = succ[bi].iter().any(|&s| {
            s != bi
                && preds[s].len() >= 2
                && preds[s].iter().any(|&pr| pr != bi)
                && !matches!(blocks[s].terminator, Term::Return { .. })
        });
        if !has_join {
            continue;
        }
        let mut ends: Vec<String> = vec![];
        for (m, p, db) in edges {
            if *db != bi {
                continue;
            }
            for (l, m2, db2) in edges {
                if *db2 != bi || m2 != m {
                    continue;
                }
                // Exactly two deep, all distinct, root is not itself a
                // borrower (a longer chain is a different shape).
                if p == m || m == l || p == l {
                    continue;
                }
                if edges.iter().any(|(b, _, _)| b == p) {
                    continue;
                }
                // Single definition of each chain link, in this block.
                let single_in = |r: &str| {
                    defs.get(r).is_some_and(|v| v.len() == 1 && v[0] == bi)
                };
                if !single_in(m) || !single_in(l) {
                    continue;
                }
                // Exact borrower sets: no other borrower of the root or
                // of the intermediate, and nothing reborrows the leaf.
                if borrowers_of(p) != BTreeSet::from([m.clone()]) {
                    continue;
                }
                if borrowers_of(m) != BTreeSet::from([l.clone()]) {
                    continue;
                }
                if !borrowers_of(l).is_empty() {
                    continue;
                }
                // Dead outside this block (uses inside it precede the ends),
                // never used by this block own terminator, never dropped,
                // never version-conflicted.
                let dead_outside = |r: &str| {
                    uses.get(r).is_none_or(|us| us.iter().all(|u| *u == bi))
                };
                if !dead_outside(m) || !dead_outside(l) {
                    continue;
                }
                if term_uses(&blocks[bi].terminator, m) || term_uses(&blocks[bi].terminator, l) {
                    continue;
                }
                if drop_at.get(m).is_some_and(|v| !v.is_empty())
                    || drop_at.get(l).is_some_and(|v| !v.is_empty())
                {
                    continue;
                }
                if conflicted.contains(m) || conflicted.contains(l) {
                    continue;
                }
                // Defensive disjointness: two chains in one block must not
                // share a register (the gates above already imply this).
                if ends.contains(m) || ends.contains(l) {
                    continue;
                }
                // Leaf first: borrowers before sources.
                ends.push(l.clone());
                ends.push(m.clone());
            }
        }
        if !ends.is_empty() {
            out.insert(bi, ends);
        }
    }
    out
}

/// Plan borrow-ends for every MIR Drop site. Pure MIR-level (no SA text).
pub fn plan_drops(blocks: &[Block], doms: &[HashSet<usize>]) -> DropPlan {
    let n = blocks.len();
    let succ = successors(blocks);
    // Entry-reachable blocks (cleanup duplicates are not).
    let mut entry_reach = HashSet::from([0usize]);
    if n > 0 {
        entry_reach = reachable_from(&succ, 0);
        entry_reach.insert(0);
    }
    // Definitions per local (Assign + Call + asm-outs, mirroring version.rs).
    let mut defs: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    // Borrow edges (borrower, source, def block); ZST borrows emit no `&`.
    let mut edges: Vec<(String, String, usize)> = vec![];
    // Operand/place uses per local -> blocks.
    let mut uses: BTreeMap<String, BTreeSet<usize>> = BTreeMap::new();
    let mut used = |name: &str, bi: usize| {
        uses.entry(name.to_string()).or_default().insert(bi);
    };
    // Locals ever borrowed (reborrow chains veto early ends).
    let mut borrowed: BTreeSet<String> = BTreeSet::new();
    // Locals carrying a version-conflict marker anywhere.
    let mut conflicted: BTreeSet<String> = BTreeSet::new();
    // MIR Drop sites per local.
    let mut drop_at: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (bi, b) in blocks.iter().enumerate() {
        for st in &b.statements {
            if let Stmt::Assign { dest, rvalue, .. } = st {
                defs.entry(dest.clone()).or_default().push(bi);
                match rvalue {
                    Rvalue::Use { op } => {
                        if let Some(p) = operand_place(op) {
                            used(p, bi);
                            if matches!(op, Operand::Conflict { .. }) {
                                conflicted.insert(p.to_string());
                            }
                        }
                    }
                    Rvalue::Repeat { op, .. } => {
                        if let Some(p) = operand_place(op) {
                            used(p, bi);
                            if matches!(op.as_ref(), Operand::Conflict { .. }) {
                                conflicted.insert(p.to_string());
                            }
                        }
                    }
                    Rvalue::Call { args, .. } => {
                        for a in args {
                            if let Some(p) = operand_place(a) {
                                used(p, bi);
                                if matches!(a, Operand::Conflict { .. }) {
                                    conflicted.insert(p.to_string());
                                }
                            }
                        }
                    }
                    Rvalue::BinOp { left, right, .. } => {
                        for o in [left.as_ref(), right.as_ref()] {
                            if let Some(p) = operand_place(o) {
                                used(p, bi);
                                if matches!(o, Operand::Conflict { .. }) {
                                    conflicted.insert(p.to_string());
                                }
                            }
                        }
                    }
                    Rvalue::UnOp { operand, .. } | Rvalue::Cast { op: operand, .. } => {
                        if let Some(p) = operand_place(operand) {
                            used(p, bi);
                            if matches!(operand.as_ref(), Operand::Conflict { .. }) {
                                conflicted.insert(p.to_string());
                            }
                        }
                    }
                    Rvalue::Aggregate { elems, .. } => {
                        for e in elems {
                            if let Some(p) = operand_place(e) {
                                used(p, bi);
                                if matches!(e, Operand::Conflict { .. }) {
                                    conflicted.insert(p.to_string());
                                }
                            }
                        }
                    }
                    Rvalue::Ref { place, zst, .. } => {
                        used(place, bi);
                        borrowed.insert(place.clone());
                        if !zst {
                            edges.push((dest.clone(), place.clone(), bi));
                        }
                    }
                    Rvalue::Discriminant { place } | Rvalue::RawPtr { place, .. } => {
                        used(place, bi);
                    }
                    Rvalue::ThreadLocal { .. } | Rvalue::Unsupported { .. } => {}
                }
            } else if let Stmt::SetDisc { place, .. } = st {
                used(place, bi);
            }
        }
        match &b.terminator {
            Term::Call { args, dest, .. } => {
                for a in args {
                    if let Some(p) = operand_place(a) {
                        used(p, bi);
                        if matches!(a, Operand::Conflict { .. }) {
                            conflicted.insert(p.to_string());
                        }
                    }
                }
                if let Some(d) = dest {
                    defs.entry(d.clone()).or_default().push(bi);
                }
            }
            Term::Drop { place, .. } => {
                used(place, bi);
                if place != "__VERSION_CONFLICT__" {
                    drop_at.entry(place.clone()).or_default().push(bi);
                }
            }
            Term::SwitchInt { discr, .. } => {
                if let Some(p) = operand_place(discr) {
                    used(p, bi);
                    if matches!(discr.as_ref(), Operand::Conflict { .. }) {
                        conflicted.insert(p.to_string());
                    }
                }
            }
            Term::Assert { cond, .. } => {
                if let Some(p) = operand_place(cond) {
                    used(p, bi);
                    if matches!(cond.as_ref(), Operand::Conflict { .. }) {
                        conflicted.insert(p.to_string());
                    }
                }
            }
            Term::InlineAsm { outs, ins, .. } => {
                for o in outs {
                    defs.entry(o.clone()).or_default().push(bi);
                }
                for a in ins {
                    if let Some(p) = operand_place(a) {
                        used(p, bi);
                        if matches!(a, Operand::Conflict { .. }) {
                            conflicted.insert(p.to_string());
                        }
                    }
                }
            }
            _ => {}
        }
    }
    // Merge ends (phase 3) are computed from the same scan; a borrower
    // they release needs no second end at a downstream Drop (see below).
    let merge_ends = plan_merge_ends(blocks, &succ, &edges, &defs, &uses, &drop_at, &conflicted, &entry_reach);
    let merged: BTreeSet<String> = merge_ends.values().flatten().cloned().collect();
    let mut plan = DropPlan { ends: BTreeMap::new(), loud: BTreeSet::new(), cleanup: BTreeSet::new(), merge_ends };
    // Borrowers already ended (per borrower, at most one end per CFG path:
    // a second end downstream would double-free).
    let mut ended_at: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    // RPO-ish order: iterate Drop blocks in index order (emission is RPO,
    // but planning only needs determinism; reachability decides soundness).
    let mut drop_blocks: Vec<usize> = (0..n)
        .filter(|bi| matches!(blocks[*bi].terminator, Term::Drop { .. }))
        .collect();
    drop_blocks.sort();
    for di in drop_blocks {
        let place = match &blocks[di].terminator {
            Term::Drop { place, .. } => place.clone(),
            _ => continue,
        };
        if place == "__VERSION_CONFLICT__" {
            continue;
        }
        // Cleanup dedup (phase 2) first: the release is a duplicate of a
        // reachable path, so nothing is freed here. Detected before the
        // borrow gates because an unreachable site never executes a borrow
        // end either.
        if !entry_reach.contains(&di) {
            plan.cleanup.insert(di);
            continue;
        }
        // Borrowers of this place (self-borrows veto the whole site).
        let mut borrowers: BTreeSet<String> = BTreeSet::new();
        let mut self_borrow = false;
        for (b, s, _) in &edges {
            if s == &place {
                if b == &place {
                    self_borrow = true;
                } else {
                    borrowers.insert(b.clone());
                }
            }
        }
        // Phase-3 merge ends already released these borrowers on every
        // path through their defining block, and single-def means they
        // are never defined off that path - so a downstream Drop needs
        // neither a second end nor a loud mark for them.
        borrowers.retain(|b| !merged.contains(b));
        if borrowers.is_empty() || self_borrow {
            if self_borrow {
                plan.loud.insert(di);
            }
            continue;
        }
        let after = reachable_from(&succ, di);
        let mut ok = true;
        for b in &borrowers {
            let single = defs.get(b).is_some_and(|v| v.len() == 1);
            let db = defs.get(b).and_then(|v| v.first().copied());
            let dominated = db.is_some_and(|d| doms[di].contains(&d));
            let no_loopback = db.is_some_and(|d| d == di || !after.contains(&d));
            let dead_after = uses.get(b).is_none_or(|us| us.iter().all(|u| !after.contains(u)));
            let no_chain = !borrowed.contains(b);
            let no_drop = drop_at.get(b).is_none_or(|v| v.is_empty());
            let no_conflict = !conflicted.contains(b);
            // One end per path: skip sites downstream of an existing end.
            let shadowed = ended_at.get(b).is_some_and(|sites| sites.iter().any(|s| after.contains(s) || *s == di));
            let upstream_ended = ended_at.get(b).is_some_and(|sites| {
                sites.iter().any(|s| {
                    // An earlier end that reaches di already ended b here.
                    reachable_from(&succ, *s).contains(&di)
                })
            });
            if !(single && dominated && no_loopback && dead_after && no_chain && no_drop && no_conflict)
                || shadowed
                || upstream_ended
            {
                ok = false;
                break;
            }
        }
        if ok {
            let v: Vec<String> = borrowers.into_iter().collect();
            for b in &v {
                ended_at.entry(b.clone()).or_default().push(di);
            }
            plan.ends.insert(di, v);
        } else {
            plan.loud.insert(di);
        }
    }
    // Phase-2 note: a site downstream of an end needs no loud mark when the
    // end dominates it (every path already ended the borrow); dominated
    // downstream sites cannot exist without the borrower also being endable,
    // so no post-pass adjustment is needed.
    plan
}

#[cfg(test)]
mod borrow_end_tests {
    use super::*;
    use crate::mir::Function;

    fn blk(id: &str, stmts: Vec<Stmt>, term: Term) -> Block {
        Block { id: id.to_string(), statements: stmts, terminator: term }
    }

    fn rf(dest: &str, place: &str) -> Stmt {
        Stmt::Assign {
            dest: dest.to_string(),
            dest_place: None,
            rvalue: Rvalue::Ref { place: place.to_string(), mut_: false, via: None, zst: false },
        }
    }

    fn doms(blocks: &[Block]) -> Vec<HashSet<usize>> {
        crate::order::dom_sets(blocks)
    }

    fn lin() -> Vec<Block> {
        // bb0: _2 = &_1; call _3 = f(_2) -> bb1; bb1: Drop _1 -> bb2; bb2: Return.
        vec![
            blk("bb0", vec![rf("_2", "_1")], Term::Call {
                func: "f".to_string(), func_raw: None,
                args: vec![Operand::Move { place: "_2".to_string() }],
                dest: Some("_3".to_string()), target: Some("bb1".to_string()),
                sig: None,
            }),
            blk("bb1", vec![], Term::Drop { place: "_1".to_string(), target: "bb2".to_string() }),
            blk("bb2", vec![], Term::Return { ret: None }),
        ]
    }

    #[test]
    fn linear_borrow_ended() {
        let blocks = lin();
        let plan = plan_drops(&blocks, &doms(&blocks));
        assert_eq!(plan.ends.get(&1), Some(&vec!["_2".to_string()]));
        assert!(plan.loud.is_empty());
    }

    #[test]
    fn used_after_vetoes() {
        // Borrower reused after the drop: ending it would move-into-trap.
        let mut blocks = lin();
        blocks.push(blk("bb3", vec![], Term::Return { ret: None }));
        if let Term::Drop { target, .. } = &mut blocks[1].terminator {
            *target = "bb3".to_string();
        }
        blocks[2] = blk("bb2", vec![], Term::Goto { target: "bb3".to_string() });
        // Use _2 in bb3 via a call arg.
        blocks[3] = blk("bb3", vec![], Term::Call {
            func: "g".to_string(), func_raw: None,
            args: vec![Operand::Copy { place: "_2".to_string() }],
            dest: None, target: None, sig: None,
        });
        let plan = plan_drops(&blocks, &doms(&blocks));
        assert!(!plan.ends.contains_key(&1));
        assert!(plan.loud.contains(&1));
    }

    #[test]
    fn multidef_vetoes() {
        // Borrower redefined later: single-def gate refuses.
        let mut blocks = lin();
        blocks[2] = blk("bb2", vec![rf("_2", "_1")], Term::Return { ret: None });
        let plan = plan_drops(&blocks, &doms(&blocks));
        assert!(!plan.ends.contains_key(&1));
        assert!(plan.loud.contains(&1));
    }

    #[test]
    fn unreachable_drop_stays_loud() {
        // Cleanup duplicate unreachable from entry: no `!b`, no `!p` (phase 2).
        let mut blocks = lin();
        blocks.push(blk("bb9", vec![], Term::Drop { place: "_1".to_string(), target: "bb2".to_string() }));
        let plan = plan_drops(&blocks, &doms(&blocks));
        assert_eq!(plan.ends.get(&1), Some(&vec!["_2".to_string()]));
        assert!(!plan.ends.contains_key(&3));
        assert!(plan.cleanup.contains(&3), "unreachable drop deduped");
        assert!(!plan.loud.contains(&3), "dedup is not a gap");
        assert!(plan.loud.is_empty());
    }

    #[test]
    fn cleanup_without_borrow_also_deduped() {
        // No borrowers -> phase 1 skips the site, but the release is still a
        // duplicate of the main path, so phase 2 must omit it.
        let mut blocks = vec![
            blk("bb0", vec![], Term::Drop { place: "_1".to_string(), target: "bb1".to_string() }),
            blk("bb1", vec![], Term::Return { ret: None }),
            blk("bb9", vec![], Term::Drop { place: "_1".to_string(), target: "bb1".to_string() }),
        ];
        let plan = plan_drops(&blocks, &doms(&blocks));
        assert!(plan.ends.is_empty());
        assert_eq!(plan.cleanup.iter().copied().collect::<Vec<_>>(), vec![2]);
        assert!(plan.loud.is_empty());
    }

    #[test]
    fn no_borrow_no_plan() {
        let blocks = vec![
            blk("bb0", vec![], Term::Drop { place: "_1".to_string(), target: "bb1".to_string() }),
            blk("bb1", vec![], Term::Return { ret: None }),
        ];
        let plan = plan_drops(&blocks, &doms(&blocks));
        assert!(plan.ends.is_empty());
        assert!(plan.loud.is_empty());
    }

    #[test]
    fn zst_borrow_ignored() {
        // ZST borrows emit `= 0` (no `&`, no lock): no plan, no loud.
        let blocks = vec![
            blk("bb0", vec![Stmt::Assign {
                dest: "_2".to_string(), dest_place: None,
                rvalue: Rvalue::Ref { place: "_1".to_string(), mut_: false, via: None, zst: true },
            }], Term::Goto { target: "bb1".to_string() }),
            blk("bb1", vec![], Term::Drop { place: "_1".to_string(), target: "bb2".to_string() }),
            blk("bb2", vec![], Term::Return { ret: None }),
        ];
        let plan = plan_drops(&blocks, &doms(&blocks));
        assert!(plan.ends.is_empty());
        assert!(plan.loud.is_empty());
    }

    fn use_copy(dest: &str, place: &str) -> Stmt {
        Stmt::Assign {
            dest: dest.to_string(), dest_place: None,
            rvalue: Rvalue::Use { op: Operand::Copy { place: place.to_string() } },
        }
    }

    fn const_bool(dest: &str, v: &str) -> Stmt {
        Stmt::Assign {
            dest: dest.to_string(), dest_place: None,
            rvalue: Rvalue::Use { op: Operand::Const { value: v.to_string(), str_bytes: None, str_len: None } },
        }
    }

    fn switch(discr: &str, target: &str, otherwise: &str) -> Term {
        Term::SwitchInt {
            discr: Box::new(Operand::Move { place: discr.to_string() }),
            targets: vec![("0".to_string(), target.to_string())],
            otherwise: otherwise.to_string(),
        }
    }

    // Corpus 313 shape: bb0 branches to bb2 (Active arm) or bb1 (join);
    // bb2 builds _3 = &_1, _4 = &_3, uses both only inside bb2, then
    // branches to bb1 (join) or bb3. The join bb1 must see _1 unlocked.
    fn chain313() -> Vec<Block> {
        vec![
            blk("bb0", vec![Stmt::Assign {
                dest: "_2".to_string(), dest_place: None,
                rvalue: Rvalue::Discriminant { place: "_1".to_string() },
            }], switch("_2", "bb2", "bb1")),
            blk("bb1", vec![const_bool("_0", "Val(Scalar(0x00), bool)")],
                Term::Goto { target: "bb4".to_string() }),
            blk("bb2", vec![
                rf("_3", "_1"),
                rf("_4", "_3"),
                use_copy("_7", "_4"),
                use_copy("_6", "_7"),
                Stmt::Assign {
                    dest: "_5".to_string(), dest_place: None,
                    rvalue: Rvalue::BinOp {
                        op: "Gt".to_string(),
                        left: Box::new(Operand::Move { place: "_6".to_string() }),
                        right: Box::new(Operand::Const { value: "Val(Scalar(0x00000000), i32)".to_string(), str_bytes: None, str_len: None }),
                    },
                },
            ], switch("_5", "bb1", "bb3")),
            blk("bb3", vec![const_bool("_0", "Val(Scalar(0x01), bool)")],
                Term::Goto { target: "bb4".to_string() }),
            blk("bb4", vec![], Term::Return { ret: None }),
        ]
    }

    #[test]
    fn double_chain_merge_ended_leaf_first() {
        let blocks = chain313();
        let plan = plan_drops(&blocks, &doms(&blocks));
        assert_eq!(
            plan.merge_ends.get(&2),
            Some(&vec!["_4".to_string(), "_3".to_string()]),
            "leaf _4 before intermediate _3"
        );
        assert!(plan.ends.is_empty(), "no Drop site involved");
        assert!(plan.loud.is_empty());
    }

    #[test]
    fn chain_used_after_merge_vetoes() {
        // _4 reused in bb3 (past the join): ending it in bb2 would trap.
        let mut blocks = chain313();
        blocks[3] = blk("bb3", vec![
            const_bool("_0", "Val(Scalar(0x01), bool)"),
            use_copy("_8", "_4"),
        ], Term::Goto { target: "bb4".to_string() });
        let plan = plan_drops(&blocks, &doms(&blocks));
        assert!(plan.merge_ends.is_empty(), "live past the join: no merge end");
    }

    #[test]
    fn single_borrow_no_merge_end() {
        // One-deep borrow at a join is not this shape: leave it alone.
        let mut blocks = chain313();
        blocks[2] = blk("bb2", vec![
            rf("_3", "_1"),
            const_bool("_5", "Val(Scalar(0x00), bool)"),
        ], switch("_5", "bb1", "bb3"));
        let plan = plan_drops(&blocks, &doms(&blocks));
        assert!(plan.merge_ends.is_empty(), "single borrow is out of scope");
    }

    #[test]
    fn terminator_use_vetoes() {
        // The branch discriminant itself reads _4: ends emitted before the
        // terminator would free the discriminant source.
        let mut blocks = chain313();
        blocks[2] = blk("bb2", vec![
            rf("_3", "_1"),
            rf("_4", "_3"),
            use_copy("_7", "_4"),
        ], switch("_4", "bb1", "bb3"));
        let plan = plan_drops(&blocks, &doms(&blocks));
        assert!(plan.merge_ends.is_empty(), "terminator use vetoes");
    }

    #[test]
    fn merge_end_suppresses_downstream_drop_loud() {
        // Same join, then a Drop of _1 after the join: the chain is
        // already ended in bb2, so the Drop needs no second end and no
        // loud mark (the old chain gate would have made it loud).
        let mut blocks = chain313();
        blocks[1] = blk("bb1", vec![const_bool("_0", "Val(Scalar(0x00), bool)")],
            Term::Goto { target: "bb4".to_string() });
        blocks[3] = blk("bb3", vec![const_bool("_0", "Val(Scalar(0x01), bool)")],
            Term::Goto { target: "bb4".to_string() });
        blocks[4] = blk("bb4", vec![], Term::Drop { place: "_1".to_string(), target: "bb5".to_string() });
        blocks.push(blk("bb5", vec![], Term::Return { ret: None }));
        let plan = plan_drops(&blocks, &doms(&blocks));
        assert_eq!(
            plan.merge_ends.get(&2),
            Some(&vec!["_4".to_string(), "_3".to_string()])
        );
        assert!(plan.ends.is_empty(), "no second end at the Drop");
        assert!(plan.loud.is_empty(), "merge-ended borrower is not loud");
        assert!(plan.cleanup.is_empty());
    }
}
