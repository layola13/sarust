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
//! Operates on the versioned MIR (same input lower and coverage share, so
//! parity holds by construction). Text-level passes (drop.rs) see the
//! inserted `!b` lines as ordinary frees and skip `b` at exits.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use crate::mir::{Block, Operand, Rvalue, Stmt, Term};

/// Drop-block index -> borrower regs to free first (sorted, independent).
/// Drop-block index -> live borrow we cannot provably end (loud).
pub struct DropPlan {
    pub ends: BTreeMap<usize, Vec<String>>,
    pub loud: BTreeSet<usize>,
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
    let mut plan = DropPlan { ends: BTreeMap::new(), loud: BTreeSet::new() };
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
        if borrowers.is_empty() || self_borrow {
            if self_borrow {
                plan.loud.insert(di);
            }
            continue;
        }
        if !entry_reach.contains(&di) {
            // Cleanup-path duplicate: unconditional ends unsound here.
            plan.loud.insert(di);
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
    // A site downstream of an end needs no loud mark when the end dominates
    // it (every path already ended the borrow); otherwise it stays loud.
    // (Ends dominate their site by construction only when the borrower def
    // dominates; downstream non-dominated sites were marked loud above, and
    // dominated ones cannot occur without the borrower also being endable —
    // so no post-pass adjustment is needed for phase 1.)
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
            blk("bb2", vec![], Term::Return),
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
        blocks.push(blk("bb3", vec![], Term::Return));
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
        blocks[2] = blk("bb2", vec![rf("_2", "_1")], Term::Return);
        let plan = plan_drops(&blocks, &doms(&blocks));
        assert!(!plan.ends.contains_key(&1));
        assert!(plan.loud.contains(&1));
    }

    #[test]
    fn unreachable_drop_stays_loud() {
        // Cleanup duplicate unreachable from entry: no unconditional end.
        let mut blocks = lin();
        blocks.push(blk("bb9", vec![], Term::Drop { place: "_1".to_string(), target: "bb2".to_string() }));
        let plan = plan_drops(&blocks, &doms(&blocks));
        assert_eq!(plan.ends.get(&1), Some(&vec!["_2".to_string()]));
        assert!(!plan.ends.contains_key(&3));
        assert!(plan.loud.contains(&3));
    }

    #[test]
    fn no_borrow_no_plan() {
        let blocks = vec![
            blk("bb0", vec![], Term::Drop { place: "_1".to_string(), target: "bb1".to_string() }),
            blk("bb1", vec![], Term::Return),
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
            blk("bb2", vec![], Term::Return),
        ];
        let plan = plan_drops(&blocks, &doms(&blocks));
        assert!(plan.ends.is_empty());
        assert!(plan.loud.is_empty());
    }
}
